//! __NAME__ on this node: one model at a time, loaded when the POM deploys it
//! here (`engine.load`) and stopped when the POM takes it away
//! (`engine.unload`). The contract is `docs/subsystems/inference.md` 7.1 of
//! the POM repository.
//!
//! One manager thread owns the engine's server process. `query` never waits
//! on it: commands go through a channel and the state is read from a shared
//! snapshot. The child starts from that long-lived thread in its own process
//! group with `stdin` closed and its output in `run/engine.log`; on Linux it
//! gets the parent-death signal, on macOS (no such signal) its PID is kept in
//! `run/engine.pid` and a stale one is stopped at the next start, so a killed
//! plugin host never strands GPU memory for long.

use crate::bridge::Bridge;
use crate::http;
use crate::launch::{self, Launch};
use serde_json::{json, Map, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_secs(2);
/// How long the engine may take to stop before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(30);
const DEFAULT_CONTEXT: u32 = 8192;

/// What the POM sends in `engine.load` (`plugin_engine::load_params`).
#[derive(Debug, Clone, PartialEq)]
pub struct LoadRequest {
    pub model_path: PathBuf,
    /// The name the network knows the model by.
    pub model_label: String,
    pub context: u32,
    /// The node's NVIDIA GPU indices the deployment chose; empty = the
    /// engine's default device (Apple silicon, CPU, or GPU 0).
    pub gpus: Vec<u32>,
    /// Every GPU of the machine: `{index, vendor, ordinal, name, vram_total_mb}`.
    pub gpu_devices: Vec<Value>,
    /// Concurrent requests (the engine's batch slots).
    pub sessions: u32,
    /// The node's KV cache type (`f16`, `q8_0`, `q4_0`, ...).
    pub kv: String,
    /// The engine settings the deployment chose (`crate::settings`).
    pub settings: Map<String, Value>,
    /// One stage of a model split across machines (`{}` for the first).
    pub link: Option<Value>,
    /// Where conversation sessions are kept (the node's KV snapshot folder).
    pub session_dir: Option<PathBuf>,
    /// The vision projector the deployment chose.
    pub mmproj_path: Option<PathBuf>,
    /// `prefill` or `decode`; `None` = both.
    pub role: Option<String>,
    pub prefill_min_tokens: Option<u32>,
}

fn absolute(value: &Value, name: &str) -> Result<Option<PathBuf>, String> {
    match value.as_str().filter(|text| !text.is_empty()) {
        None => Ok(None),
        Some(text) if Path::new(text).is_absolute() => Ok(Some(PathBuf::from(text))),
        Some(_) => Err(format!("{name} must be absolute")),
    }
}

impl LoadRequest {
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let model_path =
            absolute(&value["model_path"], "model_path")?.ok_or("model_path is missing")?;
        let model_label = value["model_label"]
            .as_str()
            .filter(|label| !label.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                model_path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| crate::PLUGIN_CODE.into());
        let role = match value["role"].as_str() {
            None | Some("both") => None,
            Some(role @ ("prefill" | "decode")) => Some(role.to_owned()),
            Some(other) => return Err(format!("unknown inference role {other}")),
        };
        Ok(Self {
            model_path,
            model_label,
            context: value["n_ctx"]
                .as_u64()
                .filter(|context| *context >= 512)
                .map(|context| context.min(1 << 21) as u32)
                .unwrap_or(DEFAULT_CONTEXT),
            gpus: value["gpus"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_u64)
                        .filter(|gpu| *gpu < 64)
                        .map(|gpu| gpu as u32)
                        .collect()
                })
                .unwrap_or_default(),
            gpu_devices: value["gpu_devices"].as_array().cloned().unwrap_or_default(),
            sessions: value["max_sessions"]
                .as_u64()
                .map(|count| count.clamp(1, 64) as u32)
                .unwrap_or(1),
            kv: value["kv"].as_str().unwrap_or("f16").to_owned(),
            settings: crate::settings::select(&value["settings"]),
            link: value.get("link").filter(|link| link.is_object()).cloned(),
            session_dir: absolute(&value["session_dir"], "session_dir")?,
            mmproj_path: absolute(&value["mmproj_path"], "mmproj_path")?,
            role,
            prefill_min_tokens: value["prefill_min_tokens"]
                .as_u64()
                .map(|tokens| tokens.min(u64::from(u32::MAX)) as u32),
        })
    }

    /// What this engine refuses before it starts anything.
    pub fn check(&self) -> Result<(), String> {
        if !self.model_path.is_file() {
            return Err(format!(
                "the model {} is not on this machine",
                self.model_path.display()
            ));
        }
        if self.link.is_some() && !launch::STAGES {
            return Err("this engine does not run a slice of a model split across machines".into());
        }
        if self.role.is_some() {
            return Err("this engine has no prefill/decode split".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Loading,
    Ready,
    Stopping,
    Error,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Loading => "loading",
            Phase::Ready => "ready",
            Phase::Stopping => "stopping",
            Phase::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub phase: Phase,
    pub detail: Option<String>,
    pub runtime: Option<PathBuf>,
    pub runtime_version: Option<String>,
    pub request: Option<LoadRequest>,
    /// The engine server's loopback port and the model name it serves.
    pub engine: Option<(u16, String)>,
    pub since: Instant,
}

impl Snapshot {
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            detail: None,
            runtime: None,
            runtime_version: None,
            request: None,
            engine: None,
            since: Instant::now(),
        }
    }

    /// For the POM (`engine.status`, read every 2 s) and the screen: never a
    /// secret, never a prompt.
    pub fn status_json(&self, in_flight: u32) -> Value {
        json!({
            "state": self.phase.as_str(),
            "detail": self.detail,
            "model": self.request.as_ref().map(|request| request.model_label.clone()),
            "model_path": self.request.as_ref().map(|request| request.model_path.display().to_string()),
            "gpus": self.request.as_ref().map(|request| request.gpus.clone()),
            "n_ctx": self.request.as_ref().map(|request| request.context),
            "busy": in_flight > 0,
            "in_flight": in_flight,
            "runtime": {"ready": self.runtime.is_some(), "version": self.runtime_version},
            "seconds_in_state": self.since.elapsed().as_secs(),
        })
    }

    /// The service the POM draws next to this machine (`host.services`).
    pub fn service(&self) -> Value {
        let (status, detail) = match (self.phase, &self.runtime) {
            (Phase::Error, _) => ("error", self.detail.clone()),
            (_, None) => (
                "waiting",
                Some(format!("no {} runtime on this machine", crate::PLUGIN_CODE)),
            ),
            (Phase::Loading, _) => (
                "starting",
                self.request
                    .as_ref()
                    .map(|request| request.model_label.clone()),
            ),
            (Phase::Ready, _) => (
                "ready",
                self.request
                    .as_ref()
                    .map(|request| request.model_label.clone()),
            ),
            _ => ("ready", Some("no model loaded".into())),
        };
        let mut service = json!({
            "id": crate::PLUGIN_CODE,
            "kind": "inference",
            "name": "__NAME__",
            "status": status,
            "version": self.runtime_version,
            "detail": detail.map(|text| text.chars().take(190).collect::<String>()),
            "architectures": launch::ARCHITECTURES,
            "settings": crate::settings::schema(),
        });
        if launch::STAGES {
            service["stages"] = json!(true);
        }
        if launch::VISION {
            service["vision"] = json!(true);
        }
        if !launch::POM_SETTINGS.is_empty() {
            service["pom_settings"] = json!(launch::POM_SETTINGS);
        }
        service
    }
}

enum Command_ {
    Load(Box<LoadRequest>),
    Unload,
    Stop,
}

pub struct Engine {
    shared: Arc<Mutex<Snapshot>>,
    commands: Sender<Command_>,
    manager: Option<JoinHandle<()>>,
    bridge: Option<Bridge>,
    log: PathBuf,
}

impl Engine {
    pub fn start(base: PathBuf) -> Result<Self, String> {
        let run = base.join("run");
        fs::create_dir_all(&run).map_err(|error| format!("{}: {error}", run.display()))?;
        let shared = Arc::new(Mutex::new(Snapshot::new()));
        let reader = Arc::clone(&shared);
        let bridge = Bridge::start(Arc::new(move || {
            let snapshot = reader.lock().unwrap_or_else(PoisonError::into_inner);
            (snapshot.phase == Phase::Ready)
                .then(|| snapshot.engine.clone())
                .flatten()
        }))?;
        let (commands, receiver) = channel();
        let manager = Manager {
            base,
            run: run.clone(),
            shared: Arc::clone(&shared),
            commands: receiver,
            child: None,
            deadline: None,
            health: None,
        };
        let manager = thread::Builder::new()
            .name(format!("pom-plugin-{}-engine", crate::PLUGIN_CODE))
            .spawn(move || manager.run())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            shared,
            commands,
            manager: Some(manager),
            bridge: Some(bridge),
            log: run.join("engine.log"),
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn in_flight(&self) -> u32 {
        self.bridge
            .as_ref()
            .map_or(0, |bridge| bridge.in_flight.load(Ordering::SeqCst))
    }

    pub fn status_json(&self) -> Value {
        self.snapshot().status_json(self.in_flight())
    }

    /// `engine.upstream`: the bridge, only while the model is ready.
    pub fn upstream_json(&self) -> Value {
        let snapshot = self.snapshot();
        match (snapshot.phase, &self.bridge) {
            (Phase::Ready, Some(bridge)) => {
                json!({"status": "ready", "port": bridge.port, "token": bridge.token})
            }
            (Phase::Error, _) => json!({"status": "error", "error": snapshot.detail}),
            _ => json!({"status": "starting"}),
        }
    }

    /// `engine.link`: an earlier stage's port and secret (feature L6).
    pub fn link_json(&self) -> Value {
        json!({"status": "error", "error": "this engine does not run stages"})
    }

    pub fn load(&self, request: LoadRequest) -> Result<(), String> {
        request.check()?;
        self.commands
            .send(Command_::Load(Box::new(request)))
            .map_err(|_| "the engine manager stopped".to_owned())
    }

    pub fn unload(&self) {
        let _ = self.commands.send(Command_::Unload);
    }

    /// The engine's own output, for the administrator's screen.
    pub fn log_tail(&self, lines: usize) -> Vec<String> {
        let Ok(file) = File::open(&self.log) else {
            return Vec::new();
        };
        let all: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();
        all[all.len().saturating_sub(lines)..].to_vec()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.bridge = None;
        let _ = self.commands.send(Command_::Stop);
        if let Some(manager) = self.manager.take() {
            let _ = manager.join();
        }
    }
}

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}
#[cfg(target_os = "linux")]
extern "C" {
    fn prctl(option: i32, ...) -> i32;
}
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// The engine's server: its own process group, so a stop reaches every
/// process it started.
struct Group {
    child: Child,
    pid_file: PathBuf,
}

impl Group {
    fn spawn(launch: &Launch, run: &Path) -> Result<Self, String> {
        let pid_file = run.join("engine.pid");
        stop_stale(&pid_file, &launch.program);
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(run.join("engine.log"))
            .map_err(|error| error.to_string())?;
        let errors = log.try_clone().map_err(|error| error.to_string())?;
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.args)
            .current_dir(run)
            .env_clear()
            .envs(
                ["PATH", "HOME"]
                    .iter()
                    .filter_map(|key| std::env::var(key).ok().map(|value| (*key, value))),
            )
            .envs(
                launch
                    .env
                    .iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
            // The plugin host's stdin/stdout are its IPC with the node.
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errors));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            #[cfg(target_os = "linux")]
            unsafe {
                command.pre_exec(|| {
                    // PR_SET_PDEATHSIG: the child dies with the thread that
                    // spawned it, so spawn only from the manager thread.
                    if prctl(1, SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        return Err("this engine plugin runs on Linux and macOS".into());
        #[cfg(unix)]
        {
            let child = command
                .spawn()
                .map_err(|error| format!("{}: {error}", launch.program.display()))?;
            let _ = fs::write(
                &pid_file,
                format!("{}\n{}\n", child.id(), launch.program.display()),
            );
            Ok(Self { child, pid_file })
        }
    }

    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    fn stop(mut self, grace: Duration) {
        #[cfg(unix)]
        unsafe {
            kill(-(self.child.id() as i32), SIGTERM);
        }
        let until = Instant::now() + grace;
        while Instant::now() < until {
            if self.exited().is_some() {
                let _ = fs::remove_file(&self.pid_file);
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
        #[cfg(unix)]
        unsafe {
            kill(-(self.child.id() as i32), SIGKILL);
        }
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.pid_file);
    }
}

/// Stops an engine group left by a plugin host that was killed: the saved
/// PID, and only when that process still runs the engine's program.
fn stop_stale(pid_file: &Path, program: &Path) {
    let Ok(text) = fs::read_to_string(pid_file) else {
        return;
    };
    let _ = fs::remove_file(pid_file);
    let Some(pid) = text
        .lines()
        .next()
        .and_then(|line| line.trim().parse::<i32>().ok())
    else {
        return;
    };
    let running = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout).contains(&program.display().to_string())
        })
        .unwrap_or(false);
    #[cfg(unix)]
    if running && pid > 1 {
        unsafe {
            kill(-pid, SIGKILL);
        }
    }
}

struct Manager {
    base: PathBuf,
    run: PathBuf,
    shared: Arc<Mutex<Snapshot>>,
    commands: Receiver<Command_>,
    child: Option<Group>,
    deadline: Option<Instant>,
    /// The engine's port and health path while it loads.
    health: Option<(u16, String)>,
}

impl Manager {
    fn update(&self, change: impl FnOnce(&mut Snapshot)) {
        change(&mut self.shared.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn set_phase(&self, phase: Phase, detail: Option<String>) {
        self.update(|snapshot| {
            if snapshot.phase != phase {
                snapshot.since = Instant::now();
            }
            snapshot.phase = phase;
            snapshot.detail = detail;
        });
    }

    fn append_log(&self, line: &str) {
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.run.join("engine.log"))
        {
            let _ = writeln!(file, "[pom-plugin-{}] {line}", crate::PLUGIN_CODE);
        }
    }

    fn find_runtime(&self) -> Option<PathBuf> {
        let runtime = launch::find_runtime(&self.base);
        let version = runtime.as_deref().and_then(launch::runtime_version);
        self.update(|snapshot| {
            snapshot.runtime = runtime.clone();
            snapshot.runtime_version = version;
        });
        runtime
    }

    fn run(mut self) {
        self.find_runtime();
        loop {
            match self.commands.recv_timeout(POLL) {
                Ok(Command_::Load(request)) => self.load(*request),
                Ok(Command_::Unload) => self.unload(Phase::Idle, None),
                Ok(Command_::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    self.unload(Phase::Idle, None);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.poll();
        }
    }

    fn unload(&mut self, phase: Phase, detail: Option<String>) {
        if let Some(child) = self.child.take() {
            self.set_phase(Phase::Stopping, None);
            child.stop(STOP_GRACE);
        }
        self.health = None;
        self.deadline = None;
        self.update(|snapshot| {
            snapshot.engine = None;
            if phase == Phase::Idle {
                snapshot.request = None;
            }
        });
        self.set_phase(phase, detail);
    }

    fn fail(&mut self, error: String) {
        self.append_log(&error);
        self.unload(Phase::Error, Some(error));
    }

    fn load(&mut self, request: LoadRequest) {
        self.unload(Phase::Idle, None);
        let Some(runtime) = self.find_runtime() else {
            return self.fail(format!(
                "no {} runtime: build it (scripts/build-runtime.sh) or set {}",
                crate::PLUGIN_CODE,
                launch::RUNTIME_ENV
            ));
        };
        let port = match http::free_port() {
            Ok(port) => port,
            Err(error) => return self.fail(error),
        };
        let launch = match launch::command(&runtime, &request, port) {
            Ok(launch) => launch,
            Err(error) => return self.fail(error),
        };
        self.append_log(&format!(
            "loading {} on port {port}",
            request.model_path.display()
        ));
        self.update(|snapshot| snapshot.request = Some(request.clone()));
        self.set_phase(
            Phase::Loading,
            Some(format!("loading {}", request.model_label)),
        );
        match Group::spawn(&launch, &self.run) {
            Ok(child) => {
                self.child = Some(child);
                self.health = Some((port, launch.health_path.clone()));
                self.deadline =
                    Some(Instant::now() + Duration::from_secs(launch::LOAD_TIMEOUT_SECS));
                self.update(|snapshot| snapshot.engine = Some((port, launch.served_model.clone())));
            }
            Err(error) => self.fail(error),
        }
    }

    fn poll(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if let Some(status) = child.exited() {
            self.child = None;
            return self.fail(format!("the engine stopped ({status})"));
        }
        let Some((port, path)) = self.health.clone() else {
            return;
        };
        let healthy = http::request("GET", port, &path, &[], None, Duration::from_secs(1))
            .is_ok_and(|response| response.status == 200);
        let phase = self
            .shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .phase;
        match (phase, healthy) {
            (Phase::Loading, true) => {
                self.append_log("ready");
                self.deadline = None;
                self.set_phase(Phase::Ready, None);
            }
            (Phase::Loading, false)
                if self
                    .deadline
                    .is_some_and(|deadline| Instant::now() > deadline) =>
            {
                self.fail("the engine did not finish loading in time".into())
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_load_request_is_read_with_safe_defaults() {
        let request = LoadRequest::from_json(&json!({"model_path": "/m/model.gguf"})).unwrap();
        assert_eq!(request.model_label, "model");
        assert_eq!(request.context, DEFAULT_CONTEXT);
        assert_eq!(request.sessions, 1);
        assert!(request.link.is_none() && request.role.is_none());
        assert!(LoadRequest::from_json(&json!({}))
            .unwrap_err()
            .contains("model_path"));
        assert!(LoadRequest::from_json(&json!({"model_path": "m.gguf"})).is_err());
        assert!(LoadRequest::from_json(&json!({"model_path": "/m", "role": "x"})).is_err());
    }

    #[test]
    fn what_the_engine_cannot_run_is_refused_before_it_starts() {
        let mut request =
            LoadRequest::from_json(&json!({"model_path": "/nowhere/model.gguf"})).unwrap();
        assert!(request.check().unwrap_err().contains("not on this machine"));
        request.model_path = std::env::current_exe().unwrap();
        request.link = Some(json!({}));
        assert_eq!(request.check().is_err(), !launch::STAGES);
    }

    #[test]
    fn the_service_says_what_the_engine_runs_and_hides_no_secret() {
        let mut snapshot = Snapshot::new();
        let service = snapshot.service();
        assert_eq!(service["kind"], "inference");
        assert_eq!(service["status"], "waiting");
        snapshot.runtime = Some(PathBuf::from("/rt"));
        snapshot.phase = Phase::Ready;
        assert_eq!(snapshot.service()["status"], "ready");
        assert_eq!(snapshot.status_json(2)["busy"], true);
    }

    #[test]
    fn a_child_that_cannot_start_is_an_error_with_its_reason() {
        let base = std::env::temp_dir().join(format!("pom-engine-test-{}", std::process::id()));
        let engine = Engine::start(base.clone()).unwrap();
        assert_eq!(engine.upstream_json()["status"], "starting");
        let model = std::env::current_exe().unwrap();
        engine
            .load(LoadRequest::from_json(&json!({"model_path": model})).unwrap())
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while engine.snapshot().phase != Phase::Error && Instant::now() < until {
            thread::sleep(Duration::from_millis(50));
        }
        // No runtime in a test environment, or a runtime that is not a server.
        assert_eq!(engine.snapshot().phase, Phase::Error);
        assert_eq!(engine.upstream_json()["status"], "error");
        drop(engine);
        let _ = fs::remove_dir_all(base);
    }
}
