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
use anyhow::Context;
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
        .route("/api/ui/models/delete", post(delete_model))
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
    /// Commit this binary was built from, so "which build is that peer running?"
    /// is answerable without a shell on the machine.
    pub build_commit: String,
    /// Este nó aceita atualizar-se pela interface. A tela só desenha o botão
    /// quando isto é verdade, para nunca oferecer o que vai dar 403.
    pub dev_update: bool,
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
        // Not `system.hostname`: that's the raw OS hostname, and ignores
        // `node_name` from rdma-p2p.json. `network.hostname` is what
        // `with_identity` (main.rs) actually announces to peers — the same
        // value `/admin/nodes` already shows correctly on the Network tab.
        hostname: state.network.hostname.clone(),
        build_commit: crate::BUILD_COMMIT.to_string(),
        dev_update: crate::devupdate::enabled(),
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

/// Walk the directories once at boot, so the first screen does not pay for it.
///
/// Measured on the captain's machine with the slices on an external volume: the
/// very first `/api/ui/models` took **11,9 s**, and the deploy screen's own
/// `/api/ui/cluster/models` took 13,2 s cold against 0,15 s once that scan had
/// happened — the fan-out to eleven peers was never the cost, the local walk
/// was. Nothing else changes: the same cache, the same TTL, the same
/// stale-while-revalidate. What moves is *when* the walk happens, from "the
/// first person to open a screen" to "while the node is still starting up".
///
/// Deliberately not awaited. A slow volume must not hold the gateway's port
/// closed, and a request that lands mid-walk simply waits on the same scan it
/// would have started itself.
pub fn warm_catalog(state: GatewayState) {
    tokio::spawn(async move {
        let started = Instant::now();
        let catalog = local_catalog(&state).await;
        info!(
            "catálogo pronto em {:.1}s — {} modelos, {} conjuntos de fatias",
            started.elapsed().as_secs_f32(),
            catalog.models.len(),
            catalog.shard_sets.len()
        );
    });
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
        return Err(format!(
            "{what}: “{trimmed}” é um arquivo, e aqui se informa a pasta que o contém."
        ));
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
    capabilities: Vec<routing_contract::ModelCapability>,
    inference_url: Option<String>,
    /// Endereco por onde este no fala com o peer, e a latencia que mediu.
    route: Option<RouteView>,
    gateway_url: Option<String>,
    load: u32,
    is_self: bool,
    /// Present when the peer answered `/api/ui/node`.
    node: Option<serde_json::Value>,
    /// Why the peer could not be reached, when it could not.
    unreachable: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RouteView {
    pub addr: String,
    pub rtt_ms: f64,
}

/// Rota escolhida para este peer, ou `None` quando ainda nao foi medida.
///
/// `None` e informacao: significa "ainda nao sei", e a tela diz isso em vez de
/// mostrar um endereco que ninguem confirmou.
pub fn route_view_for(node_id: &str, tabela: &crate::route::RouteTable) -> Option<RouteView> {
    tabela
        .snapshot()
        .into_iter()
        .find(|(id, _)| id == node_id)
        .map(|(_, escolha)| RouteView {
            addr: escolha.addr,
            rtt_ms: escolha.rtt_ms,
        })
}

#[derive(Debug, Serialize)]
struct RoutingEdgeView {
    from_node_id: String,
    to_node_id: String,
    model_id: String,
    from_end_layer: u32,
    to_start_layer: u32,
}

/// All viable handoffs observed in the model inventory. These are candidates,
/// not routing commands: the planner remains the sole authority selecting the
/// live path and balancing load.
fn candidate_edges(views: &[PeerView]) -> Vec<RoutingEdgeView> {
    let mut edges = Vec::new();
    for from in views {
        for from_capability in &from.capabilities {
            if !from_capability.supports_inference {
                continue;
            }
            for to in views {
                if to.node_id == from.node_id {
                    continue;
                }
                for to_capability in &to.capabilities {
                    if !to_capability.supports_inference
                        || to_capability.model_id != from_capability.model_id
                        || to_capability.end_layer <= from_capability.end_layer
                        || to_capability.start_layer > from_capability.end_layer.saturating_add(1)
                    {
                        continue;
                    }
                    edges.push(RoutingEdgeView {
                        from_node_id: from.node_id.clone(),
                        to_node_id: to.node_id.clone(),
                        model_id: from_capability.model_id.clone(),
                        from_end_layer: from_capability.end_layer,
                        to_start_layer: to_capability.start_layer,
                    });
                }
            }
        }
    }
    edges.sort_by(|left, right| {
        (
            &left.model_id,
            &left.from_node_id,
            &left.to_node_id,
            left.from_end_layer,
            left.to_start_layer,
        )
            .cmp(&(
                &right.model_id,
                &right.from_node_id,
                &right.to_node_id,
                right.from_end_layer,
                right.to_start_layer,
            ))
    });
    edges.dedup_by(|left, right| {
        left.model_id == right.model_id
            && left.from_node_id == right.from_node_id
            && left.to_node_id == right.to_node_id
            && left.from_end_layer == right.from_end_layer
            && left.to_start_layer == right.to_start_layer
    });
    edges
}

/// Coverage is calculated per advertised model, not just the model currently
/// selected for generation. The dashboard may observe every model without
/// affecting the routing decision.
#[derive(Debug, Serialize)]
struct ModelCoverageView {
    model_id: String,
    total_layers: u32,
    covered_layers: usize,
    coverage: Vec<bool>,
    complete: bool,
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
            capabilities: if p.capabilities.is_empty() && !p.model.is_empty() {
                vec![routing_contract::ModelCapability {
                    model_id: p.model.clone(),
                    start_layer: p.start_layer,
                    end_layer: p.end_layer,
                    total_layers: Some(p.total_layers),
                    supports_inference: p.inference_port.is_some(),
                }]
            } else {
                p.capabilities.clone()
            },
            inference_url: p.inference_url(),
            route: route_view_for(&p.node_id, &state.network.routes),
            gateway_url: p.gateway_url(),
            load: state.loads.load(&p.node_id).await,
            is_self,
            node,
            unreachable,
        });
    }

    views.sort_by_key(|v| (v.start_layer, v.short_id.clone()));

    // Layer coverage for every model in the inventory. Older peers do not
    // advertise `total_layers` per capability, so use their legacy peer total
    // when the model matches, otherwise infer the smallest possible total from
    // the greatest advertised layer.
    let mut model_ids: Vec<_> = views
        .iter()
        .flat_map(|view| {
            view.capabilities
                .iter()
                .map(|capability| capability.model_id.clone())
        })
        .collect();
    model_ids.sort();
    model_ids.dedup();
    let model_inventory: Vec<ModelCoverageView> = model_ids
        .into_iter()
        .map(|model_id| {
            let requested_model = &model_id;
            let capabilities: Vec<_> = views
                .iter()
                .flat_map(|view| {
                    view.capabilities
                        .iter()
                        .filter(move |capability| capability.model_id == *requested_model)
                        .map(move |capability| (view, capability))
                })
                .collect();
            let total_layers = capabilities
                .iter()
                .filter_map(|(view, capability)| {
                    capability.total_layers.or_else(|| {
                        (view.model == model_id && view.total_layers > 0)
                            .then_some(view.total_layers)
                    })
                })
                .max()
                .unwrap_or_else(|| {
                    capabilities
                        .iter()
                        .map(|(_, capability)| capability.end_layer + 1)
                        .max()
                        .unwrap_or(0)
                });
            let mut coverage = vec![false; total_layers as usize];
            for (_, capability) in capabilities {
                for layer in capability.start_layer
                    ..=capability.end_layer.min(total_layers.saturating_sub(1))
                {
                    if let Some(slot) = coverage.get_mut(layer as usize) {
                        *slot = true;
                    }
                }
            }
            let covered_layers = coverage.iter().filter(|covered| **covered).count();
            ModelCoverageView {
                model_id,
                total_layers,
                covered_layers,
                complete: total_layers > 0 && covered_layers == total_layers as usize,
                coverage,
            }
        })
        .collect();

    // Preserve the legacy active-model fields for existing UI consumers.
    let active = state.active_model().await;
    let active_coverage = model_inventory
        .iter()
        .find(|inventory| inventory.model_id == active);
    let total = active_coverage
        .map(|inventory| inventory.total_layers)
        .unwrap_or(0);
    let covered_count = active_coverage
        .map(|inventory| inventory.covered_layers)
        .unwrap_or(0);
    let covered = active_coverage
        .map(|inventory| inventory.coverage.clone())
        .unwrap_or_default();

    // Ask the same planner used by generation which handoffs it selected.
    // This is observability only: the UI cannot influence routing or load
    // balancing through this endpoint.
    let routing_edges = if active.is_empty() || total == 0 {
        Vec::new()
    } else {
        crate::pipeline::plan_pipeline(&peers, &active, total, &state.loads)
            .await
            .map(|plan| {
                plan.edges
                    .into_iter()
                    .map(|edge| RoutingEdgeView {
                        from_node_id: edge.from_node_id,
                        to_node_id: edge.to_node_id,
                        model_id: active.clone(),
                        from_end_layer: edge.from_end_layer,
                        to_start_layer: edge.to_start_layer,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let candidate_edges = candidate_edges(&views);

    let self_node = serde_json::to_value(node_info(State(state.clone())).await.0).ok();

    Json(json!({
        "self_id": self_id,
        "active_model": active,
        "total_layers": total,
        "covered_layers": covered_count,
        "coverage": covered,
        "complete": active_coverage.is_some_and(|inventory| inventory.complete),
        "model_inventory": model_inventory,
        "peers": views,
        "routing_edges": routing_edges,
        "candidate_edges": candidate_edges,
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
                    return (
                        node_id,
                        None,
                        Some("sem porta de gateway anunciada".to_string()),
                    );
                };
                let url = format!("{base}/api/ui/models");
                match tokio::time::timeout(PEER_TIMEOUT * 3, http.get(&url).send()).await {
                    Ok(Ok(r)) if r.status().is_success() => {
                        match r.json::<serde_json::Value>().await {
                            Ok(v) => (node_id, Some(v), None),
                            Err(e) => (node_id, None, Some(format!("resposta inválida: {e}"))),
                        }
                    }
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
    #[serde(default = "d_true")]
    pub offload_kqv: bool,
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
    /// Operator kill-switch for speculative decoding — off by default even
    /// on a cluster where every peer supports it (`speculate/v1-lookup`).
    #[serde(default)]
    pub speculative_decoding_enabled: bool,
    #[serde(default = "d_speculative_k")]
    pub speculative_k: u32,
    #[serde(default = "d_speculative_lookup_window")]
    pub speculative_lookup_window: u32,
}

fn d_gpu_layers() -> u32 {
    99
}
fn d_n_ctx() -> u32 {
    8192
}
fn d_n_batch() -> u32 {
    512
}
fn d_max_new() -> u32 {
    256
}
fn d_max_sessions() -> u32 {
    1
}
fn d_prefill_chunk() -> u32 {
    crate::prefill::DEFAULT_CHUNK_TOKENS
}
fn d_temp() -> f32 {
    0.7
}
fn d_top_p() -> f32 {
    0.9
}
fn d_top_k() -> u32 {
    40
}
fn d_repeat_penalty() -> f32 {
    1.15
}
fn d_repeat_last_n() -> u32 {
    64
}
fn d_true() -> bool {
    true
}
fn d_speculative_k() -> u32 {
    4
}
fn d_speculative_lookup_window() -> u32 {
    3
}

impl Default for RuntimeParams {
    fn default() -> Self {
        Self {
            n_gpu_layers: d_gpu_layers(),
            n_ctx: d_n_ctx(),
            offload_kqv: d_true(),
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
            speculative_decoding_enabled: false,
            speculative_k: d_speculative_k(),
            speculative_lookup_window: d_speculative_lookup_window(),
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
            offload_kqv: c.offload_kqv,
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
            speculative_decoding_enabled: c.speculative_decoding_enabled,
            speculative_k: c.speculative_k,
            speculative_lookup_window: c.speculative_lookup_window,
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
    /// Operator-visible model identity. When supplied by deployment it must be
    /// preserved exactly instead of collapsing model families into profiles.
    #[serde(default)]
    pub model_label: Option<String>,
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
    /// Local slices to join into one file before serving. When present, the
    /// peer merges them and serves the union instead of slicing
    /// `source_model_path`.
    #[serde(default)]
    pub merge: Option<Vec<MergeSource>>,
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

fn model_label(req: &ApplyRequest) -> String {
    req.model_label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::model::ModelProfile::guess_from_name(&req.base_name).name)
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
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "job desconhecido" })),
        )
            .into_response(),
    }
}

/// Slice locally if needed, then reconfigure this node onto the result.
async fn apply(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ApplyRequest>,
) -> Response {
    // A merge request lists the files it needs instead of pointing at one; the
    // guard below checks whichever set is actually going to be read.
    if let Some(parts) = &req.merge {
        for p in parts {
            if !std::path::Path::new(&p.source_model_path).exists() {
                return bad_request(format!(
                    "fatia não encontrada neste peer: {}",
                    p.source_model_path
                ));
            }
        }
    } else if !std::path::Path::new(&req.source_model_path).exists() {
        return bad_request(format!(
            "modelo não encontrado neste peer: {}",
            req.source_model_path
        ));
    }
    if req.start_layer > req.end_layer {
        return bad_request("faixa de camadas invertida");
    }
    // Answered here rather than from inside the spawned job: a configuration
    // that `NodeConfig::validate` will reject is a bad request, and reporting
    // it as one keeps the caller from reading "job criado" as "deu certo".
    if req.total_layers == 0 {
        return bad_request(format!(
            "a fatia {} não veio com o total de camadas do modelo, e sem ele o nó não sabe \
             se esta faixa é o começo, o meio ou o fim da rede",
            req.base_name
        ));
    }
    if req.end_layer >= req.total_layers {
        return bad_request(format!(
            "a fatia L{}-{} passa do fim de um modelo de {} camadas",
            req.start_layer, req.end_layer, req.total_layers
        ));
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

async fn run_apply(state: GatewayState, req: ApplyRequest, job_id: &str) -> anyhow::Result<String> {
    let model_path = if let Some(parts) = req.merge.clone() {
        let shard_dir = state.shard_dir().await;
        merge_parts(shard_dir, &req, &parts, job_id).await?
    } else if req.skip_split || (req.is_first() && req.is_last() && req.peer_count <= 1) {
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

        let out_dir = plan::shard_output_dir(&state.shard_dir().await, &req.base_name);
        let progress_job = job_id.to_string();
        let outcome = plan::extract_shard(
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
            if outcome.reused {
                "reaproveitado"
            } else {
                "gerado"
            }
        );
        outcome.output_path
    };

    set_job(
        job_id,
        "running",
        "carregando",
        "iniciando servidor de inferência",
    )
    .await;

    // The selected deployment label is shared by every peer. Old callers that
    // do not send it retain the profile-derived compatibility fallback.
    let label = model_label(&req);

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
        offload_kqv: req.params.offload_kqv,
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
        speculative_decoding_enabled: req.params.speculative_decoding_enabled,
        speculative_k: req.params.speculative_k,
        speculative_lookup_window: req.params.speculative_lookup_window,
    };

    state.runtime.apply(cfg).await?;
    Ok(model_path)
}

/// Join local slices into one GGUF covering their union, then serve that.
///
/// The peer that receives a merged step holds (or just received) each part as
/// a separate file; this runs `splitter::merge` on this machine and the result
/// becomes the model file the node is configured with. The output lands in the
/// shard directory under the shared base name, so the catalog picks it up the
/// same way it picks up a slice that was carved here.
async fn merge_parts(
    shard_dir: std::path::PathBuf,
    req: &ApplyRequest,
    parts: &[MergeSource],
    job_id: &str,
) -> anyhow::Result<String> {
    if parts.len() < 2 {
        anyhow::bail!("juntar menos de duas fatias não faz sentido");
    }
    let start = parts.first().map(|p| p.start_layer).unwrap_or(0);
    let end = parts
        .last()
        .map(|p| p.end_layer)
        .ok_or_else(|| anyhow::anyhow!("nenhuma fatia para juntar"))?;
    let out_dir = plan::shard_output_dir(&shard_dir, &req.base_name);
    let output = out_dir.join(plan::shard_filename(&req.base_name, start, end));

    // Reuse an existing merge: rebuilding gigabytes is wasted I/O for an
    // identical result. Safe because the output name is only ever given to a
    // complete file (see the rename below).
    if let Ok(meta) = tokio::fs::metadata(&output).await {
        if meta.len() > 1024 {
            info!(
                "modelo junto já existe, reaproveitando: {}",
                output.display()
            );
            invalidate_catalog().await;
            return Ok(output.to_string_lossy().into_owned());
        }
    }

    set_job(
        job_id,
        "running",
        "juntando fatias",
        &format!(
            "{} fatia(s), camadas {}-{} (pode levar alguns minutos)",
            parts.len(),
            start,
            end
        ),
    )
    .await;

    let merge_parts: Vec<crate::splitter::MergePart> = parts
        .iter()
        .map(|p| crate::splitter::MergePart {
            path: std::path::PathBuf::from(&p.source_model_path),
            start_layer: p.start_layer,
            end_layer: p.end_layer,
        })
        .collect();
    let dst = output.clone();
    // Write to a temporary name and rename when complete, so a merge cut short
    // (kill, power loss) never leaves a half file under the final name where a
    // later apply would reuse it as a finished model. The unique suffix also
    // keeps two concurrent merges of the same range from interleaving writes.
    let temp = output.with_file_name(format!(
        "{}.{}.part",
        output
            .file_name()
            .map(|s| s.to_string_lossy())
            .unwrap_or_default(),
        uuid::Uuid::new_v4().simple()
    ));
    let temp_cleanup = temp.clone();
    let progress_job = job_id.to_string();

    let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let n = crate::splitter::merge(&merge_parts, &temp, move |pct| {
            let id = progress_job.clone();
            tokio::spawn(async move { set_job_progress(&id, pct).await });
        })?;
        // No Windows o rename não substitui um destino que já existe. A fusão
        // só chega aqui quando o arquivo junto não estava lá, então um destino
        // ocupado é outra fusão que terminou primeiro: o resultado é o mesmo
        // arquivo, e ficar com o que já está pronto é o certo.
        #[cfg(windows)]
        if dst.exists() {
            let _ = std::fs::remove_file(&temp);
            return Ok(std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(n));
        }
        std::fs::rename(&temp, &dst)
            .with_context(|| format!("mover {} para {}", temp.display(), dst.display()))?;
        Ok(n)
    })
    .await
    .context("spawn_blocking para splitter")?
    .inspect_err(|_| {
        let _ = std::fs::remove_file(&temp_cleanup);
    })?;

    invalidate_catalog().await;

    info!(
        "modelo junto pronto: {} ({:.1} GB)",
        output.display(),
        bytes as f64 / 1_073_741_824.0
    );
    Ok(output.to_string_lossy().into_owned())
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
async fn slice_plan(_auth: ConnectionAuth, Json(req): Json<SlicePlanRequest>) -> Response {
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
        return bad_request(format!(
            "modelo não encontrado nesta máquina: {}",
            req.model_path
        ));
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
        match run_slice(
            state,
            source,
            out_dir,
            base_name,
            ranges,
            total_layers,
            &job_id,
        )
        .await
        {
            Ok(files) => {
                let msg = format!(
                    "{} fatia{} em {}",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" },
                    files
                        .first()
                        .and_then(|f| std::path::Path::new(f)
                            .parent()
                            .map(|p| p.display().to_string()))
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
    _state: GatewayState,
    source: std::path::PathBuf,
    out_dir: std::path::PathBuf,
    base_name: String,
    ranges: Vec<(u32, u32)>,
    total_layers: u32,
    job_id: &str,
) -> anyhow::Result<Vec<String>> {
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
        set_job(
            job_id,
            "running",
            "lendo",
            &format!("{label} · lendo o modelo"),
        )
        .await;

        let progress_job = job_id.to_string();
        let writing_label = label.clone();
        let first_report = Arc::new(AtomicBool::new(true));
        let outcome = plan::extract_shard(
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
            if outcome.reused {
                "reaproveitada"
            } else {
                "gerada"
            }
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

// ── Cluster launch ───────────────────────────────────────────────────────────

/// Stop the inference server on this node, announce the node as idle,
/// and free the inference port. Called by `launch` before reconfiguring
/// peers so no stale endpoint receives inference requests mid-transition.
async fn stop_inference(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
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
async fn unload_model(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
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
async fn leave_network(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
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
        let Some(base) = peer.gateway_url() else {
            continue;
        };

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
            .flat_map(|set| {
                set.get("shards")
                    .and_then(|s| s.as_array())
                    .into_iter()
                    .flatten()
            })
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
async fn release_candidate(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
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
        model_label: Some(cfg.model_label.clone()),
        start_layer: cfg.start_layer,
        end_layer: cfg.end_layer,
        total_layers: cfg.total_layers,
        peer_count: cfg.peer_count,
        peer_index: cfg.peer_index,
        primary: cfg.primary,
        skip_split: true,
        merge: None,
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
async fn rejoin_network(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    let assignment = state.local_assignment.read().await.clone();
    match state.network.rejoin(assignment.as_ref()).await {
        Ok(()) => Json(json!({ "detached": false })).into_response(),
        Err(e) => bad_request(format!("não deu para voltar à rede: {e}")),
    }
}

/// One local slice feeding a merge on this peer: the file and the range it
/// stands for. The peer joins these into one GGUF before serving it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeSource {
    /// Path of the slice *on this peer*.
    pub source_model_path: String,
    pub start_layer: u32,
    pub end_layer: u32,
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
    /// Local slices to join into one file before serving. When present, the
    /// peer merges them (`splitter::merge`) and serves the union; the merge
    /// replaces `skip_split` because the file covering the range only comes
    /// into existence through the merge.
    #[serde(default)]
    pub merge: Option<Vec<MergeSource>>,
    /// How many of this peer's layers go on its GPU, overriding
    /// `params.n_gpu_layers` for this peer alone.
    ///
    /// The only knob that is honestly per-machine: the rest of `RuntimeParams`
    /// is either protocol (context window, batch, chunk) or sampling, and both
    /// have to agree across the pipeline. Offload does not — a box with a 5060
    /// Ti and a CPU-only box in the same cluster want different numbers, and
    /// before this the whole launch had to settle on one.
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    /// Onde o cache KV deste peer mora, sobrepondo `params.offload_kqv` só para
    /// ele.
    ///
    /// Pela mesma razão que o offload: o cache é dimensionado pela janela de
    /// contexto, e uma máquina com 16 GB de VRAM e uma sem GPU no mesmo
    /// pipeline não podem estar as duas certas sobre onde ele cabe.
    #[serde(default)]
    pub offload_kqv: Option<bool>,
    /// Posição deste peer no pipeline, quando ela não é a posição dele nesta
    /// requisição.
    ///
    /// `launch` deduz `peer_index`/`peer_count` da lista de alvos, o que só
    /// vale quando a requisição descreve o pipeline inteiro. Quem soma uma
    /// máquina de cada vez (`deploy::add_node_endpoint`) manda um alvo e
    /// precisa dizer a posição real - sem isto o nó somado se declara o último
    /// da divisão, sobe sem captura de residual e a inferência morre no
    /// primeiro token com "no embeddings".
    #[serde(default)]
    pub peer_index: Option<u32>,
    #[serde(default)]
    pub peer_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct LaunchRequest {
    pub base_name: String,
    /// The identity selected in the deployment screen, propagated unchanged
    /// to every apply request. Missing values keep older callers compatible.
    #[serde(default)]
    pub model_label: Option<String>,
    pub total_layers: u32,
    pub targets: Vec<LaunchTarget>,
    /// Node that should act as the entry point.
    #[serde(default)]
    pub primary_node_id: Option<String>,
    #[serde(default)]
    pub params: RuntimeParams,
}

/// The cluster-wide settings as one peer will actually receive them.
///
/// Everything in `RuntimeParams` has to agree across the pipeline — the context
/// window and the batch decide the prompt budget the gateway enforces, and the
/// sampling is only used by the last peer — except how much of its own share a
/// machine offloads to its GPU. That one is a property of the machine.
fn params_for(base: &RuntimeParams, target: &LaunchTarget) -> RuntimeParams {
    let mut params = base.clone();
    if let Some(gpu) = target.n_gpu_layers {
        params.n_gpu_layers = gpu;
    }
    if let Some(kqv) = target.offload_kqv {
        params.offload_kqv = kqv;
    }
    params
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
    // Checked before phase 1, which stops inference on every target: a launch
    // that cannot possibly apply must not be allowed to take the cluster down
    // on its way to failing. `deploy.rs` refuses this earlier and with the
    // offending files named; this is the guard for every other caller.
    if req.total_layers == 0 {
        return bad_request(
            "não foi informado de quantas camadas é o modelo, e as fatias não podem ser \
             atribuídas sem esse total. Fatias geradas pela tela de Fatiar gravam esse dado; \
             as antigas não, e nesse caso o total é deduzido da cobertura das fatias.",
        );
    }
    if let Some(t) = req.targets.iter().find(|t| t.end_layer >= req.total_layers) {
        return bad_request(format!(
            "a fatia L{}-{} passa do fim de um modelo de {} camadas",
            t.start_layer, t.end_layer, req.total_layers
        ));
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
                warn!(
                    "peer {} inacessivel para stop-inference: {e}",
                    target.node_id
                );
            }
        }
    }

    // Brief settle: let the OS release ports.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 2: dispatch apply with the new configuration.
    let mut results = Vec::new();

    for (index, target) in req.targets.iter().enumerate() {
        let params = params_for(&req.params, target);
        let apply_req = ApplyRequest {
            source_model_path: target.source_model_path.clone(),
            base_name: req.base_name.clone(),
            model_label: req.model_label.clone(),
            start_layer: target.start_layer,
            end_layer: target.end_layer,
            total_layers: req.total_layers,
            peer_count: target.peer_count.unwrap_or(peer_count),
            peer_index: target.peer_index.unwrap_or(index as u32),
            primary: req.primary_node_id.as_deref() == Some(target.node_id.as_str()),
            skip_split: target.skip_split,
            merge: target.merge.clone(),
            params,
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

pub async fn get_setup_key(State(config): State<AuthState>) -> impl IntoResponse {
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
            cfg.bootstrap_nodes = req
                .bootstrap
                .split(',')
                .map(crate::config::normalize_peer_addr)
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    match cfg.save() {
        Ok(()) => {
            // Trigger restart: spawn a new instance and exit.
            // The new instance will find the config file and start in full mode.
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                let e = crate::respawn::respawn_self();
                tracing::error!("respawn falhou, o nó segue no ar: {e}");
            });
            Json(json!({"saved": true, "restart": true, "message": "Configuracao salva. Reiniciando..."})).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Um blob do store do Ollama: `<raiz>/blobs/sha256-…`, com `<raiz>/manifests`
/// ao lado.
///
/// O manifesto é que dá nome ao modelo, e ele não é reescrito quando o blob é
/// apagado por fora. A conferência do `manifests/` ao lado importa porque a
/// biblioteca do LM Studio também tem uma pasta `blobs/` — vazia (ver
/// `catalog::detect_source`).
fn is_ollama_blob(path: &std::path::Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if parent.file_name().and_then(|n| n.to_str()) != Some("blobs") {
        return false;
    }
    parent
        .parent()
        .map(|root| root.join("manifests").is_dir())
        .unwrap_or(false)
}

#[derive(Debug, Deserialize)]
pub struct DeleteRequest {
    pub path: String,
    /// Sem isto, avisos viram 409 e nada é apagado.
    #[serde(default)]
    pub force: bool,
}

/// Apaga um GGUF do disco desta máquina.
///
/// Avisa, mas não impede: quem opera decide. O que não se faz é apagar em
/// silêncio algo que está no ar — a resposta nomeia o que segura o arquivo.
async fn delete_model(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeleteRequest>,
) -> Response {
    let path = match std::path::Path::new(&req.path).canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("arquivo não encontrado: {}", req.path) })),
            )
                .into_response()
        }
    };

    // O mesmo portão que governa o que sai da máquina governa o que some dela.
    if let Err(e) = crate::transfer::ensure_gguf(&path) {
        return bad_request(e);
    }

    let size = tokio::fs::metadata(&path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut warnings = Vec::new();

    // Servindo agora: apagar tira o modelo do ar sem que nada na tela diga, e
    // o nó segue anunciando cobertura até a próxima requisição quebrar.
    if let Some(cfg) = state.runtime.config().await {
        if std::path::Path::new(&cfg.model_path)
            .canonicalize()
            .map(|p| p == path)
            .unwrap_or(false)
        {
            warnings.push(json!({
                "kind": "serving",
                "message": format!(
                    "este nó está servindo as camadas {}-{} deste arquivo; apagar tira o modelo do ar",
                    cfg.start_layer, cfg.end_layer
                ),
            }));
        }
    }

    // Em trânsito: apagar por baixo de uma transferência em curso.
    let target = path.to_string_lossy().to_string();
    let in_flight: Vec<String> = state
        .transfers
        .jobs
        .read()
        .await
        .values()
        .filter(|j| j.path == target && (j.state == "running" || j.state == "verifying"))
        .map(|j| j.id.clone())
        .collect();
    for id in &in_flight {
        warnings.push(json!({
            "kind": "transfer",
            "message": format!("há uma transferência em curso com este arquivo (job {id})"),
        }));
    }

    if is_ollama_blob(&path) {
        warnings.push(json!({
            "kind": "ollama_store",
            "message": "este arquivo está dentro do store do Ollama; apagá-lo deixa o manifesto do Ollama órfão. O certo lá é `ollama rm`",
        }));
    }

    if !warnings.is_empty() && !req.force {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "warnings": warnings,
                "path": target,
                "size_bytes": size,
            })),
        )
            .into_response();
    }

    if let Err(e) = tokio::fs::remove_file(&path).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("não deu para apagar: {e}") })),
        )
            .into_response();
    }
    info!("🗑️  apagado do disco: {}", path.display());
    Json(json!({ "deleted": true, "path": target, "freed_bytes": size })).into_response()
}

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
mod delete_tests {
    use super::*;

    /// O aviso do store do Ollama existe porque apagar um blob de lá deixa o
    /// manifesto apontando para nada — o store fica quebrado, e ele não é
    /// nosso para consertar.
    #[test]
    fn an_ollama_blob_is_recognised_by_its_layout() {
        let root = std::env::temp_dir().join(format!("rdma-del-{}", std::process::id()));
        let blobs = root.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(root.join("manifests")).unwrap();
        let blob = blobs.join("sha256-abc");
        std::fs::write(&blob, b"GGUF").unwrap();

        assert!(is_ollama_blob(&blob));

        // Um `blobs/` sem `manifests/` ao lado não é o store do Ollama — o
        // LM Studio também tem uma pasta com esse nome, e vazia.
        let outro = std::env::temp_dir().join(format!("rdma-del2-{}", std::process::id()));
        std::fs::create_dir_all(outro.join("blobs")).unwrap();
        let solto = outro.join("blobs").join("sha256-xyz");
        std::fs::write(&solto, b"GGUF").unwrap();
        assert!(!is_ollama_blob(&solto));

        let fatia = root.join("solto.gguf");
        std::fs::write(&fatia, b"GGUF").unwrap();
        assert!(!is_ollama_blob(&fatia));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outro).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rota escolhida tem que chegar a interface: sem ela, "por onde este
    /// salto vai" so se responde com shell na maquina.
    #[test]
    fn the_cluster_view_carries_the_chosen_route() {
        let tabela = crate::route::RouteTable::new();
        let vista = route_view_for("n1", &tabela);
        assert!(vista.is_none(), "peer nao medido nao inventa rota");
    }

    /// Offload e cache KV sao as duas coisas que uma maquina pode discordar do
    /// cluster, e discordar nao pode mexer em mais nada de que o pipeline
    /// depende - contexto, batch e amostragem tem que continuar iguais nos dois
    /// lados do salto.
    #[test]
    fn only_the_hardware_settings_are_per_machine() {
        let base = RuntimeParams {
            n_gpu_layers: 99,
            n_ctx: 32768,
            offload_kqv: true,
            ..RuntimeParams::default()
        };
        let target = |gpu, kqv| LaunchTarget {
            node_id: "a".into(),
            source_model_path: "/m.gguf".into(),
            start_layer: 0,
            end_layer: 15,
            skip_split: true,
            merge: None,
            n_gpu_layers: gpu,
            offload_kqv: kqv,
            peer_index: None,
            peer_count: None,
        };

        let cpu_only = params_for(&base, &target(Some(0), Some(false)));
        assert_eq!(cpu_only.n_gpu_layers, 0);
        assert!(!cpu_only.offload_kqv);
        assert_eq!(cpu_only.n_ctx, 32768, "o resto continua sendo do cluster");

        // Sem sobreposicao, o numero do cluster vale — e sobrepor um nao mexe
        // no outro.
        let herdado = params_for(&base, &target(None, None));
        assert_eq!(herdado.n_gpu_layers, 99);
        assert!(herdado.offload_kqv);
        assert!(params_for(&base, &target(Some(0), None)).offload_kqv);
        assert_eq!(
            params_for(&base, &target(None, Some(false))).n_gpu_layers,
            99
        );
    }

    fn req(start: u32, end: u32, total: u32, peers: u32) -> ApplyRequest {
        ApplyRequest {
            source_model_path: "/m.gguf".into(),
            base_name: "m".into(),
            model_label: None,
            start_layer: start,
            end_layer: end,
            total_layers: total,
            peer_count: peers,
            peer_index: 0,
            primary: true,
            skip_split: false,
            merge: None,
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
        assert!(p.offload_kqv);
    }

    #[test]
    fn params_accept_partial_overrides() {
        let p: RuntimeParams =
            serde_json::from_str(r#"{"n_ctx":250000,"no_thinking":false,"offload_kqv":false}"#)
                .unwrap();
        assert_eq!(p.n_ctx, 250000);
        assert!(!p.no_thinking);
        assert!(!p.offload_kqv);
        assert_eq!(p.top_k, 40, "campos ausentes mantêm o padrão");
    }

    #[test]
    fn selected_model_label_is_not_normalized_to_a_profile_name() {
        let mut request = req(0, 31, 32, 1);
        request.model_label = Some("Qwen3.5-9B-Instruct".into());

        assert_eq!(model_label(&request), "Qwen3.5-9B-Instruct");
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// `merge_parts` with two real mini-GGUF slices: the execution path the
    /// planner's merged steps feed into, exercised with files instead of mocks.
    #[test]
    fn merge_parts_joins_two_slices_into_a_whole() {
        block_on(async {
            let root =
                std::env::temp_dir().join(format!("rdmap2p-webui-merge-{}", std::process::id()));
            let parts_dir = root.join("parts");
            std::fs::create_dir_all(&parts_dir).unwrap();
            let a = parts_dir.join("m_0_15.gguf");
            let b = parts_dir.join("m_16_31.gguf");
            std::fs::write(&a, crate::splitter::test_gguf_bytes(16)).unwrap();
            std::fs::write(&b, crate::splitter::test_gguf_bytes(16)).unwrap();

            let req = req(0, 31, 32, 1);
            let parts = vec![
                MergeSource {
                    source_model_path: a.to_string_lossy().into_owned(),
                    start_layer: 0,
                    end_layer: 15,
                },
                MergeSource {
                    source_model_path: b.to_string_lossy().into_owned(),
                    start_layer: 16,
                    end_layer: 31,
                },
            ];
            let out = merge_parts(root.clone(), &req, &parts, "job-merge")
                .await
                .unwrap();
            let out_path = std::path::Path::new(&out);
            assert_eq!(
                out_path.file_name().unwrap().to_string_lossy().as_ref(),
                "m_0_31.gguf",
                "o modelo junto sai no diretório de fatias com o nome convencional"
            );
            let layout = crate::gguf::read_layout(out_path).unwrap();
            assert_eq!(
                layout.layer_count(),
                32,
                "as duas fatias juntas cobrem o modelo"
            );
            assert!(out_path.metadata().unwrap().len() > a.metadata().unwrap().len());

            // Reuse: a second apply with the same range must not rebuild it —
            // a marker written over the output proves the file was left alone.
            let marker = vec![0xAAu8; 2048];
            std::fs::write(&out_path, &marker).unwrap();
            let again = merge_parts(root.clone(), &req, &parts, "job-merge-2")
                .await
                .unwrap();
            assert_eq!(again, out);
            assert_eq!(
                std::fs::read(&out_path).unwrap(),
                marker,
                "o modelo junto existente foi reaproveitado, não reescrito"
            );

            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn merge_parts_refuses_one_part_or_a_gap() {
        block_on(async {
            let root = std::env::temp_dir()
                .join(format!("rdmap2p-webui-merge-bad-{}", std::process::id()));
            let parts_dir = root.join("parts");
            std::fs::create_dir_all(&parts_dir).unwrap();
            let a = parts_dir.join("m_0_15.gguf");
            let b = parts_dir.join("m_17_31.gguf");
            std::fs::write(&a, crate::splitter::test_gguf_bytes(16)).unwrap();
            std::fs::write(&b, crate::splitter::test_gguf_bytes(15)).unwrap();

            let req = req(0, 31, 32, 1);
            let single = vec![MergeSource {
                source_model_path: a.to_string_lossy().into_owned(),
                start_layer: 0,
                end_layer: 15,
            }];
            let err = merge_parts(root.clone(), &req, &single, "job-1")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("menos de duas"), "{err}");

            let gap = vec![
                MergeSource {
                    source_model_path: a.to_string_lossy().into_owned(),
                    start_layer: 0,
                    end_layer: 15,
                },
                MergeSource {
                    source_model_path: b.to_string_lossy().into_owned(),
                    start_layer: 17,
                    end_layer: 31,
                },
            ];
            let err = merge_parts(root.clone(), &req, &gap, "job-2")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("vizinhas"), "{err}");

            std::fs::remove_dir_all(&root).ok();
        });
    }
}
