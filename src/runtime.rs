//! Mutable operating state of this node.
//!
//! Everything that used to be fixed by the command line at startup — which GGUF
//! to serve, which layers, the sampling knobs — lives here so the web UI can
//! change it while the process keeps running. The runtime owns the
//! `inference_server.py` child and restarts it when the configuration changes.
//!
//! This is supervision only. What the inference server *does* with a request is
//! untouched; the arguments passed to it are exactly the ones `run-inf.sh`
//! documents.

use crate::model::{LayerAssignment, ModelProfile};
use crate::network::Network;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::{watch, RwLock};
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

/// Give up restarting a crash-looping inference server after this many tries.
const MAX_RESTARTS: u32 = 10;

// ── Configuration ────────────────────────────────────────────────────────────

/// What this node is currently serving. Serialisable so the UI can round-trip it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeConfig {
    /// Path of the GGUF this peer loads — a full model or one slice of one.
    pub model_path: String,
    /// Label shared by every peer of the same model. The pipeline planner only
    /// chains peers whose label matches exactly, so this must agree across the
    /// cluster (see `ModelProfile::guess_from_name`).
    pub model_label: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    /// How many peers the model is split across.
    pub peer_count: u32,
    /// 0-based position of this peer in that split.
    pub peer_index: u32,
    /// This peer is the entry point clients talk to.
    pub primary: bool,

    pub inference_port: u16,
    pub n_gpu_layers: u32,
    pub n_ctx: u32,
    pub n_batch: u32,
    /// Concurrent generations; the context is split between them.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: u32,
    /// Prompt tokens pushed through the pipeline per round trip.
    #[serde(default = "default_prefill_chunk")]
    pub prefill_chunk_tokens: u32,
    pub max_new_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub repeat_penalty: f32,
    pub repeat_last_n: u32,
    pub no_thinking: bool,
    pub single_peer: bool,
    /// Disable embedding extraction in llama.cpp. Set on the last (sampling)
    /// peer of a multi-peer pipeline when `embeddings=True` breaks logits
    /// (observed with phi-4 where the model outputs EOS as first token).
    #[serde(default)]
    pub no_embeddings: bool,
}

fn default_max_sessions() -> u32 { 1 }
fn default_prefill_chunk() -> u32 { crate::prefill::DEFAULT_CHUNK_TOKENS }

impl NodeConfig {
    pub fn layers_count(&self) -> u32 {
        self.end_layer.saturating_sub(self.start_layer) + 1
    }

    fn validate(&self) -> Result<()> {
        if self.model_path.trim().is_empty() {
            bail!("model_path é obrigatório");
        }
        if !PathBuf::from(&self.model_path).exists() {
            bail!("arquivo não encontrado: {}", self.model_path);
        }
        if self.start_layer > self.end_layer {
            bail!(
                "faixa de camadas inválida: {}-{}",
                self.start_layer,
                self.end_layer
            );
        }
        if self.total_layers == 0 {
            bail!("total_layers não pode ser zero");
        }
        if self.end_layer >= self.total_layers {
            bail!(
                "camada final {} fora do modelo de {} camadas",
                self.end_layer,
                self.total_layers
            );
        }
        if self.peer_count == 0 {
            bail!("peer_count precisa ser >= 1");
        }
        if self.peer_index >= self.peer_count {
            bail!(
                "peer_index {} fora de um split de {} peers",
                self.peer_index,
                self.peer_count
            );
        }
        Ok(())
    }

    /// The assignment this configuration implies, for gossip and the TUI.
    pub fn to_assignment(&self, profile: &ModelProfile) -> LayerAssignment {
        let count = self.layers_count();
        LayerAssignment {
            model: self.model_label.clone(),
            total_layers: self.total_layers,
            start_layer: self.start_layer,
            end_layer: self.end_layer,
            layers_count: count,
            memory_used_mb: count as f64 * profile.memory_per_layer_mb + profile.overhead_mb,
            memory_allocated_mb: 0.0,
        }
    }
}

// ── Status ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    /// No inference server configured on this node.
    Disabled,
    /// Process spawned, model still loading.
    Starting,
    /// `/health` answered and the model is loaded.
    Ready,
    /// Crashed past the restart budget.
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub state: ServerState,
    pub detail: String,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub log_path: Option<String>,
    /// Layer count read back from the shard's GGUF metadata via `/health`.
    pub reported_total_layers: Option<u32>,
    pub changed_at: String,
}

impl RuntimeStatus {
    fn new(state: ServerState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
            pid: None,
            restarts: 0,
            log_path: None,
            reported_total_layers: None,
            changed_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

// ── Runtime ──────────────────────────────────────────────────────────────────

pub struct NodeRuntime {
    config: RwLock<Option<NodeConfig>>,
    /// Behind its own Arc so supervisor tasks, which outlive any `&self`
    /// borrow, can keep writing to it.
    status: Arc<RwLock<RuntimeStatus>>,
    assignment: Arc<RwLock<Option<LayerAssignment>>>,
    net: Arc<Network>,
    /// Bumped on every `apply`; the supervisor of an older generation exits.
    generation: watch::Sender<u64>,
    /// Working directory the Python process and model paths are resolved from.
    cwd: PathBuf,
}

impl NodeRuntime {
    pub fn new(
        net: Arc<Network>,
        assignment: Arc<RwLock<Option<LayerAssignment>>>,
        cwd: PathBuf,
    ) -> Arc<Self> {
        let (generation, _) = watch::channel(0u64);
        Arc::new(Self {
            config: RwLock::new(None),
            status: Arc::new(RwLock::new(RuntimeStatus::new(
                ServerState::Disabled,
                "nenhum modelo carregado",
            ))),
            assignment,
            net,
            generation,
            cwd,
        })
    }

    pub async fn config(&self) -> Option<NodeConfig> {
        self.config.read().await.clone()
    }

    pub async fn status(&self) -> RuntimeStatus {
        self.status.read().await.clone()
    }

    /// Label the pipeline planner matches on; falls back to the CLI default.
    pub async fn model_label(&self, fallback: &str) -> String {
        self.config
            .read()
            .await
            .as_ref()
            .map(|c| c.model_label.clone())
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Directory the Python interpreter is resolved from.
    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    /// Stop the current inference server and announce to peers that this
    /// node no longer serves any layers. Called before a cluster reconfiguration
    /// so no peer sends inference requests to a stale endpoint.
    pub async fn stop(&self) {
        info!("parando servidor de inferencia neste no");

        let generation = *self.generation.borrow() + 1;
        self.generation.send_replace(generation);

        *self.config.write().await = None;
        *self.assignment.write().await = None;
        *self.status.write().await =
            RuntimeStatus::new(ServerState::Disabled, "parado para reconfiguracao");

        let _ = self.net.announce(None).await;
    }

    /// Switch this node to a new configuration: stop the current inference
    /// server, re-announce the new layer range, start the new server.
    pub async fn apply(&self, cfg: NodeConfig) -> Result<()> {
        cfg.validate()?;

        let profile = ModelProfile::guess_from_name(&cfg.model_path);
        let assignment = cfg.to_assignment(&profile);

        info!(
            "⚙️  Reconfigurando nó: {} camadas {}-{}/{} (peer {}/{})",
            cfg.model_label,
            cfg.start_layer,
            cfg.end_layer,
            cfg.total_layers,
            cfg.peer_index + 1,
            cfg.peer_count
        );

        // Bumping the generation stops the previous supervisor, which kills its
        // child on the way out.
        //
        // `send_replace`, not `send`: with no live receivers — the case on the
        // very first apply, before any supervisor exists — `send` reports an
        // error and leaves the stored value untouched, so the supervisor we are
        // about to spawn would read a stale generation and exit immediately.
        let generation = *self.generation.borrow() + 1;
        self.generation.send_replace(generation);

        *self.config.write().await = Some(cfg.clone());
        *self.assignment.write().await = Some(assignment.clone());
        *self.status.write().await =
            RuntimeStatus::new(ServerState::Starting, "iniciando servidor de inferência");

        // Let peers know immediately instead of waiting for the next gossip tick.
        if let Err(e) = self.net.announce(Some(&assignment)).await {
            warn!("announce após reconfiguração falhou: {e}");
        }

        self.spawn_supervisor(cfg, generation);
        Ok(())
    }

    /// Start (or restart) the `inference_server.py` child for `generation`.
    fn spawn_supervisor(&self, cfg: NodeConfig, generation: u64) {
        let python = self.python_exe();
        let script = self.cwd.join("inference_server.py");
        let log_path = self.cwd.join(format!(
            "inference_server_{}.log",
            cfg.inference_port
        ));
        let port = cfg.inference_port;
        let mut gen_rx = self.generation.subscribe();

        // The supervisor needs the runtime to publish status; a raw pointer would
        // be wrong here, so hand it the pieces it writes to.
        let status = StatusWriter {
            status: self.status_handle(),
        };

        tokio::spawn(async move {
            let log_str = log_path.to_string_lossy().into_owned();
            status.set_log(&log_str).await;

            // Before starting, kill anything already holding our port - it may
            // be a zombie from a previous rdma-p2p run or a stale watcher loop.
            free_port(port).await;

            let mut restarts = 0u32;
            loop {
                if *gen_rx.borrow() != generation {
                    return; // superseded by a newer configuration
                }
                if restarts > 0 {
                    warn!(
                        "reiniciando servidor de inferência (tentativa {}/{})",
                        restarts, MAX_RESTARTS
                    );
                    sleep(Duration::from_secs(5)).await;
                    if *gen_rx.borrow() != generation {
                        return;
                    }
                }

                let mut cmd = Command::new(&python);
                cmd.env("PYTHONUNBUFFERED", "1")
                    .arg(&script)
                    .arg("--model").arg(&cfg.model_path)
                    .arg("--start-layer").arg(cfg.start_layer.to_string())
                    .arg("--end-layer").arg(cfg.end_layer.to_string())
                    .arg("--port").arg(cfg.inference_port.to_string())
                    // Explicit, not inherited from the script's default: a peer
                    // known by an IPv6 address is dialled at http://[addr]:port,
                    // which an IPv4-only listener refuses.
                    .arg("--host").arg("::")
                    .arg("--n-gpu-layers").arg(cfg.n_gpu_layers.to_string())
                    .arg("--n-ctx").arg(cfg.n_ctx.to_string())
                    .arg("--n-batch").arg(cfg.n_batch.to_string())
                    .arg("--max-sessions").arg(cfg.max_sessions.max(1).to_string())
                    .arg("--max-new-tokens").arg(cfg.max_new_tokens.to_string())
                    .arg("--temperature").arg(cfg.temperature.to_string())
                    .arg("--top-p").arg(cfg.top_p.to_string())
                    .arg("--top-k").arg(cfg.top_k.to_string())
                    .arg("--repeat-penalty").arg(cfg.repeat_penalty.to_string())
                    .arg("--repeat-last-n").arg(cfg.repeat_last_n.to_string())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if cfg.no_thinking {
                    cmd.arg("--no-thinking");
                }
                if cfg.single_peer {
                    cmd.arg("--single-peer");
                }
                if cfg.no_embeddings {
                    cmd.arg("--no-embeddings");
                }

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        error!("falha ao iniciar servidor de inferência: {e}");
                        restarts += 1;
                        if restarts >= MAX_RESTARTS {
                            status
                                .set(ServerState::Failed, format!("não iniciou: {e}"), restarts)
                                .await;
                            return;
                        }
                        continue;
                    }
                };

                let pid = child.id();
                status.set_pid(pid).await;
                status
                    .set(
                        ServerState::Starting,
                        format!("carregando {}", short_path(&cfg.model_path)),
                        restarts,
                    )
                    .await;

                pipe_output(child.stdout.take(), &log_str, "OUT", false);
                pipe_output(child.stderr.take(), &log_str, "ERR", true);

                tokio::select! {
                    result = child.wait() => {
                        warn!("servidor de inferência encerrou: {result:?}");
                        restarts += 1;
                        if restarts >= MAX_RESTARTS {
                            status.set(
                                ServerState::Failed,
                                format!("encerrou {MAX_RESTARTS}x — veja {log_str}"),
                                restarts,
                            ).await;
                            return;
                        }
                    }
                    _ = gen_rx.changed() => {
                        info!("configuração trocada — encerrando servidor de inferência anterior");
                        let _ = child.kill().await;
                        return;
                    }
                }
            }
        });

        self.spawn_health_watch(cfg.inference_port, generation);
    }

    /// Poll the inference server until it answers, then mark the node ready and
    /// correct `total_layers` from the shard's GGUF metadata.
    fn spawn_health_watch(&self, port: u16, generation: u64) {
        let status = StatusWriter {
            status: self.status_handle(),
        };
        let assignment = self.assignment.clone();
        let gen_rx = self.generation.subscribe();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();

        tokio::spawn(async move {
            let url = format!("http://127.0.0.1:{port}/health");
            loop {
                sleep(Duration::from_secs(3)).await;
                if *gen_rx.borrow() != generation {
                    return;
                }

                let Ok(resp) = http.get(&url).send().await else {
                    continue;
                };
                if !resp.status().is_success() {
                    continue;
                }
                let Ok(body) = resp.json::<serde_json::Value>().await else {
                    continue;
                };
                if !body.get("model_loaded").and_then(|v| v.as_bool()).unwrap_or(false) {
                    continue;
                }

                let reported = body
                    .get("original_block_count")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);

                // Shards written by split_model.py carry the real layer count;
                // trust it over the name-based guess.
                if let Some(total) = reported {
                    let mut w = assignment.write().await;
                    if let Some(ref mut a) = *w {
                        if a.total_layers != total {
                            info!("total_layers corrigido: {} → {total} (metadata GGUF)", a.total_layers);
                            a.total_layers = total;
                        }
                    }
                }

                status.set_ready(reported).await;
                return;
            }
        });
    }

    fn python_exe(&self) -> String {
        // The venv at the repo root is what has llama-cpp-python; the system
        // python3 on macOS is 3.9 without numpy and dies in a restart loop.
        let venv = self.cwd.join("venv/bin/python3");
        if venv.exists() {
            venv.to_string_lossy().into_owned()
        } else {
            "python3".to_string()
        }
    }

    fn status_handle(&self) -> Arc<RwLock<RuntimeStatus>> {
        self.status.clone()
    }
}

fn short_path(path: &str) -> String {
    PathBuf::from(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Can the inference server still take this port?
///
/// It binds the dual-stack wildcard `::`, so that is what has to be probed: a
/// `0.0.0.0` probe succeeds while a `::` listener already owns the port on some
/// stacks, and the supervisor would then start a child doomed to `address
/// already in use`. Falls back to IPv4 on a host with IPv6 turned off.
fn port_is_free(port: u16) -> bool {
    use std::net::TcpListener;
    match TcpListener::bind(("::", port)) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => false,
        Err(_) => TcpListener::bind(("0.0.0.0", port)).is_ok(),
    }
}

/// Kill any process holding `port` so the inference server can bind.
///
/// Uses `lsof -ti :PORT` to find the PID, sends SIGKILL, then polls the port
/// for up to 5 seconds until it is free.
async fn free_port(port: u16) {
    // Fast path: if we can bind, the port is already free.
    if port_is_free(port) {
        return;
    }

    info!("porta {port} em uso — procurando processo para matar");

    let output = tokio::process::Command::new("lsof")
        .args(["-ti", &format!(":{port}")])
        .output()
        .await;

    let Ok(out) = output else {
        warn!("nao foi possivel executar lsof para verificar a porta {port}");
        return;
    };

    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        warn!("porta {port} ocupada mas lsof nao encontrou o processo");
        return;
    }

    for pid_str in stdout.split_whitespace() {
        let Ok(pid) = pid_str.parse::<i32>() else { continue };
        // Skip our own process - never kill ourselves.
        if pid == std::process::id() as i32 {
            continue;
        }
        info!("matando processo {pid} que ocupa a porta {port}");
        let _ = Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .output()
            .await;
    }

    // Poll for up to 5 seconds until the port is free.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if port_is_free(port) {
            info!("porta {port} liberada");
            return;
        }
        if tokio::time::Instant::now() > deadline {
            warn!("porta {port} continua ocupada apos 5s — seguindo mesmo assim");
            return;
        }
        sleep(Duration::from_millis(200)).await;
    }
}

/// Stream a child pipe into the log file and the terminal.
fn pipe_output<R>(pipe: Option<R>, log_path: &str, tag: &'static str, is_err: bool)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(pipe) = pipe else { return };
    let log_path = log_path.to_string();
    tokio::spawn(async move {
        use std::io::Write;
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if is_err {
                eprintln!("\x1b[31m[InfServer ERR]\x1b[0m {line}");
            } else {
                println!("\x1b[36m[InfServer]\x1b[0m {line}");
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                let _ = writeln!(f, "[{tag}] {line}");
            }
        }
    });
}

// ── Status plumbing ──────────────────────────────────────────────────────────
//
// Supervisor tasks outlive the borrow of `&self`, so they need an owned handle
// to the status cell. The runtime keeps that cell behind its own Arc.

struct StatusWriter {
    status: Arc<RwLock<RuntimeStatus>>,
}

impl StatusWriter {
    async fn set(&self, state: ServerState, detail: String, restarts: u32) {
        let mut s = self.status.write().await;
        s.state = state;
        s.detail = detail;
        s.restarts = restarts;
        s.changed_at = chrono::Utc::now().to_rfc3339();
    }

    async fn set_pid(&self, pid: Option<u32>) {
        self.status.write().await.pid = pid;
    }

    async fn set_log(&self, path: &str) {
        self.status.write().await.log_path = Some(path.to_string());
    }

    async fn set_ready(&self, reported_total_layers: Option<u32>) {
        let mut s = self.status.write().await;
        s.state = ServerState::Ready;
        s.detail = "modelo carregado".to_string();
        s.reported_total_layers = reported_total_layers;
        s.changed_at = chrono::Utc::now().to_rfc3339();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> NodeConfig {
        NodeConfig {
            model_path: "/nonexistent/model.gguf".into(),
            model_label: "qwen35".into(),
            start_layer: 0,
            end_layer: 15,
            total_layers: 32,
            peer_count: 2,
            peer_index: 0,
            primary: true,
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx: 8192,
            max_sessions: 1,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            n_batch: 512,
            max_new_tokens: 256,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
        }
    }

    #[test]
    fn rejects_missing_file() {
        assert!(cfg().validate().is_err());
    }

    #[test]
    fn rejects_inverted_range() {
        let mut c = cfg();
        c.model_path = file!().to_string();
        c.start_layer = 20;
        c.end_layer = 5;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_range_past_end_of_model() {
        let mut c = cfg();
        c.model_path = file!().to_string();
        c.end_layer = 40;
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_peer_index_outside_split() {
        let mut c = cfg();
        c.model_path = file!().to_string();
        c.peer_index = 5;
        assert!(c.validate().is_err());
    }

    #[test]
    fn layers_count_is_inclusive() {
        assert_eq!(cfg().layers_count(), 16);
    }
}
