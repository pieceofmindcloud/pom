//! Web interface served by the peer itself, plus the control API behind it.
//!
//! The assets are compiled into the binary (`include_str!`), so there is no
//! frontend build step and no files to deploy alongside `rdma-p2p` — the same
//! single binary that joins the network also serves the dashboard.
//!
//! Every node serves it, so the UI can be opened on any peer. The cluster view
//! works by asking each peer's own `/api/ui/node`, which is why `PeerInfo`
//! carries `gateway_port`.

use crate::auth::{AuthState, ConnectionAuth};
use crate::catalog::{self, Catalog};
use crate::config::Role;
use crate::gateway::GatewayState;
use crate::gguf;
use crate::plan::{self, PeerBudget, DEFAULT_RESERVE_PCT};
use crate::runtime::{NodeConfig, RuntimeStatus};
use crate::sysmon::{self, SystemLoad};
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

/// How long a scanned catalog is reused before walking the filesystem again.
const CATALOG_TTL: Duration = Duration::from_secs(20);
/// Timeout for one peer-to-peer control call.
const PEER_TIMEOUT: Duration = Duration::from_secs(4);

pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/", get(index))
        .route("/ui/app.css", get(stylesheet))
        .route("/ui/app.js", get(script))
        .route("/api/ui/node", get(node_info))
        .route("/api/ui/models", get(models))
        .route("/api/ui/cluster", get(cluster))
        .route("/api/ui/cluster/models", get(cluster_models))
        .route("/api/ui/plan", post(plan_split_endpoint))
        .route("/api/ui/apply", post(apply))
        .route("/api/ui/launch", post(launch))
        .route("/api/ui/jobs/:id", get(job_status))
        .route("/api/setup/key", get(get_setup_key))
        .route("/api/setup/save", post(save_setup))
}

// ── Static assets ────────────────────────────────────────────────────────────

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/app.css"),
    )
}

async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}

// ── Node view ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct NodeInfo {
    pub node_id: String,
    pub short_id: String,
    pub hostname: String,
    pub role: String,
    pub connection_key: String,
    pub p2p_port: u16,
    pub gateway_port: u16,
    pub inference_port: Option<u16>,
    pub config: Option<NodeConfig>,
    pub status: RuntimeStatus,
    pub system: SystemLoad,
    pub peer_count: usize,
    pub load: u32,
    pub active_model: String,
}

async fn node_info(State(state): State<GatewayState>) -> Json<NodeInfo> {
    let system = sysmon::sample().await;
    let (role, conn_key) = {
        let cfg = state.auth.read().await;
        (cfg.role.to_string(), cfg.connection_key.clone())
    };
    Json(NodeInfo {
        short_id: state.network.node_id[..state.network.node_id.len().min(12)].to_string(),
        node_id: state.network.node_id.clone(),
        hostname: system.hostname.clone(),
        role,
        connection_key: conn_key,
        p2p_port: state.network.bind_addr.port(),
        gateway_port: state.gateway_port,
        inference_port: state.network.inference_port,
        config: state.runtime.config().await,
        status: state.runtime.status().await,
        peer_count: state.network.alive_peers().await.len(),
        load: state.loads.load(&state.network.node_id).await,
        active_model: state.active_model().await,
        system,
    })
}

// ── Model catalog ────────────────────────────────────────────────────────────

struct CachedCatalog {
    at: Instant,
    catalog: Arc<Catalog>,
}

fn catalog_cache() -> &'static Mutex<Option<CachedCatalog>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<CachedCatalog>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

async fn local_catalog(state: &GatewayState) -> Arc<Catalog> {
    {
        let guard = catalog_cache().lock().await;
        if let Some(ref c) = *guard {
            if c.at.elapsed() < CATALOG_TTL {
                return c.catalog.clone();
            }
        }
    }

    let dir = state.runtime.models_dir();
    // The walk touches the filesystem; keep it off the async reactor.
    let scanned = tokio::task::spawn_blocking(move || catalog::discover(&dir))
        .await
        .unwrap_or_else(|e| {
            warn!("varredura de modelos falhou: {e}");
            catalog::discover(std::path::Path::new("models"))
        });

    let catalog = Arc::new(scanned);
    *catalog_cache().lock().await = Some(CachedCatalog {
        at: Instant::now(),
        catalog: catalog.clone(),
    });
    catalog
}

async fn models(State(state): State<GatewayState>) -> impl IntoResponse {
    let catalog = local_catalog(&state).await;
    let system = sysmon::sample().await;
    Json(json!({
        "hostname": system.hostname,
        "node_id": state.network.node_id,
        "models": catalog.models,
        "shard_sets": catalog.shard_sets,
        "scanned": catalog.scanned,
    }))
}

// ── Cluster view ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct PeerView {
    node_id: String,
    short_id: String,
    addr: String,
    model: String,
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    inference_url: Option<String>,
    gateway_url: Option<String>,
    load: u32,
    is_self: bool,
    /// Present when the peer answered `/api/ui/node`.
    node: Option<serde_json::Value>,
    /// Why the peer could not be reached, when it could not.
    unreachable: Option<String>,
}

async fn cluster(State(state): State<GatewayState>) -> impl IntoResponse {
    let peers = state.all_peers().await;
    let self_id = state.network.node_id.clone();

    // Fan out to every peer at once; a slow peer must not serialise the others.
    let futures: Vec<_> = peers
        .iter()
        .map(|p| {
            let is_self = p.node_id == self_id;
            let url = p.gateway_url();
            let http = state.http.clone();
            async move {
                if is_self || url.is_none() {
                    return (None, None);
                }
                let url = format!("{}/api/ui/node", url.unwrap());
                match tokio::time::timeout(PEER_TIMEOUT, http.get(&url).send()).await {
                    Ok(Ok(r)) if r.status().is_success() => match r.json().await {
                        Ok(v) => (Some(v), None),
                        Err(e) => (None, Some(format!("resposta inválida: {e}"))),
                    },
                    Ok(Ok(r)) => (None, Some(format!("HTTP {}", r.status()))),
                    Ok(Err(e)) => (None, Some(format!("{e}"))),
                    Err(_) => (None, Some("timeout".to_string())),
                }
            }
        })
        .collect();

    let fetched = futures::future::join_all(futures).await;

    let mut views = Vec::new();
    for (p, (node, unreachable)) in peers.iter().zip(fetched) {
        let is_self = p.node_id == self_id;
        let node = if is_self {
            // Serialise our own view through the same shape as a remote one.
            serde_json::to_value(node_info(State(state.clone())).await.0).ok()
        } else {
            node
        };
        views.push(PeerView {
            short_id: p.node_id[..p.node_id.len().min(12)].to_string(),
            node_id: p.node_id.clone(),
            addr: p.addr.clone(),
            model: p.model.clone(),
            start_layer: p.start_layer,
            end_layer: p.end_layer,
            total_layers: p.total_layers,
            inference_url: p.inference_url(),
            gateway_url: p.gateway_url(),
            load: state.loads.load(&p.node_id).await,
            is_self,
            node,
            unreachable,
        });
    }

    views.sort_by_key(|v| (v.start_layer, v.short_id.clone()));

    // Layer coverage for the model this node is serving.
    let active = state.active_model().await;
    let total = views
        .iter()
        .find(|v| v.model == active)
        .map(|v| v.total_layers)
        .unwrap_or(0);
    let mut covered = vec![false; total as usize];
    for v in views.iter().filter(|v| v.model == active) {
        for l in v.start_layer..=v.end_layer.min(total.saturating_sub(1)) {
            if let Some(slot) = covered.get_mut(l as usize) {
                *slot = true;
            }
        }
    }
    let covered_count = covered.iter().filter(|c| **c).count();

    let self_node = serde_json::to_value(node_info(State(state.clone())).await.0).ok();

    Json(json!({
        "self_id": self_id,
        "active_model": active,
        "total_layers": total,
        "covered_layers": covered_count,
        "coverage": covered,
        "complete": total > 0 && covered_count == total as usize,
        "peers": views,
        "node": self_node,
    }))
}

/// Each peer's installed models, so the setup screen can say who is missing what.
async fn cluster_models(State(state): State<GatewayState>) -> impl IntoResponse {
    let peers = state.all_peers().await;
    let self_id = state.network.node_id.clone();
    let local = local_catalog(&state).await;
    let local_host = sysmon::sample().await.hostname;

    let futures: Vec<_> = peers
        .iter()
        .filter(|p| p.node_id != self_id)
        .map(|p| {
            let url = p.gateway_url();
            let http = state.http.clone();
            let node_id = p.node_id.clone();
            async move {
                let Some(base) = url else {
                    return (node_id, None, Some("sem porta de gateway anunciada".to_string()));
                };
                let url = format!("{base}/api/ui/models");
                match tokio::time::timeout(PEER_TIMEOUT * 3, http.get(&url).send()).await {
                    Ok(Ok(r)) if r.status().is_success() => match r.json::<serde_json::Value>().await
                    {
                        Ok(v) => (node_id, Some(v), None),
                        Err(e) => (node_id, None, Some(format!("resposta inválida: {e}"))),
                    },
                    Ok(Ok(r)) => (node_id, None, Some(format!("HTTP {}", r.status()))),
                    Ok(Err(e)) => (node_id, None, Some(format!("{e}"))),
                    Err(_) => (node_id, None, Some("timeout".to_string())),
                }
            }
        })
        .collect();

    let mut out = vec![json!({
        "node_id": self_id,
        "hostname": local_host,
        "is_self": true,
        "models": local.models,
        "shard_sets": local.shard_sets,
        "scanned": local.scanned,
    })];

    for (node_id, value, error) in futures::future::join_all(futures).await {
        match value {
            Some(v) => out.push(json!({
                "node_id": node_id,
                "hostname": v.get("hostname").cloned().unwrap_or(json!(null)),
                "is_self": false,
                "models": v.get("models").cloned().unwrap_or(json!([])),
                "shard_sets": v.get("shard_sets").cloned().unwrap_or(json!([])),
                "scanned": v.get("scanned").cloned().unwrap_or(json!([])),
            })),
            None => out.push(json!({
                "node_id": node_id,
                "is_self": false,
                "error": error,
                "models": [],
                "shard_sets": [],
            })),
        }
    }

    Json(json!({ "peers": out }))
}

// ── Planning ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct PlanRequest {
    /// Path of the source model, as seen on the node named by `layout_node_id`.
    pub model_path: String,
    pub budgets: Vec<PeerBudget>,
    #[serde(default)]
    pub reserve_pct: Option<f64>,
    /// Node whose copy of the GGUF should be measured. The dashboard can run on
    /// a peer that does not have the model — the MacBook's LM Studio library is
    /// nearly empty while the ksia's is not — so the layout is read wherever the
    /// file actually is.
    #[serde(default)]
    pub layout_node_id: Option<String>,
}

/// Work out asymmetric layer ranges from each peer's declared memory budget.
async fn plan_split_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<PlanRequest>,
) -> Response {
    // Delegate to the peer that holds the file when it is not this one.
    if let Some(ref target) = req.layout_node_id {
        if *target != state.network.node_id {
            let base = state
                .all_peers()
                .await
                .iter()
                .find(|p| p.node_id == *target)
                .and_then(|p| p.gateway_url());
            let Some(base) = base else {
                return bad_request(format!("peer {target} não está acessível para ler o GGUF"));
            };
            let mut forwarded = req;
            // Clear the hop marker so the receiving peer measures locally.
            forwarded.layout_node_id = None;
            return match state
                .http
                .post(format!("{base}/api/ui/plan"))
                .json(&forwarded)
                .send()
                .await
            {
                Ok(r) => {
                    let status = r.status();
                    match r.json::<serde_json::Value>().await {
                        Ok(v) => (status, Json(v)).into_response(),
                        Err(e) => bad_request(format!("resposta inválida do peer: {e}")),
                    }
                }
                Err(e) => bad_request(format!("peer não respondeu: {e}")),
            };
        }
    }

    let path = std::path::PathBuf::from(&req.model_path);
    let layout = match tokio::task::spawn_blocking(move || gguf::read_layout(&path)).await {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => return bad_request(format!("não foi possível ler o GGUF: {e}")),
        Err(e) => return bad_request(format!("leitura do GGUF falhou: {e}")),
    };

    let reserve = req.reserve_pct.unwrap_or(DEFAULT_RESERVE_PCT);
    match plan::plan_split(&req.model_path, &layout, &req.budgets, reserve) {
        Ok(p) => Json(p).into_response(),
        Err(e) => bad_request(e.to_string()),
    }
}

// ── Applying a configuration ─────────────────────────────────────────────────

/// Sampling and context knobs. Defaults match `run-p1.sh`, which is the
/// configuration the two-peer cluster was validated with.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RuntimeParams {
    #[serde(default = "d_gpu_layers")]
    pub n_gpu_layers: u32,
    #[serde(default = "d_n_ctx")]
    pub n_ctx: u32,
    #[serde(default = "d_n_batch")]
    pub n_batch: u32,
    #[serde(default = "d_max_new")]
    pub max_new_tokens: u32,
    /// Generations allowed in flight at once. The KV cache is split evenly
    /// between them, so this divides the usable window: raising it buys
    /// concurrency and costs context.
    #[serde(default = "d_max_sessions")]
    pub max_sessions: u32,
    #[serde(default = "d_temp")]
    pub temperature: f32,
    #[serde(default = "d_top_p")]
    pub top_p: f32,
    #[serde(default = "d_top_k")]
    pub top_k: u32,
    #[serde(default = "d_repeat_penalty")]
    pub repeat_penalty: f32,
    #[serde(default = "d_repeat_last_n")]
    pub repeat_last_n: u32,
    #[serde(default = "d_true")]
    pub no_thinking: bool,
}

fn d_gpu_layers() -> u32 { 99 }
fn d_n_ctx() -> u32 { 8192 }
fn d_n_batch() -> u32 { 512 }
fn d_max_new() -> u32 { 256 }
fn d_max_sessions() -> u32 { 1 }
fn d_temp() -> f32 { 0.7 }
fn d_top_p() -> f32 { 0.9 }
fn d_top_k() -> u32 { 40 }
fn d_repeat_penalty() -> f32 { 1.15 }
fn d_repeat_last_n() -> u32 { 64 }
fn d_true() -> bool { true }

impl Default for RuntimeParams {
    fn default() -> Self {
        Self {
            n_gpu_layers: d_gpu_layers(),
            n_ctx: d_n_ctx(),
            n_batch: d_n_batch(),
            max_new_tokens: d_max_new(),
            max_sessions: d_max_sessions(),
            temperature: d_temp(),
            top_p: d_top_p(),
            top_k: d_top_k(),
            repeat_penalty: d_repeat_penalty(),
            repeat_last_n: d_repeat_last_n(),
            no_thinking: d_true(),
        }
    }
}

/// Told to a single peer: slice this range out of your local copy and serve it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApplyRequest {
    /// Full model on the receiving peer's filesystem.
    pub source_model_path: String,
    /// Shared across peers so every shard resolves to the same model label —
    /// the pipeline planner only chains peers whose label matches exactly.
    pub base_name: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    pub peer_count: u32,
    pub peer_index: u32,
    #[serde(default)]
    pub primary: bool,
    /// Set when the source is already a shard (or covers the whole model) and
    /// no slicing is needed.
    #[serde(default)]
    pub skip_split: bool,
    #[serde(default)]
    pub params: RuntimeParams,
}

impl ApplyRequest {
    fn is_first(&self) -> bool {
        self.start_layer == 0
    }
    fn is_last(&self) -> bool {
        self.end_layer + 1 >= self.total_layers
    }
    /// A single peer covering the whole model runs without hidden-state
    /// extraction, which is what `--single-peer` controls.
    fn single_peer(&self) -> bool {
        self.peer_count <= 1 && self.is_first() && self.is_last()
    }
}

// ── Job tracking ─────────────────────────────────────────────────────────────
//
// Slicing a multi-GB model takes minutes, far longer than an HTTP request
// should hold open, so `apply` returns a job id the UI polls.

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    /// "running" | "done" | "failed"
    pub state: &'static str,
    pub step: String,
    pub message: String,
    pub started_at: String,
    pub finished_at: Option<String>,
}

fn jobs() -> &'static RwLock<HashMap<String, Job>> {
    static JOBS: std::sync::OnceLock<RwLock<HashMap<String, Job>>> = std::sync::OnceLock::new();
    JOBS.get_or_init(|| RwLock::new(HashMap::new()))
}

async fn set_job(id: &str, state: &'static str, step: &str, message: &str) {
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(id) {
        job.state = state;
        job.step = step.to_string();
        job.message = message.to_string();
        if state != "running" {
            job.finished_at = Some(chrono::Utc::now().to_rfc3339());
        }
    }
}

async fn job_status(Path(id): Path<String>) -> Response {
    match jobs().read().await.get(&id) {
        Some(job) => Json(job.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "job desconhecido" })))
            .into_response(),
    }
}

/// Slice locally if needed, then reconfigure this node onto the result.
async fn apply(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ApplyRequest>,
) -> Response {
    if !std::path::Path::new(&req.source_model_path).exists() {
        return bad_request(format!(
            "modelo não encontrado neste peer: {}",
            req.source_model_path
        ));
    }
    if req.start_layer > req.end_layer {
        return bad_request("faixa de camadas invertida");
    }

    let id = uuid::Uuid::new_v4().simple().to_string();
    jobs().write().await.insert(
        id.clone(),
        Job {
            id: id.clone(),
            state: "running",
            step: "preparando".into(),
            message: String::new(),
            started_at: chrono::Utc::now().to_rfc3339(),
            finished_at: None,
        },
    );

    let job_id = id.clone();
    tokio::spawn(async move {
        match run_apply(state, req, &job_id).await {
            Ok(path) => {
                set_job(&job_id, "done", "pronto", &format!("servindo {path}")).await;
            }
            Err(e) => {
                warn!("apply falhou: {e:#}");
                set_job(&job_id, "failed", "erro", &format!("{e:#}")).await;
            }
        }
    });

    Json(json!({ "job_id": id })).into_response()
}

async fn run_apply(
    state: GatewayState,
    req: ApplyRequest,
    job_id: &str,
) -> anyhow::Result<String> {
    let cwd = state.runtime.cwd().clone();

    let model_path = if req.skip_split || (req.is_first() && req.is_last() && req.peer_count <= 1) {
        // Nothing to carve out: this peer serves the file as it is.
        req.source_model_path.clone()
    } else {
        set_job(
            job_id,
            "running",
            "fatiando",
            &format!(
                "extraindo camadas {}-{} (pode levar alguns minutos)",
                req.start_layer, req.end_layer
            ),
        )
        .await;

        let python = python_exe(&cwd);
        let out_dir = plan::shard_output_dir(&cwd, &req.base_name);
        let outcome = plan::extract_shard(
            &cwd,
            &python,
            &req.source_model_path,
            &out_dir,
            &req.base_name,
            req.start_layer,
            req.end_layer,
            req.is_first(),
            req.is_last(),
        )
        .await?;

        info!(
            "shard pronto: {} ({:.1} GB, {})",
            outcome.output_path,
            outcome.bytes as f64 / 1_073_741_824.0,
            if outcome.reused { "reaproveitado" } else { "gerado" }
        );
        outcome.output_path
    };

    set_job(job_id, "running", "carregando", "iniciando servidor de inferência").await;

    // The label must be derived from the *shared* base name, not the local
    // filename, so peers holding different quantisations still agree.
    let label = crate::model::ModelProfile::guess_from_name(&req.base_name).name;

    let inference_port = state
        .network
        .inference_port
        .ok_or_else(|| anyhow::anyhow!("este nó subiu sem --inference-port"))?;

    let cfg = NodeConfig {
        model_path: model_path.clone(),
        model_label: label,
        start_layer: req.start_layer,
        end_layer: req.end_layer,
        total_layers: req.total_layers,
        peer_count: req.peer_count.max(1),
        peer_index: req.peer_index,
        primary: req.primary,
        inference_port,
        n_gpu_layers: req.params.n_gpu_layers,
        n_ctx: req.params.n_ctx,
        n_batch: req.params.n_batch,
        max_new_tokens: req.params.max_new_tokens,
        max_sessions: req.params.max_sessions,
        temperature: req.params.temperature,
        top_p: req.params.top_p,
        top_k: req.params.top_k,
        repeat_penalty: req.params.repeat_penalty,
        repeat_last_n: req.params.repeat_last_n,
        no_thinking: req.params.no_thinking,
        single_peer: req.single_peer(),
    };

    state.runtime.apply(cfg).await?;
    Ok(model_path)
}

fn python_exe(cwd: &std::path::Path) -> String {
    let venv = cwd.join("venv/bin/python3");
    if venv.exists() {
        venv.to_string_lossy().into_owned()
    } else {
        "python3".to_string()
    }
}

// ── Cluster launch ───────────────────────────────────────────────────────────

/// One entry per peer taking part in the launch.
#[derive(Debug, Deserialize)]
pub struct LaunchTarget {
    pub node_id: String,
    /// Path of the source model *on that peer*.
    pub source_model_path: String,
    pub start_layer: u32,
    pub end_layer: u32,
    #[serde(default)]
    pub skip_split: bool,
}

#[derive(Debug, Deserialize)]
pub struct LaunchRequest {
    pub base_name: String,
    pub total_layers: u32,
    pub targets: Vec<LaunchTarget>,
    /// Node that should act as the entry point.
    #[serde(default)]
    pub primary_node_id: Option<String>,
    #[serde(default)]
    pub params: RuntimeParams,
}

/// Dispatch the planned configuration to every peer, including this one.
async fn launch(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<LaunchRequest>,
) -> Response {
    if req.targets.is_empty() {
        return bad_request("nenhum peer selecionado");
    }

    let peers = state.all_peers().await;
    let self_id = state.network.node_id.clone();
    let peer_count = req.targets.len() as u32;
    let mut results = Vec::new();

    for (index, target) in req.targets.iter().enumerate() {
        let apply_req = ApplyRequest {
            source_model_path: target.source_model_path.clone(),
            base_name: req.base_name.clone(),
            start_layer: target.start_layer,
            end_layer: target.end_layer,
            total_layers: req.total_layers,
            peer_count,
            peer_index: index as u32,
            primary: req.primary_node_id.as_deref() == Some(target.node_id.as_str()),
            skip_split: target.skip_split,
            params: req.params.clone(),
        };

        if target.node_id == self_id {
            let resp = apply(ConnectionAuth, State(state.clone()), Json(apply_req)).await;
            results.push(json!({
                "node_id": target.node_id,
                "dispatched": resp.status().is_success(),
                "local": true,
            }));
            continue;
        }

        let Some(base) = peers
            .iter()
            .find(|p| p.node_id == target.node_id)
            .and_then(|p| p.gateway_url())
        else {
            results.push(json!({
                "node_id": target.node_id,
                "dispatched": false,
                "error": "peer não encontrado ou sem porta de gateway anunciada",
            }));
            continue;
        };

        // The peer's /api/ui/apply is behind ConnectionAuth. Every node in a
        // cluster shares the same connection key, so forward ours — without it
        // the remote leg of a launch always came back 401.
        let url = format!("{base}/api/ui/apply");
        let key = state.auth.read().await.connection_key.clone();
        match state
            .http
            .post(&url)
            .bearer_auth(&key)
            .json(&apply_req)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or(json!({}));
                results.push(json!({
                    "node_id": target.node_id,
                    "dispatched": true,
                    "job_id": body.get("job_id"),
                }));
            }
            Ok(r) => {
                let code = r.status();
                let body = r.text().await.unwrap_or_default();
                results.push(json!({
                    "node_id": target.node_id,
                    "dispatched": false,
                    "error": format!("HTTP {code}: {body}"),
                }));
            }
            Err(e) => results.push(json!({
                "node_id": target.node_id,
                "dispatched": false,
                "error": e.to_string(),
            })),
        }
    }

    Json(json!({ "results": results })).into_response()
}

// ── Setup API ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SetupKeyResponse {
    pub connection_key: String,
    pub addresses: Vec<String>,
}

pub async fn get_setup_key(
    State(config): State<AuthState>,
) -> impl IntoResponse {
    let raw = format!("{}-{}", uuid::Uuid::new_v4(), std::process::id());
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let key = hex::encode(hasher.finalize());

    {
        let mut cfg = config.write().await;
        cfg.connection_key = key.clone();
    }

    let mut addresses = Vec::new();
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if let Ok(()) = socket.connect("8.8.8.8:80") {
            if let Ok(addr) = socket.local_addr() {
                addresses.push(format!("IPv4: {}:42777", addr.ip()));
            }
        }
    }

    Json(SetupKeyResponse {
        connection_key: key,
        addresses,
    })
}

#[derive(Deserialize)]
pub struct SaveSetupRequest {
    pub role: String,
    #[serde(default)]
    pub bootstrap: String,
    #[serde(default)]
    pub connection_key: String,
}

pub async fn save_setup(
    State(config): State<AuthState>,
    Json(req): Json<SaveSetupRequest>,
) -> impl IntoResponse {
    let role = if req.role == "principal" {
        Role::Principal
    } else {
        Role::Node
    };

    let mut cfg = config.write().await;
    cfg.role = role.clone();

    if role == Role::Node {
        if !req.connection_key.is_empty() {
            cfg.connection_key = req.connection_key.clone();
        }
        if !req.bootstrap.is_empty() {
            cfg.bootstrap_nodes = req.bootstrap.split(',').map(|s| s.trim().to_string()).collect();
        }
    }

    match cfg.save() {
        Ok(()) => {
            // Trigger restart: spawn a new instance and exit.
            // The new instance will find the config file and start in full mode.
            let gateway_port = cfg.gateway_port;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("rdma-p2p"));
                let _ = std::process::Command::new(&exe)
                    .arg("--gateway-port")
                    .arg(gateway_port.to_string())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                std::process::exit(0);
            });
            Json(json!({"saved": true, "restart": true, "message": "Configuracao salva. Reiniciando..."})).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()}))
        ).into_response(),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn bad_request(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": message.into() })),
    )
        .into_response()
}

/// Unused query extractor placeholder kept out of the router; documented here
/// so future filters have an obvious home.
#[allow(dead_code)]
type UiQuery = Query<HashMap<String, String>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn req(start: u32, end: u32, total: u32, peers: u32) -> ApplyRequest {
        ApplyRequest {
            source_model_path: "/m.gguf".into(),
            base_name: "m".into(),
            start_layer: start,
            end_layer: end,
            total_layers: total,
            peer_count: peers,
            peer_index: 0,
            primary: true,
            skip_split: false,
            params: RuntimeParams::default(),
        }
    }

    #[test]
    fn first_and_last_follow_the_layer_range() {
        let r = req(0, 15, 32, 2);
        assert!(r.is_first());
        assert!(!r.is_last());

        let r = req(16, 31, 32, 2);
        assert!(!r.is_first());
        assert!(r.is_last());
    }

    #[test]
    fn single_peer_only_when_one_node_covers_everything() {
        assert!(req(0, 31, 32, 1).single_peer());
        assert!(!req(0, 15, 32, 2).single_peer());
        assert!(!req(0, 31, 32, 2).single_peer());
    }

    #[test]
    fn params_default_to_the_validated_cluster_settings() {
        let p: RuntimeParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.n_gpu_layers, 99);
        assert_eq!(p.n_ctx, 8192);
        assert_eq!(p.n_batch, 512);
        assert!(p.no_thinking);
    }

    #[test]
    fn params_accept_partial_overrides() {
        let p: RuntimeParams = serde_json::from_str(r#"{"n_ctx":4096,"no_thinking":false}"#).unwrap();
        assert_eq!(p.n_ctx, 4096);
        assert!(!p.no_thinking);
        assert_eq!(p.top_k, 40, "campos ausentes mantêm o padrão");
    }
}
