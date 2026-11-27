//! The real `pom-plugin-host`, sandboxed by the policy the node hands it, with
//! the fixture plugin's `probe` entry point asking from inside what it can
//! reach. Linux only: the other systems get their sandbox in later phases.
#![cfg(target_os = "linux")]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// The fixture `cdylib`, built next to this test's binaries.
fn fixture() -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "-p", "enterprise-plugin-fixture"])
        .status()
        .expect("build the fixture plugin");
    assert!(status.success(), "the fixture plugin did not build");
    let host = PathBuf::from(env!("CARGO_BIN_EXE_pom-plugin-host"));
    let library = host
        .parent()
        .unwrap()
        .join("libenterprise_plugin_fixture.so");
    assert!(library.is_file(), "{}", library.display());
    library
}

fn landlock_abi() -> i64 {
    unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0,
            1,
        )
    }
}

struct Host {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    next: u64,
}

impl Host {
    fn start(library: &Path, policy: Option<Value>) -> Self {
        // `POM_TEST_PLUGIN_HOST`: the same binary at a path an AppArmor
        // profile allows user namespaces for, to exercise the extra layer.
        let binary = std::env::var("POM_TEST_PLUGIN_HOST")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_pom-plugin-host").to_owned());
        let mut command = Command::new(binary);
        command
            .arg(library)
            .arg("probe")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        match policy {
            Some(policy) => command.env("POM_PLUGIN_SANDBOX", policy.to_string()),
            None => command.env_remove("POM_PLUGIN_SANDBOX"),
        };
        let mut child = command.spawn().expect("start the host");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Self {
            child,
            stdin,
            stdout,
            next: 1,
        }
    }

    fn call(&mut self, method: &str, payload: Value) -> Value {
        let body = json!({"id": self.next, "method": method, "payload": payload}).to_string();
        self.next += 1;
        self.stdin
            .write_all(&(body.len() as u32).to_be_bytes())
            .unwrap();
        self.stdin.write_all(body.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
        let mut prefix = [0u8; 4];
        self.stdout
            .read_exact(&mut prefix)
            .expect("the host answered");
        let mut answer = vec![0u8; u32::from_be_bytes(prefix) as usize];
        self.stdout.read_exact(&mut answer).unwrap();
        let answer: Value = serde_json::from_slice(&answer).unwrap();
        assert_eq!(answer["ok"], true, "{answer}");
        answer["result"].clone()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.call("shutdown", Value::Null);
        let _ = self.child.wait();
    }
}

/// A plugin home, a workspace, another plugin's home and a secret, outside
/// `/tmp` (which the baseline does not grant anyway).
struct Layout {
    _root: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
    other: PathBuf,
    secret: PathBuf,
}

fn layout() -> Layout {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = root.path().join("probe");
    let workspace = root.path().join("workspace");
    let other = root.path().join("other");
    for dir in [&home, &workspace, &other] {
        std::fs::create_dir(dir).unwrap();
    }
    std::fs::write(other.join("data"), b"x").unwrap();
    let secret = root.path().join("pom.json");
    std::fs::write(&secret, b"{\"connection_key\":\"k\"}").unwrap();
    Layout {
        _root: root,
        home,
        workspace,
        other,
        secret,
    }
}

fn probe(host: &mut Host, layout: &Layout, pid: u32) -> Value {
    host.call(
        "query",
        json!({
            "read": [layout.secret, layout.other.join("data"), "/etc/hostname"],
            "write": [layout.workspace.join("w"), layout.home.join("h"), layout.other.join("x")],
            "pid": pid,
        }),
    )
}

fn refused(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| text.starts_with("refused"))
}

#[test]
fn a_legacy_plugin_runs_unconfined_as_before() {
    let library = fixture();
    let layout = layout();
    let mut host = Host::start(&library, None);
    let ready = host.call("ready", Value::Null);
    assert!(ready.get("sandbox").is_none(), "{ready}");
    let answer = probe(&mut host, &layout, std::process::id());
    assert_eq!(answer["read"][layout.secret.to_str().unwrap()], "ok");
    assert_eq!(answer["signal"], "ok");
}

#[test]
fn the_baseline_keeps_the_plugin_to_its_folders_and_away_from_other_processes() {
    if landlock_abi() < 6 {
        eprintln!("skipped: Landlock ABI {} on this kernel", landlock_abi());
        return;
    }
    let library = fixture();
    let layout = layout();
    let policy = json!({
        "read_write": [layout.home, layout.workspace],
        "read_only": [library.parent().unwrap()],
        "pid_namespace": false,
    });
    let mut host = Host::start(&library, Some(policy));
    let ready = host.call("ready", Value::Null);
    assert_eq!(ready["sandbox"]["level"], "baseline", "{ready}");

    let answer = probe(&mut host, &layout, std::process::id());
    let read = &answer["read"];
    let write = &answer["write"];
    assert!(
        refused(&read[layout.secret.to_str().unwrap()]),
        "the node's secrets: {answer}"
    );
    assert!(
        refused(&read[layout.other.join("data").to_str().unwrap()]),
        "another plugin's files: {answer}"
    );
    assert_eq!(
        read["/etc/hostname"], "ok",
        "system files stay readable: {answer}"
    );
    assert_eq!(
        write[layout.workspace.join("w").to_str().unwrap()],
        "ok",
        "the shared workspace: {answer}"
    );
    assert_eq!(
        write[layout.home.join("h").to_str().unwrap()],
        "ok",
        "its own home: {answer}"
    );
    assert!(
        refused(&write[layout.other.join("x").to_str().unwrap()]),
        "{answer}"
    );
    assert!(
        refused(&answer["signal"]),
        "no signal to a process outside: {answer}"
    );
    assert!(refused(&answer["ptrace"]), "no ptrace: {answer}");
}

#[test]
fn the_extra_layer_hides_every_other_process_where_the_system_allows_it() {
    if landlock_abi() < 6 {
        eprintln!("skipped: Landlock ABI {} on this kernel", landlock_abi());
        return;
    }
    let library = fixture();
    let layout = layout();
    let policy = json!({
        "read_write": [layout.home, layout.workspace],
        "read_only": [library.parent().unwrap()],
        "pid_namespace": true,
    });
    let mut host = Host::start(&library, Some(policy));
    let ready = host.call("ready", Value::Null);
    let level = ready["sandbox"]["level"].as_str().unwrap().to_owned();
    let answer = probe(&mut host, &layout, std::process::id());
    assert!(
        refused(&answer["read"][layout.secret.to_str().unwrap()]),
        "{answer}"
    );
    assert!(refused(&answer["signal"]), "{answer}");
    match level.as_str() {
        "pid_namespace" => {
            assert_eq!(
                answer["pid"], 1,
                "the host is PID 1 of its namespace: {answer}"
            );
            assert!(
                answer["visible_processes"].as_u64().unwrap() <= 2,
                "{answer}"
            );
        }
        "baseline" => eprintln!("user namespaces refused here: the host fell back to the baseline"),
        other => panic!("unexpected level {other}"),
    }
}
