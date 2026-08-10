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
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

/// How long a scanned catalog is served before a refresh is started behind it.
const CATALOG_TTL: Duration = Duration::from_secs(20);
/// Timeout for one peer-to-peer control call.
pub(crate) const PEER_TIMEOUT: Duration = Duration::from_secs(4);

pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/node", get(node_info))
        .route("/api/ui/models", get(models))
        .route("/api/ui/cluster", get(cluster))
        .route("/api/ui/cluster/models", get(cluster_models))
        .route("/api/ui/paths", get(get_paths).post(save_paths))
        .route("/api/ui/plan", post(plan_split_endpoint))
        .route("/api/ui/apply", post(apply))
        .route("/api/ui/launch", post(launch))
        .route("/api/ui/stop-inference", post(stop_inference))
        .route("/api/ui/slice", post(slice_locally))
        .route("/api/ui/slice/plan", post(slice_plan))
        .route("/api/ui/jobs", get(list_jobs))
        .route("/api/ui/jobs/:id", get(job_status))
        .route("/api/ui/unload", post(unload_model))
        .route("/api/ui/release", post(release_slice))
        .route("/api/ui/release/candidate", get(release_candidate))
        .route("/api/ui/leave", post(leave_network))
        .route("/api/ui/rejoin", post(rejoin_network))
        .route("/api/ui/requests", get(crate::reqlog::list_requests))
    // `/api/setup/key` e `/api/setup/save` NAO entram aqui de proposito.
    //
    // Elas nao tem autenticacao - nao podem ter, e o momento em que a chave do
    // cluster nasce - e so fazem sentido antes de existir configuracao. Num no
    // ja configurado elas eram um portao aberto: `GET /api/setup/key` troca o
    // `connection_key` em memoria, entao qualquer um alcancando a porta do
    // gateway rotacionava a credencial de operador, derrubava a chave legitima
    // (403) e ficava com acesso total a /admin, /api/ui/apply e /v1.
    // Reproduzido em 2026-08-08 num no local; com o no atendendo IPv6 publico
    // isso deixou de ser teorico.
    //
    // Quem precisa delas e o modo setup, que monta o proprio roteador com estas
    // duas rotas e a setup.html (ver `start_setup_mode` em main.rs). O unico
    // consumidor e aquela pagina, servida so enquanto nao ha `rdma-p2p.json`.
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
    /// True while the operator has taken this node off the network.
    pub detached: bool,
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
        detached: state.network.is_detached(),
        system,
    })
}

// ── Model catalog ────────────────────────────────────────────────────────────

/// Directories one scan covered: the whole-model libraries and the slice folder.
type CatalogRoots = (Vec<std::path::PathBuf>, std::path::PathBuf);

struct CachedCatalog {
    at: Instant,
    /// Directories the cached scan covered. Point the configuration somewhere
    /// else and the old result stops being an answer to the new question.
    roots: CatalogRoots,
    catalog: Arc<Catalog>,
    /// A refresh is already running; a second one would only duplicate the work.
    refreshing: bool,
}

fn catalog_cache() -> &'static Mutex<Option<CachedCatalog>> {
    static CACHE: std::sync::OnceLock<Mutex<Option<CachedCatalog>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Walk the directories. Blocking work, kept off the reactor.
async fn scan_catalog(roots: &CatalogRoots) -> Catalog {
    let (dirs, shards) = (roots.0.clone(), roots.1.clone());
    tokio::task::spawn_blocking(move || catalog::discover(&dirs, &shards))
        .await
        .unwrap_or_else(|e| {
            warn!("varredura de modelos falhou: {e}");
            Catalog {
                models: Vec::new(),
                shard_sets: Vec::new(),
                scanned: Vec::new(),
            }
        })
}

/// Rescan in the background and publish the result, unless the configuration
/// moved on in the meantime.
fn spawn_refresh(roots: CatalogRoots) {
    tokio::spawn(async move {
        let catalog = Arc::new(scan_catalog(&roots).await);
        let mut guard = catalog_cache().lock().await;
        let superseded = guard.as_ref().is_some_and(|c| c.roots != roots);
        if superseded {
            return;
        }
        *guard = Some(CachedCatalog {
            at: Instant::now(),
            roots,
            catalog,
            refreshing: false,
        });
    });
}

/// Ask for a fresh walk without throwing away what we can already answer with —
/// after slicing, after the directories change. A slice that exists must not
/// need a restart to show up.
async fn invalidate_catalog() {
    let roots = {
        let mut guard = catalog_cache().lock().await;
        match guard.as_mut() {
            Some(c) if !c.refreshing => {
                c.refreshing = true;
                Some(c.roots.clone())
            }
            _ => None,
        }
    };
    if let Some(roots) = roots {
        spawn_refresh(roots);
    }
}

/// The catalog of this machine, served from cache while it is refreshed.
///
/// Scanning a slow volume is not cheap — the captain's SD card full of models
/// takes about two minutes the first time — and the screen polls this. Handing
/// back the previous answer and refreshing behind it keeps the list on screen
/// instead of blanking it every time the cache turns stale. Only the very first
/// scan, and a change of directories, have nothing valid to serve and wait.
async fn local_catalog(state: &GatewayState) -> Arc<Catalog> {
    let roots: CatalogRoots = (state.model_dirs().await, state.shard_dir().await);

    {
        let mut guard = catalog_cache().lock().await;
        if let Some(c) = guard.as_mut() {
            if c.roots == roots {
                let stale = c.at.elapsed() >= CATALOG_TTL;
                if stale && !c.refreshing {
                    c.refreshing = true;
                    spawn_refresh(roots.clone());
                }
                return c.catalog.clone();
            }
        }
    }

    let catalog = Arc::new(scan_catalog(&roots).await);
    *catalog_cache().lock().await = Some(CachedCatalog {
        at: Instant::now(),
        roots,
        catalog: catalog.clone(),
        refreshing: false,
    });
    catalog
}

// ── Model directories ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct PathEntry {
    path: String,
    exists: bool,
    /// "ollama" | "lmstudio" | "folder" — what the scan will make of it.
    kind: &'static str,
}

fn path_entry(dir: &std::path::Path) -> PathEntry {
    PathEntry {
        path: dir.to_string_lossy().into_owned(),
        exists: dir.is_dir(),
        kind: match catalog::detect_source(dir) {
            catalog::ModelSource::Ollama => "ollama",
            catalog::ModelSource::LmStudio => "lmstudio",
            _ => "folder",
        },
    }
}

/// Where this node looks for models and where it writes slices.
async fn get_paths(State(state): State<GatewayState>) -> impl IntoResponse {
    let cfg = state.auth.read().await;
    let shard = cfg.shard_dir_path();
    Json(json!({
        "model_dirs": cfg.model_dir_paths().iter().map(|d| path_entry(d)).collect::<Vec<_>>(),
        "shard_dir": path_entry(&shard),
        "using_defaults": cfg.model_dirs_are_default(),
        "detected": catalog::default_model_dirs()
            .iter().map(|d| path_entry(d)).collect::<Vec<_>>(),
        "default_shard_dir": crate::config::default_shard_dir().to_string_lossy(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct SavePathsRequest {
    pub model_dirs: Vec<String>,
    /// Empty means "back to `<project_root>/models`".
    #[serde(default)]
    pub shard_dir: String,
}

/// Refuse anything that is not a directory sitting on this machine right now.
///
/// A typo here is not a small thing: the node would advertise a library it does
/// not have, and the mistake would only surface much later, as a peer that
/// cannot find the model it was told to slice.
fn check_dir(raw: &str, what: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{what}: caminho vazio."));
    }
    let path = std::path::Path::new(trimmed);
    if !path.exists() {
        return Err(format!("{what}: “{trimmed}” não existe nesta máquina."));
    }
    if !path.is_dir() {
        return Err(format!("{what}: “{trimmed}” é um arquivo, e aqui se informa a pasta que o contém."));
    }
    Ok(trimmed.to_string())
}

async fn save_paths(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SavePathsRequest>,
) -> Response {
    let mut model_dirs = Vec::new();
    for raw in &req.model_dirs {
        match check_dir(raw, "Pasta de modelos inteiros") {
            Ok(p) => {
                if !model_dirs.contains(&p) {
                    model_dirs.push(p);
                }
            }
            Err(e) => return bad_request(e),
        }
    }

    let shard_dir = if req.shard_dir.trim().is_empty() {
        None
    } else {
        match check_dir(&req.shard_dir, "Pasta de modelos fatiados") {
            Ok(p) => Some(p),
            Err(e) => return bad_request(e),
        }
    };

    {
        let mut cfg = state.auth.write().await;
        cfg.model_dirs = Some(model_dirs);
        cfg.shard_dir = shard_dir;
        if let Err(e) = cfg.save() {
            return bad_request(format!("não deu para gravar rdma-p2p.json: {e}"));
        }
    }

    invalidate_catalog().await;
    info!("pastas de modelos atualizadas pela interface");
    get_paths(State(state)).await.into_response()
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
    /// Prompt tokens per round trip through the pipeline.
    #[serde(default = "d_prefill_chunk")]
    pub prefill_chunk_tokens: u32,
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
    /// Disable embedding extraction in llama.cpp context on this peer.
    /// Auto-set for the last peer of a multi-peer pipeline when true.
    #[serde(default)]
    pub no_embeddings: bool,
}

fn d_gpu_layers() -> u32 { 99 }
fn d_n_ctx() -> u32 { 8192 }
fn d_n_batch() -> u32 { 512 }
fn d_max_new() -> u32 { 256 }
fn d_max_sessions() -> u32 { 1 }
fn d_prefill_chunk() -> u32 { crate::prefill::DEFAULT_CHUNK_TOKENS }
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
            prefill_chunk_tokens: d_prefill_chunk(),
            temperature: d_temp(),
            top_p: d_top_p(),
            top_k: d_top_k(),
            repeat_penalty: d_repeat_penalty(),
            repeat_last_n: d_repeat_last_n(),
            no_thinking: d_true(),
            no_embeddings: false,
        }
    }
}

/// Carry a running node's knobs over to whoever takes its slice.
///
/// A handover that quietly reset the context window or the sampling would change
/// the model's behaviour halfway through a session, which is worse than the
/// handover failing outright.
impl From<&NodeConfig> for RuntimeParams {
    fn from(c: &NodeConfig) -> Self {
        Self {
            n_gpu_layers: c.n_gpu_layers,
            n_ctx: c.n_ctx,
            n_batch: c.n_batch,
            max_new_tokens: c.max_new_tokens,
            max_sessions: c.max_sessions,
            prefill_chunk_tokens: c.prefill_chunk_tokens,
            temperature: c.temperature,
            top_p: c.top_p,
            top_k: c.top_k,
            repeat_penalty: c.repeat_penalty,
            repeat_last_n: c.repeat_last_n,
            no_thinking: c.no_thinking,
            no_embeddings: c.no_embeddings,
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
    fn is_last_peer(&self) -> bool {
        self.peer_index + 1 >= self.peer_count
    }
    /// A single peer covering the whole model runs without hidden-state
    /// extraction, which is what `--single-peer` controls.
    fn single_peer(&self) -> bool {
        self.peer_count <= 1 && self.is_first() && self.is_last()
    }
    /// When checked in the UI, force no_embeddings on every peer (dangerous —
    /// the first peer won't be able to extract hidden states). When unchecked,
    /// auto-detect: only the last peer in a multi-peer pipeline skips embeddings.
    fn no_embeddings(&self) -> bool {
        if self.params.no_embeddings {
            true
        } else {
            !self.single_peer() && self.is_last_peer()
        }
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
    /// "slice" — cutting a model here — or "apply" — binding this node to a
    /// range. The notification list says which, because one is the operator's
    /// own doing and the other can arrive from a peer's launch.
    pub kind: &'static str,
    /// One line naming the work, for a list that has no room for the detail.
    pub title: String,
    pub step: String,
    pub message: String,
    /// 0..100 while `split_model.py` reports it; `None` before the first line
    /// arrives, which the screen shows as an indeterminate bar rather than a
    /// zero that looks stuck.
    pub progress: Option<f64>,
    /// Files the job has produced so far, in order.
    pub outputs: Vec<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

pub(crate) fn jobs() -> &'static RwLock<HashMap<String, Job>> {
    static JOBS: std::sync::OnceLock<RwLock<HashMap<String, Job>>> = std::sync::OnceLock::new();
    JOBS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub(crate) fn new_job(id: &str, kind: &'static str, title: impl Into<String>, step: &str) -> Job {
    Job {
        id: id.to_string(),
        state: "running",
        kind,
        title: title.into(),
        step: step.to_string(),
        message: String::new(),
        progress: None,
        outputs: Vec::new(),
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: None,
    }
}

/// How long a finished job stays listed before it is dropped.
///
/// Long enough that an operator who was on another screen still sees "pronto"
/// when they come back; short enough that yesterday's runs are not news.
const JOB_RETENTION: Duration = Duration::from_secs(600);

/// Every job this node knows about, newest first.
///
/// This is what makes the notification menu survive a reload: the work lives
/// here, on the node running it, and the browser only ever asks what is going
/// on. Finished jobs are pruned as the list is built, which is the only moment
/// anybody looks at them.
async fn list_jobs() -> Response {
    let cutoff = chrono::Utc::now() - chrono::Duration::from_std(JOB_RETENTION).unwrap();
    let mut map = jobs().write().await;
    map.retain(|_, job| {
        job.state == "running"
            || job
                .finished_at
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .is_none_or(|t| t.with_timezone(&chrono::Utc) > cutoff)
    });

    let mut list: Vec<Job> = map.values().cloned().collect();
    list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Json(json!({ "jobs": list })).into_response()
}

pub(crate) async fn set_job(id: &str, state: &'static str, step: &str, message: &str) {
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(id) {
        job.state = state;
        job.step = step.to_string();
        job.message = message.to_string();
        if state != "running" {
            job.finished_at = Some(chrono::Utc::now().to_rfc3339());
            // Finished is finished: leaving a bar at 87% on a job that is done
            // reads as a stall.
            if state == "done" {
                job.progress = Some(100.0);
            }
        }
    }
}

/// Progress only ever moves forward.
///
/// Each reported percentage is delivered by its own task, so a line from the
/// slice that just finished can land after the next one has started and drag
/// the bar backwards — which reads as the work being redone.
pub(crate) async fn set_job_progress(id: &str, pct: f64) {
    let pct = pct.clamp(0.0, 100.0);
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(id) {
        if job.progress.map_or(true, |current| pct > current) {
            job.progress = Some(pct);
        }
    }
}

/// Progress reported by the slicer, mapped onto the whole job: part `index` of
/// `total` running at `pct`.
fn overall_progress(index: u32, total: u32, pct: f64) -> f64 {
    let total = total.max(1) as f64;
    ((index as f64 + pct / 100.0) / total * 100.0).clamp(0.0, 100.0)
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
    let title = format!(
        "Carregando {} · camadas {}–{}",
        req.base_name, req.start_layer, req.end_layer
    );
    jobs()
        .write()
        .await
        .insert(id.clone(), new_job(&id, "apply", title, "preparando"));

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
        let out_dir = plan::shard_output_dir(&state.shard_dir().await, &req.base_name);
        let progress_job = job_id.to_string();
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
            move |pct| {
                let id = progress_job.clone();
                tokio::spawn(async move { set_job_progress(&id, pct).await });
            },
        )
        .await?;
        invalidate_catalog().await;

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
        prefill_chunk_tokens: req.params.prefill_chunk_tokens,
        temperature: req.params.temperature,
        top_p: req.params.top_p,
        top_k: req.params.top_k,
        repeat_penalty: req.params.repeat_penalty,
        repeat_last_n: req.params.repeat_last_n,
        no_thinking: req.params.no_thinking,
        single_peer: req.single_peer(),
        no_embeddings: req.no_embeddings(),
    };

    state.runtime.apply(cfg).await?;
    Ok(model_path)
}

// ── Slicing a whole model, here, on demand ───────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SliceRequest {
    /// Whole model on *this* machine.
    pub model_path: String,
    /// Target size of each slice, in order. A slice with no size takes what is
    /// left of the model.
    pub slices: Vec<plan::SizedSlice>,
    /// Catalog name of the model, used to name the slices. Without it the name
    /// comes from the file, which for an Ollama blob is a digest.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SlicePlanRequest {
    pub model_path: String,
    #[serde(default)]
    pub slices: Vec<plan::SizedSlice>,
    /// Shortcut: fill the list with `n` slices of equal layer count, each
    /// carrying its real size as the target.
    #[serde(default)]
    pub equal_parts: Option<u32>,
}

/// Read the tensor table of a local model, once per file version.
///
/// The builder screen asks for a preview on every keystroke; re-reading the
/// header each time would make typing a size feel heavy.
async fn layout_of(path: &std::path::Path) -> Result<Arc<crate::gguf::GgufLayout>, String> {
    type Key = (String, u64, u64);
    static CACHE: std::sync::OnceLock<Mutex<HashMap<Key, Arc<crate::gguf::GgufLayout>>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("modelo não encontrado nesta máquina: {e}"))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = (path.to_string_lossy().into_owned(), meta.len(), mtime);

    if let Some(hit) = cache.lock().await.get(&key).cloned() {
        return Ok(hit);
    }

    let p = path.to_path_buf();
    let layout = match tokio::task::spawn_blocking(move || gguf::read_layout(&p)).await {
        Ok(Ok(l)) => Arc::new(l),
        Ok(Err(e)) => return Err(format!("não foi possível ler o GGUF: {e}")),
        Err(e) => return Err(format!("leitura do GGUF falhou: {e}")),
    };
    cache.lock().await.insert(key, layout.clone());
    Ok(layout)
}

/// What a list of target sizes turns into: layer ranges, real sizes, coverage.
async fn slice_plan(
    _auth: ConnectionAuth,
    Json(req): Json<SlicePlanRequest>,
) -> Response {
    let layout = match layout_of(std::path::Path::new(&req.model_path)).await {
        Ok(l) => l,
        Err(e) => return bad_request(e),
    };
    let planned = match req.equal_parts {
        Some(n) if n >= 1 && n <= 64 => plan::plan_equal(&layout, n),
        Some(_) => return bad_request("escolha entre 1 e 64 fatias"),
        None => plan::plan_by_size(&layout, &req.slices),
    };
    match planned {
        Ok(p) => Json(p).into_response(),
        Err(e) => bad_request(e.to_string()),
    }
}

/// Cut a local model into `parts` slices, in the background.
///
/// Nothing about the split itself is new: it is the same `split_model.py` the
/// cluster launch already runs, one range at a time, writing into the
/// configured slice directory. What is new is that the operator can ask for it
/// without planning a whole cluster, and can watch it happen.
async fn slice_locally(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SliceRequest>,
) -> Response {
    let source = std::path::PathBuf::from(&req.model_path);
    if !source.is_file() {
        return bad_request(format!("modelo não encontrado nesta máquina: {}", req.model_path));
    }
    if req.slices.is_empty() {
        return bad_request("nenhuma fatia na lista");
    }
    if req.slices.len() > 64 {
        return bad_request("no máximo 64 fatias");
    }

    let layout = match layout_of(&source).await {
        Ok(l) => l,
        Err(e) => return bad_request(e),
    };

    // The screen previews with the same function, but the decision is remade
    // here: a stale preview must not become a slicing run nobody approved.
    let sized = match plan::plan_by_size(&layout, &req.slices) {
        Ok(p) => p,
        Err(e) => return bad_request(e.to_string()),
    };
    if sized.blocked {
        return bad_request(
            sized
                .warnings
                .first()
                .cloned()
                .unwrap_or_else(|| "há uma fatia impossível na lista".to_string()),
        );
    }

    let total_layers = sized.total_layers;
    let ranges: Vec<(u32, u32)> = sized
        .ranges
        .iter()
        .filter(|r| !r.empty)
        .map(|r| (r.start_layer, r.end_layer))
        .collect();
    if ranges.is_empty() {
        return bad_request("este arquivo não declara camadas — não há o que fatiar");
    }

    let base_name = req
        .name
        .as_deref()
        .and_then(plan::base_name_from_label)
        .unwrap_or_else(|| plan::base_name_of(&req.model_path));
    let out_dir = plan::shard_output_dir(&state.shard_dir().await, &base_name);
    let id = uuid::Uuid::new_v4().simple().to_string();
    let title = format!(
        "Fatiando {base_name} em {} {}",
        ranges.len(),
        if ranges.len() == 1 { "fatia" } else { "fatias" }
    );
    jobs()
        .write()
        .await
        .insert(id.clone(), new_job(&id, "slice", title, "preparando"));

    let job_id = id.clone();
    tokio::spawn(async move {
        match run_slice(state, source, out_dir, base_name, ranges, total_layers, &job_id).await {
            Ok(files) => {
                let msg = format!(
                    "{} fatia{} em {}",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" },
                    files
                        .first()
                        .and_then(|f| std::path::Path::new(f).parent().map(|p| p.display().to_string()))
                        .unwrap_or_default()
                );
                set_job(&job_id, "done", "pronto", &msg).await;
            }
            Err(e) => {
                warn!("fatiamento falhou: {e:#}");
                set_job(&job_id, "failed", "erro", &format!("{e:#}")).await;
            }
        }
    });

    Json(json!({ "job_id": id })).into_response()
}

async fn run_slice(
    state: GatewayState,
    source: std::path::PathBuf,
    out_dir: std::path::PathBuf,
    base_name: String,
    ranges: Vec<(u32, u32)>,
    total_layers: u32,
    job_id: &str,
) -> anyhow::Result<Vec<String>> {
    let cwd = state.runtime.cwd().clone();
    let python = python_exe(&cwd);
    let parts = ranges.len() as u32;
    let mut outputs = Vec::new();

    for (index, (start, end)) in ranges.iter().copied().enumerate() {
        let index = index as u32;
        // Reading the source and packing the new header comes before a single
        // byte of weights is written, and on a model with a large tokenizer it
        // is most of the wall time. Saying which phase we are in beats a bar
        // that sits still with no explanation.
        let label = format!(
            "fatia {}/{} · camadas {start}–{end} de {total_layers}",
            index + 1,
            parts
        );
        set_job(job_id, "running", "lendo", &format!("{label} · lendo o modelo")).await;

        let progress_job = job_id.to_string();
        let writing_label = label.clone();
        let first_report = Arc::new(AtomicBool::new(true));
        let outcome = plan::extract_shard(
            &cwd,
            &python,
            &source.to_string_lossy(),
            &out_dir,
            &base_name,
            start,
            end,
            start == 0,
            end + 1 >= total_layers,
            move |pct| {
                let id = progress_job.clone();
                let overall = overall_progress(index, parts, pct);
                let announce = first_report
                    .swap(false, Ordering::Relaxed)
                    .then(|| format!("{writing_label} · gravando"));
                tokio::spawn(async move {
                    if let Some(message) = announce {
                        set_job(&id, "running", "gravando", &message).await;
                    }
                    set_job_progress(&id, overall).await;
                });
            },
        )
        .await?;

        set_job_progress(job_id, overall_progress(index + 1, parts, 0.0)).await;
        info!(
            "fatia {}/{parts} pronta: {} ({:.1} GB, {})",
            index + 1,
            outcome.output_path,
            outcome.bytes as f64 / 1_073_741_824.0,
            if outcome.reused { "reaproveitada" } else { "gerada" }
        );
        outputs.push(outcome.output_path.clone());

        // Show up in the catalog as each one lands, not only at the end.
        invalidate_catalog().await;
        let mut map = jobs().write().await;
        if let Some(job) = map.get_mut(job_id) {
            job.outputs.push(outcome.output_path);
        }
    }

    Ok(outputs)
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

/// Stop the inference server on this node, announce the node as idle,
/// and free the inference port. Called by `launch` before reconfiguring
/// peers so no stale endpoint receives inference requests mid-transition.
async fn stop_inference(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    info!("stop-inference recebido — parando servidor neste nó");
    state.runtime.stop().await;
    Json(json!({ "stopped": true })).into_response()
}

// ── Taking a node out of service ─────────────────────────────────────────────
//
// Two different things the operator asks for, deliberately separate routes even
// though one is currently a subset of the other: unloading frees this machine's
// memory and leaves it in the cluster, ready to be given a range again; leaving
// takes the machine off the network entirely.

/// Free this machine's memory: kill the inference server, forget the range, and
/// tell the peers this node serves nothing.
///
/// Mechanically the same work `stop-inference` does during a launch, but that
/// route exists for the launch orchestration and says nothing back about what
/// was released — which is the whole answer the operator is waiting for here.
async fn unload_model(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let unloaded = state.runtime.config().await;
    if unloaded.is_none() {
        return Json(json!({ "unloaded": false, "reason": "nenhum modelo carregado" }))
            .into_response();
    }
    state.runtime.stop().await;
    info!("modelo descarregado pela interface");
    Json(json!({
        "unloaded": true,
        "model": unloaded.as_ref().map(|c| c.model_label.clone()),
        "start_layer": unloaded.as_ref().map(|c| c.start_layer),
        "end_layer": unloaded.as_ref().map(|c| c.end_layer),
    }))
    .into_response()
}

/// Take this node off the network: unload, stop announcing, forget the peers.
///
/// The HTTP gateway keeps serving, because the way back in is this same screen.
async fn leave_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    state.runtime.stop().await;
    state.network.detach().await;
    info!("nó saiu da rede pela interface");
    Json(json!({ "detached": true })).into_response()
}

// ── Releasing a slice, and the network taking it back up ─────────────────────

/// What the operator asked for when releasing.
#[derive(Debug, Deserialize)]
pub struct ReleaseRequest {
    /// Try to hand the slice to another node afterwards. On by default; the
    /// operator can turn it off to free the memory and leave the range down.
    #[serde(default = "default_true")]
    pub reassign: bool,
}

fn default_true() -> bool {
    true
}

/// A node that could take over a slice: idle, and already holding the file.
struct Candidate {
    node_id: String,
    hostname: String,
    gateway: String,
    /// Where that same slice lives on *that* peer's disk.
    path: String,
}

/// Find a peer that is serving nothing and already has this exact slice.
///
/// "Already has it" is deliberate: the point of releasing is that this machine
/// needs its memory back *now*, so the handover has to be something another node
/// can act on immediately. Copying a 10 GB slice across the link first would take
/// ~17 minutes, which is not a handover — that is what the deploy screen is for.
///
/// Matching is by layer range plus shard group, not by file name: the same slice
/// legitimately sits under different paths on different machines.
async fn find_release_candidate(state: &GatewayState, cfg: &NodeConfig) -> Option<Candidate> {
    let self_id = state.network.node_id.clone();
    let key = state.auth.read().await.connection_key.clone();

    for peer in state.all_peers().await {
        if peer.node_id == self_id {
            continue;
        }
        let Some(base) = peer.gateway_url() else { continue };

        // Busy peers are out: a node serves one range at a time.
        match state
            .http
            .get(format!("{base}/api/ui/node"))
            .bearer_auth(&key)
            .timeout(PEER_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or(json!({}));
                if !body.get("config").map(|c| c.is_null()).unwrap_or(true) {
                    continue;
                }
            }
            _ => continue,
        }

        let Ok(r) = state
            .http
            .get(format!("{base}/api/ui/models"))
            .timeout(PEER_TIMEOUT * 3)
            .send()
            .await
        else {
            continue;
        };
        if !r.status().is_success() {
            continue;
        }
        let body: serde_json::Value = r.json().await.unwrap_or(json!({}));

        let found = body
            .get("shard_sets")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .flat_map(|set| set.get("shards").and_then(|s| s.as_array()).into_iter().flatten())
            .find(|shard| {
                let range = shard.get("shard_range").and_then(|r| r.as_array());
                let matches_range = range
                    .map(|r| {
                        r.first().and_then(|v| v.as_u64()) == Some(cfg.start_layer as u64)
                            && r.get(1).and_then(|v| v.as_u64()) == Some(cfg.end_layer as u64)
                    })
                    .unwrap_or(false);
                matches_range
                    && shard
                        .get("total_layers")
                        .and_then(|v| v.as_u64())
                        .map(|t| t == cfg.total_layers as u64)
                        .unwrap_or(true)
            })
            .and_then(|shard| shard.get("path").and_then(|p| p.as_str()).map(String::from));

        if let Some(path) = found {
            return Some(Candidate {
                node_id: peer.node_id.clone(),
                hostname: peer.hostname.clone(),
                gateway: base,
                path,
            });
        }
    }
    None
}

/// Who would take this node's slice if it were released right now.
///
/// Read-only, and the reason the confirmation dialog can promise something true:
/// warning "the model will stop answering" only when it actually will beats a
/// blanket scare that the operator learns to click through.
async fn release_candidate(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let Some(cfg) = state.runtime.config().await else {
        return Json(json!({ "serving": false })).into_response();
    };
    let candidate = find_release_candidate(&state, &cfg).await;
    Json(json!({
        "serving": true,
        "slice": {
            "model": cfg.model_label,
            "start_layer": cfg.start_layer,
            "end_layer": cfg.end_layer,
            "total_layers": cfg.total_layers,
        },
        "candidate": candidate.map(|c| json!({
            "node_id": c.node_id,
            "hostname": c.hostname,
        })),
    }))
    .into_response()
}

/// Hand this node's slice back and free its RAM/VRAM, staying in the cluster.
///
/// The release is **never refused**, even when nobody can take the slice over.
/// That is the whole point of the feature: the machine needs its memory now, and
/// a node that has to ask permission to stop has not been given the freedom the
/// operator was promised. What the caller gets instead is a straight answer about
/// what the cluster looks like afterwards — `reassigned_to` when another node
/// picked the range up, or `uncovered: true` naming the range that is now down.
///
/// An uncovered range is not silent anywhere else either: the planner already
/// refuses to build a pipeline with a hole in it, so generation fails with the
/// gap named rather than falling back to a wrong answer.
async fn release_slice(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    body: Option<Json<ReleaseRequest>>,
) -> Response {
    let Some(cfg) = state.runtime.config().await else {
        return bad_request("este nó não está servindo nenhuma fatia");
    };
    let reassign = body.map(|Json(r)| r.reassign).unwrap_or(true);

    // Free the memory first, and unconditionally. Looking for a successor before
    // stopping would make the operator wait on the network for the one thing
    // they asked to happen immediately.
    state.runtime.stop().await;
    info!(
        "fatia L{}-{} liberada pelo nó; memória devolvida",
        cfg.start_layer, cfg.end_layer
    );

    let released = json!({
        "model": cfg.model_label,
        "start_layer": cfg.start_layer,
        "end_layer": cfg.end_layer,
        "total_layers": cfg.total_layers,
    });

    if !reassign {
        return Json(json!({
            "released": true, "slice": released,
            "reassigned_to": null, "uncovered": true,
            "reason": "reatribuição desligada no pedido",
        }))
        .into_response();
    }

    let Some(candidate) = find_release_candidate(&state, &cfg).await else {
        warn!(
            "fatia L{}-{} ficou descoberta: nenhum nó ocioso tem esse arquivo",
            cfg.start_layer, cfg.end_layer
        );
        return Json(json!({
            "released": true, "slice": released,
            "reassigned_to": null, "uncovered": true,
            "reason": "nenhum outro nó está ocioso e com esta fatia no disco",
        }))
        .into_response();
    };

    // Same shape a launch dispatches, so the successor comes up exactly as this
    // node was: same label, same position in the split, same runtime params.
    let apply_req = ApplyRequest {
        source_model_path: candidate.path.clone(),
        base_name: cfg.model_label.clone(),
        start_layer: cfg.start_layer,
        end_layer: cfg.end_layer,
        total_layers: cfg.total_layers,
        peer_count: cfg.peer_count,
        peer_index: cfg.peer_index,
        primary: cfg.primary,
        skip_split: true,
        params: RuntimeParams::from(&cfg),
    };

    let key = state.auth.read().await.connection_key.clone();
    match state
        .http
        .post(format!("{}/api/ui/apply", candidate.gateway))
        .bearer_auth(&key)
        .json(&apply_req)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!(
                "fatia L{}-{} assumida por {}",
                cfg.start_layer, cfg.end_layer, candidate.node_id
            );
            Json(json!({
                "released": true, "slice": released, "uncovered": false,
                "reassigned_to": {
                    "node_id": candidate.node_id,
                    "hostname": candidate.hostname,
                },
            }))
            .into_response()
        }
        Ok(r) => {
            let code = r.status();
            let text = r.text().await.unwrap_or_default();
            warn!("{} recusou assumir a fatia: HTTP {code}", candidate.node_id);
            Json(json!({
                "released": true, "slice": released,
                "reassigned_to": null, "uncovered": true,
                "reason": format!("{} recusou assumir: HTTP {code} {text}", candidate.hostname),
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "released": true, "slice": released,
            "reassigned_to": null, "uncovered": true,
            "reason": format!("{} inacessível: {e}", candidate.hostname),
        }))
        .into_response(),
    }
}

/// Announce again and go looking for the bootstrap nodes.
async fn rejoin_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let assignment = state.local_assignment.read().await.clone();
    match state.network.rejoin(assignment.as_ref()).await {
        Ok(()) => Json(json!({ "detached": false })).into_response(),
        Err(e) => bad_request(format!("não deu para voltar à rede: {e}")),
    }
}

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
pub(crate) async fn launch(
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

    // Phase 1: stop inference on every target peer so no stale endpoint
    // receives inference requests mid-transition, and to free ports before
    // the new inference servers try to bind.
    let key = state.auth.read().await.connection_key.clone();
    for target in &req.targets {
        if target.node_id == self_id {
            state.runtime.stop().await;
            continue;
        }
        let Some(base) = peers
            .iter()
            .find(|p| p.node_id == target.node_id)
            .and_then(|p| p.gateway_url())
        else {
            warn!("peer {} nao encontrado para stop-inference", target.node_id);
            continue;
        };
        let url = format!("{base}/api/ui/stop-inference");
        match state
            .http
            .post(&url)
            .bearer_auth(&key)
            .timeout(PEER_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                info!("peer {} parou inferencia", target.node_id);
            }
            Ok(r) => {
                warn!(
                    "peer {} recusou stop-inference: HTTP {}",
                    target.node_id,
                    r.status()
                );
            }
            Err(e) => {
                warn!("peer {} inacessivel para stop-inference: {e}", target.node_id);
            }
        }
    }

    // Brief settle: let the OS release ports.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 2: dispatch apply with the new configuration.
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
