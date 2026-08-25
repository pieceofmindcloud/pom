//! Web interface served by the peer itself, plus the control API behind it.
//!
//! The assets are NOT compiled into the binary: `webapp.rs` reads the built
//! bundle from disk at `config::ui_dist_dir()` (default
//! `<project_root>/apps/frontend/dist`, overridable with `POM_UI_DIST`),
//! so editing the frontend requires `npm run build` in `apps/frontend/` and
//! shipping `dist/` next to the binary.
//!
//! Every node serves it, so the UI can be opened on any peer. The cluster view
//! works by asking each peer's own `/api/ui/node`, which is why `PeerInfo`
//! carries `gateway_port`.

use crate::auth::{AuthState, ConnectionAuth};
use crate::catalog::{self, Catalog};
use crate::config::{KvCacheType, Role};
use crate::deploy::normalize_deployment_name;
use crate::gateway::GatewayState;
use crate::gguf;
use crate::network::PeerInfo;
use crate::plan::{self, PeerBudget, DEFAULT_RESERVE_PCT};
use crate::runtime::{NodeConfig, RuntimeStatus};
use crate::sysmon::{self, SystemLoad};
use anyhow::Context;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
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
/// A cluster snapshot is cheap to reuse briefly, but never allowed to hide a
/// peer/runtime mutation for more than this window.
const CLUSTER_CACHE_TTL: Duration = Duration::from_secs(2);
/// Timeout for one peer-to-peer control call.
pub(crate) const PEER_TIMEOUT: Duration = Duration::from_secs(4);

/// A stable validator for JSON consumed by the browser. Callers must pass the
/// UI payload, not a larger server response containing sampling timestamps or
/// other transport-only noise.
pub(crate) fn etag_for_json<T: Serialize>(body: &T) -> String {
    let bytes = serde_json::to_vec(body).expect("JSON de estado da UI deve serializar");
    format!("\"{}\"", hex::encode(Sha256::digest(bytes)))
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|values| {
            values
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == "*" || candidate == etag)
        })
}

fn json_response_with_etag(
    headers: &HeaderMap,
    bytes: Vec<u8>,
    etag: &str,
    version: &str,
) -> Response {
    let not_modified = if_none_match(headers, etag);
    let mut response = if not_modified {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(bytes))
    };
    *response.status_mut() = if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(etag).expect("ETag de SHA256 é um header válido"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("x-state-version"),
        HeaderValue::from_str(version).expect("versão de estado é um header válido"),
    );
    if !not_modified {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    response
}

/// Return a conditional JSON response, keeping 304 bodies empty.
pub(crate) fn conditional_json<T: Serialize>(headers: &HeaderMap, body: &T) -> Response {
    let bytes = serde_json::to_vec(body).expect("JSON de estado da UI deve serializar");
    let etag = etag_for_json(body);
    json_response_with_etag(headers, bytes, &etag, &etag)
}

/// Same response helper with a monotonic snapshot version.
pub(crate) fn conditional_json_with_version<T: Serialize>(
    headers: &HeaderMap,
    body: &T,
    etag: &str,
    version: u64,
) -> Response {
    let bytes = serde_json::to_vec(body).expect("JSON de estado da UI deve serializar");
    json_response_with_etag(headers, bytes, etag, &version.to_string())
}

pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/node", get(node_info))
        .route("/api/ui/models", get(models))
        .route("/api/ui/models/delete", post(delete_model))
        .route("/api/ui/cluster", get(cluster))
        .route("/api/ui/cluster/models", get(cluster_models))
        .route("/api/ui/peer/:node_id/runtime", get(peer_runtime))
        .route("/api/ui/network/rtt", get(network_rtt))
        .route("/api/ui/chat/telemetry", post(chat_telemetry))
        .route("/api/ui/paths", get(get_paths).post(save_paths))
        .route(
            "/api/ui/cache-types",
            get(get_cache_types).post(save_cache_types),
        )
        .route("/api/ui/network/advertise-addr", post(save_advertise_addr))
        .route("/api/ui/network/tls", post(save_tls))
        .route("/api/ui/plan", post(plan_split_endpoint))
        .route("/api/ui/apply", post(apply))
        .route("/api/ui/launch", post(launch))
        .route("/api/ui/model-identity", post(model_identity))
        .route("/api/ui/stop-inference", post(stop_inference))
        .route("/api/ui/slice", post(slice_locally))
        .route("/api/ui/slice/plan", post(slice_plan))
        .route("/api/ui/tp/check", post(tp_check))
        .route("/api/ui/jobs", get(list_jobs))
        .route("/api/ui/jobs/:id", get(job_status))
        .route("/api/ui/unload", post(unload_model))
        .route("/api/ui/reconfigure", post(reconfigure))
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
    // consumidor e aquela pagina, servida so enquanto nao ha `pom.json`.
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
    /// Endereços UDP anunciados pelo nó, necessários para o plano reconstruir
    /// as multiaddresses TCP do túnel no processo que executar o hop.
    pub addr_candidates: Vec<String>,
    /// Identidade e capabilities do túnel libp2p anunciadas por este nó.
    pub libp2p_peer_id: Option<String>,
    pub inference_frame_capabilities: Vec<String>,
    pub gateway_port: u16,
    pub inference_port: Option<u16>,
    pub config: Option<NodeConfig>,
    pub status: RuntimeStatus,
    pub system: SystemLoad,
    pub peer_count: usize,
    pub load: u32,
    pub active_model: String,
    pub active_deployment: String,
    /// True while the operator has taken this node off the network.
    pub detached: bool,
    /// Raw operator-pinned address (`Config.advertise_addr`), for pre-filling
    /// the edit form — `None` means "nothing pinned," not "unknown."
    pub advertise_addr: Option<String>,
    /// What this node actually puts in `Announce.addr` right now
    /// (`Network::announce_addr()`, full `host:port`): the pin if set, else
    /// a peer-observed public address, else the `0.0.0.0:<port>` "ask the
    /// receiver" sentinel. Always present — lets the UI show the fix taking
    /// effect without a second round trip.
    pub effective_address: String,
    /// Se o cluster fala TLS entre peers agora.
    pub tls_required: bool,
    /// Se **este** nó pode mudar isso - só o principal pode. A tela usa para
    /// decidir entre interruptor e leitura; a checagem que vale é a do
    /// `save_tls`, no servidor.
    pub tls_authority: bool,
}

async fn node_info(State(state): State<GatewayState>) -> Json<NodeInfo> {
    let system = sysmon::sample().await;
    let config = state.runtime.config().await;
    let active_deployment = config
        .as_ref()
        .map(|cfg| cfg.deployment_name.clone())
        .unwrap_or_default();
    let (role, conn_key, advertise_addr, tls_required, tls_authority) = {
        let cfg = state.auth.read().await;
        (
            cfg.role.to_string(),
            cfg.connection_key.clone(),
            cfg.advertise_addr.clone(),
            cfg.tls_required,
            cfg.role == crate::config::Role::Principal,
        )
    };
    Json(NodeInfo {
        short_id: state.network.node_id[..state.network.node_id.len().min(12)].to_string(),
        node_id: state.network.node_id.clone(),
        // Not `system.hostname`: that's the raw OS hostname, and ignores
        // `node_name` from pom.json. `network.hostname` is what
        // `with_identity` (main.rs) actually announces to peers — the same
        // value `/admin/nodes` already shows correctly on the Network tab.
        hostname: state.network.hostname.clone(),
        build_commit: crate::BUILD_COMMIT.to_string(),
        dev_update: crate::devupdate::enabled(),
        role,
        connection_key: conn_key,
        p2p_port: state.network.bind_addr.port(),
        addr_candidates: state.network.self_addr_candidates(),
        libp2p_peer_id: state.network.libp2p_peer_id.clone(),
        inference_frame_capabilities: {
            let mut capabilities = vec![
                crate::pipeline::CAP_FRAME_BATCH.to_string(),
                "relay/v1-chain".to_string(),
                "relay/v2-direct".to_string(),
                "speculate/v1-lookup".to_string(),
                "speculate/v1-mtp-sliced".to_string(),
            ];
            if state.network.mtp_ready() {
                capabilities.push("speculate/v1-mtp".to_string());
            }
            if state.network.batching_ready() {
                capabilities.push(crate::pipeline::CAP_FRAME_BATCH.to_string());
            }
            if state.network.libp2p_tunnel_ready() {
                capabilities.push(crate::pipeline::CAP_LIBP2P_TUNNEL.to_string());
            }
            capabilities
        },
        gateway_port: state.gateway_port,
        inference_port: state.network.inference_port,
        config,
        status: state.runtime.status().await,
        peer_count: state.network.alive_peers().await.len(),
        load: state.loads.load(&state.network.node_id).await,
        active_model: state.active_model().await,
        active_deployment,
        detached: state.network.is_detached(),
        advertise_addr,
        effective_address: state.network.announce_addr(),
        tls_required,
        tls_authority,
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
pub(crate) async fn invalidate_catalog() {
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
    used_bytes: u64,
    max_bytes: Option<u64>,
    usage_percent: Option<f64>,
}

fn path_entry(dir: &std::path::Path, max_bytes: Option<u64>) -> PathEntry {
    let used_bytes = crate::storage::directory_bytes(dir).unwrap_or(0);
    let usage_percent = max_bytes.map(|max| {
        crate::storage::StorageUsage {
            used_bytes,
            max_bytes: max,
        }
        .percent()
    });
    PathEntry {
        path: dir.to_string_lossy().into_owned(),
        exists: dir.is_dir(),
        kind: match catalog::detect_source(dir) {
            catalog::ModelSource::Ollama => "ollama",
            catalog::ModelSource::LmStudio => "lmstudio",
            _ => "folder",
        },
        used_bytes,
        max_bytes,
        usage_percent,
    }
}

/// Where this node looks for models and where it writes slices.
async fn get_paths(State(state): State<GatewayState>) -> impl IntoResponse {
    let cfg = state.auth.read().await;
    let shard = cfg.shard_dir_path();
    let shard_max_bytes = cfg.shard_max_bytes();
    Json(json!({
        "model_dirs": cfg.model_dir_paths().iter().map(|d| path_entry(d, None)).collect::<Vec<_>>(),
        "shard_dir": path_entry(&shard, Some(shard_max_bytes)),
        "shard_max_gb": cfg.shard_max_gb(),
        "using_defaults": cfg.model_dirs_are_default(),
        "detected": catalog::default_model_dirs()
            .iter().map(|d| path_entry(d, None)).collect::<Vec<_>>(),
        "default_shard_dir": crate::config::default_shard_dir().to_string_lossy(),
    }))
}

/// The KV cache representation is an installation setting, not a deployment
/// or machine override. The deploy screen reads this separately so it cannot
/// accidentally put a global storage choice into one node's launch payload.
async fn get_cache_types(State(state): State<GatewayState>) -> impl IntoResponse {
    let cfg = state.auth.read().await;
    Json(json!({
        "cache_type_k": cfg.cache_type_k,
        "cache_type_v": cfg.cache_type_v,
        "choices": KvCacheType::ALL.iter().map(|kind| kind.as_str()).collect::<Vec<_>>(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct SaveCacheTypesRequest {
    pub cache_type_k: KvCacheType,
    pub cache_type_v: KvCacheType,
}

async fn save_cache_types(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveCacheTypesRequest>,
) -> Response {
    {
        let mut cfg = state.auth.write().await;
        cfg.cache_type_k = req.cache_type_k;
        cfg.cache_type_v = req.cache_type_v;
        if let Err(e) = cfg.save() {
            return bad_request(format!("não deu para gravar pom.json: {e}"));
        }
    }
    info!(
        cache_type_k = req.cache_type_k.as_str(),
        cache_type_v = req.cache_type_v.as_str(),
        "tipos do cache KV atualizados pela interface"
    );
    get_cache_types(State(state)).await.into_response()
}

#[derive(Debug, Deserialize)]
pub struct SavePathsRequest {
    pub model_dirs: Vec<String>,
    /// Empty means "back to `<project_root>/models`".
    #[serde(default)]
    pub shard_dir: String,
    #[serde(default)]
    pub shard_max_gb: Option<f64>,
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

fn reject_nested_shard_dir(model_dirs: &[String], shard_dir: &str) -> Result<(), String> {
    let shard = std::path::Path::new(shard_dir)
        .canonicalize()
        .map_err(|e| format!("Pasta de modelos fatiados: não deu para resolver o caminho: {e}"))?;
    for model in model_dirs {
        let model = std::path::Path::new(model).canonicalize().map_err(|e| {
            format!("Pasta de modelos inteiros: não deu para resolver {model}: {e}")
        })?;
        if shard == model || shard.starts_with(&model) {
            return Err(
                "a pasta de modelos fatiados não pode ser a mesma ou ficar dentro de uma pasta de modelos inteiros".to_string(),
            );
        }
    }
    Ok(())
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

    let effective_shard_dir = shard_dir.clone().unwrap_or_else(|| {
        crate::config::default_shard_dir()
            .to_string_lossy()
            .into_owned()
    });
    if let Err(e) = reject_nested_shard_dir(&model_dirs, &effective_shard_dir) {
        return bad_request(e);
    }
    let shard_max_gb = match req.shard_max_gb {
        None => crate::storage::DEFAULT_SHARD_MAX_GB,
        Some(value) if value.is_finite() && value > 0.0 => value,
        Some(_) => {
            return bad_request("o limite da pasta de modelos fatiados precisa ser maior que zero")
        }
    };

    {
        let mut cfg = state.auth.write().await;
        cfg.model_dirs = Some(model_dirs);
        cfg.shard_dir = shard_dir;
        cfg.shard_max_gb = shard_max_gb;
        if let Err(e) = cfg.save() {
            return bad_request(format!("não deu para gravar pom.json: {e}"));
        }
    }

    invalidate_catalog().await;
    info!("pastas de modelos atualizadas pela interface");
    get_paths(State(state)).await.into_response()
}

#[derive(Debug, Deserialize)]
pub struct SaveAdvertiseAddrRequest {
    /// Empty means "clear the pin" — same convention as `SavePathsRequest.shard_dir`.
    #[serde(default)]
    pub advertise_addr: String,
}

/// Pin (or clear) the address this node advertises to peers, live — no
/// restart. See `Network::set_advertise_addr`'s doc comment for why this is
/// a live update rather than the `save_setup`-style respawn other config
/// changes use: a respawn re-execs with the *original* CLI argv, and
/// `--advertise-addr`/`--public-addr` always beat the config file value, so
/// it would be silently ineffective for a node ever launched with either
/// flag on its command line.
async fn save_advertise_addr(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveAdvertiseAddrRequest>,
) -> Response {
    let raw = req.advertise_addr.trim();
    let normalized = if raw.is_empty() {
        None
    } else {
        match crate::network::advertised_addr(raw, state.network.bind_addr.port()) {
            Some(a) => Some(a),
            None => return bad_request(format!("“{raw}” não é um endereço válido.")),
        }
    };

    {
        let mut cfg = state.auth.write().await;
        cfg.advertise_addr = normalized.clone();
        if let Err(e) = cfg.save() {
            return bad_request(format!("não deu para gravar pom.json: {e}"));
        }
    }

    state.network.set_advertise_addr(normalized.as_deref());
    info!("endereço anunciado atualizado pela interface: {normalized:?}");

    Json(json!({
        "advertise_addr": normalized,
        "effective_address": state.network.announce_addr(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct SaveTlsRequest {
    pub tls_required: bool,
}

/// Liga ou desliga o TLS entre peers, para o cluster inteiro.
///
/// Não precisa de reinício, e pode ser virado quantas vezes o operador quiser:
/// o servidor aceita as duas falas desde o arranque (`serve.rs`), e o que muda
/// aqui é qual delas os clientes usam e se texto claro remoto passa a ser
/// recusado. Os demais nós descobrem pela catraca de `tls_policy`, dentro de um
/// ciclo de gossip (5 s), nos dois sentidos.
async fn save_tls(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SaveTlsRequest>,
) -> Response {
    // A autoridade é verificada **aqui**, no servidor. Esconder o interruptor
    // na tela de um nó comum é conveniência; isto é o controle.
    if state.auth.read().await.role != crate::config::Role::Principal {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "só o nó principal decide o TLS do cluster." })),
        )
            .into_response();
    }

    {
        let mut cfg = state.auth.write().await;
        cfg.tls_required = req.tls_required;
        if let Err(e) = cfg.save() {
            return bad_request(format!(
                "não deu para gravar {}: {e}",
                crate::brand::config_filename()
            ));
        }
    }

    // A ordem importa pouco, mas as três têm que acontecer juntas: o esquema
    // das URLs e o cache de clientes, o que o anúncio diz, e se o servidor
    // passa a recusar texto claro de fora.
    state.peer_http.apply_cluster_policy(req.tls_required);
    state.network.set_tls(req.tls_required);
    if let Some(t) = &state.tls_server {
        t.set_strict(req.tls_required);
    }
    info!(
        "TLS entre peers {} pela interface",
        if req.tls_required {
            "ligado"
        } else {
            "desligado"
        }
    );

    Json(json!({ "tls_required": req.tls_required })).into_response()
}

async fn models(State(state): State<GatewayState>) -> impl IntoResponse {
    let catalog = local_catalog(&state).await;
    Json(json!({
        // Não `sysmon`: aquele é o hostname cru do sistema operacional e
        // ignora o `node_name` do pom.json. `network.hostname` é o nome que
        // este nó anuncia aos peers, e é o que o resto da tela mostra - ver a
        // nota gêmea em `node_info`. Quem lia daqui era o seletor "Enviar
        // para..." da tela de fatias, e ele era o único lugar do aplicativo
        // que chamava a máquina por outro nome.
        "hostname": state.network.hostname,
        "node_id": state.network.node_id,
        "models": catalog.models,
        "shard_sets": catalog.shard_sets,
        "scanned": catalog.scanned,
    }))
}

#[derive(Debug, Serialize, Deserialize)]
struct RttReport {
    source_node_id: String,
    measurements: Vec<crate::route::RttMeasurement>,
}

/// Publica apenas as medições feitas por este nó. A autenticação é necessária
/// porque o relatório faz parte da telemetria interna usada para escolher
/// rotas de inferência.
async fn network_rtt(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Json<RttReport> {
    let mut measurements = local_rtt_measurements(&state);
    measurements.sort_by(|a, b| a.target_node_id.cmp(&b.target_node_id));

    Json(RttReport {
        source_node_id: state.network.node_id.clone(),
        measurements,
    })
}

#[derive(Debug, Deserialize)]
struct ChatTelemetryRequest {
    #[serde(default)]
    node_ids: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct ChatTelemetryMeasurement {
    from_node_id: String,
    to_node_id: String,
    rtt_ms: f64,
    transport: String,
}

#[derive(Debug, Serialize)]
struct ChatTelemetryResponse {
    measurements: Vec<ChatTelemetryMeasurement>,
}

/// Combina a sonda local com os relatórios direcionais dos próprios peers.
///
/// A tela de implantação já usa esta mesma fonte para as arestas do plano,
/// mas o chat precisa de uma consulta sob demanda para a execução recém
/// terminada. Cada relatório é aceito somente quando o `source_node_id` bate
/// com o peer que respondeu: sem essa verificação, um gateway intermediário
/// poderia fazer uma medida parecer ser de outra máquina.
async fn chat_telemetry(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(request): Json<ChatTelemetryRequest>,
) -> Json<ChatTelemetryResponse> {
    let requested: std::collections::HashSet<String> = request
        .node_ids
        .into_iter()
        .filter(|node_id| !node_id.is_empty())
        .collect();
    if requested.len() < 2 {
        return Json(ChatTelemetryResponse {
            measurements: Vec::new(),
        });
    }

    let self_id = state.network.node_id.clone();
    let peers = state.all_peers().await;
    let connection_key = state.auth.read().await.connection_key.clone();
    let mut measurements = Vec::new();

    append_chat_telemetry_measurements(
        &mut measurements,
        &self_id,
        RttReport {
            source_node_id: self_id.clone(),
            measurements: local_rtt_measurements(&state),
        },
        &requested,
    );

    for peer in peers {
        if peer.node_id == self_id || !requested.contains(&peer.node_id) {
            continue;
        }
        let Some(base) = peer.gateway_url() else {
            continue;
        };
        let Ok(http) = state.peer_http.for_peer(&peer.node_id) else {
            continue;
        };
        let response = http
            .get(format!("{base}/api/ui/network/rtt"))
            .timeout(PEER_TIMEOUT)
            .bearer_auth(&connection_key)
            .send()
            .await;
        let Ok(response) = response else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(report) = response.json::<RttReport>().await else {
            continue;
        };
        append_chat_telemetry_measurements(&mut measurements, &peer.node_id, report, &requested);
    }

    measurements.sort_by(|a, b| {
        a.from_node_id
            .cmp(&b.from_node_id)
            .then_with(|| a.to_node_id.cmp(&b.to_node_id))
    });
    Json(ChatTelemetryResponse { measurements })
}

fn local_rtt_measurements(state: &GatewayState) -> Vec<crate::route::RttMeasurement> {
    let mut measurements = state.network.rtts.snapshot();
    let measured_targets: std::collections::HashSet<String> = measurements
        .iter()
        .map(|measurement| measurement.target_node_id.clone())
        .collect();
    for (node_id, route) in state.network.routes.snapshot() {
        if !measured_targets.contains(&node_id) {
            measurements.push(crate::route::RttMeasurement {
                target_node_id: node_id,
                rtt_ms: route.rtt_ms,
                transport: "direct-health".to_string(),
            });
        }
    }
    measurements
}

fn append_chat_telemetry_measurements(
    output: &mut Vec<ChatTelemetryMeasurement>,
    expected_source: &str,
    report: RttReport,
    requested: &std::collections::HashSet<String>,
) {
    if report.source_node_id != expected_source {
        return;
    }
    output.extend(
        report
            .measurements
            .into_iter()
            .filter(|measurement| {
                requested.contains(&measurement.target_node_id)
                    && measurement.target_node_id != expected_source
            })
            .map(|measurement| ChatTelemetryMeasurement {
                from_node_id: expected_source.to_string(),
                to_node_id: measurement.target_node_id,
                rtt_ms: measurement.rtt_ms,
                transport: measurement.transport,
            }),
    );
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

async fn cluster_fingerprint(state: &GatewayState, peers: &[PeerInfo]) -> String {
    let mut peer_state = serde_json::to_value(peers).expect("peers da UI devem serializar");
    if let Some(peers) = peer_state.as_array_mut() {
        for peer in peers {
            // Liveness is represented by presence in this list. The timestamp
            // itself is not rendered and would invalidate the cache on every
            // heartbeat, including the synthetic self peer.
            if let Some(peer) = peer.as_object_mut() {
                peer.remove("last_seen");
            }
        }
    }
    let mut loads = Vec::with_capacity(peers.len());
    for peer in peers {
        loads.push(json!({
            "node_id": peer.node_id,
            "load": state.loads.load(&peer.node_id).await,
        }));
    }
    let mut routes = state
        .network
        .routes
        .snapshot()
        .into_iter()
        .map(|(node_id, route)| {
            json!({
                "node_id": node_id,
                "addr": route.addr,
                "rtt_ms": route.rtt_ms,
            })
        })
        .collect::<Vec<_>>();
    routes.sort_by(|left, right| left["node_id"].as_str().cmp(&right["node_id"].as_str()));

    etag_for_json(&json!({
        "self_id": state.network.node_id,
        "peers": peer_state,
        "loads": loads,
        "routes": routes,
        "runtime": state.runtime.status().await,
        "config": state.runtime.config().await,
        "active_model": state.active_model().await,
        "detached": state.network.is_detached(),
    }))
}

/// Sampling time is not a UI fact. Keep the actual metrics in the version, but
/// remove only this timestamp so a fresh sample with identical values does not
/// invalidate the semantic cluster version.
fn cluster_etag(body: &serde_json::Value) -> String {
    let mut normalized = body.clone();
    if let Some(node) = normalized.get_mut("node") {
        remove_system_sample_time(node);
    }
    if let Some(peers) = normalized
        .get_mut("peers")
        .and_then(|value| value.as_array_mut())
    {
        for peer in peers {
            if let Some(node) = peer.get_mut("node") {
                remove_system_sample_time(node);
            }
        }
    }
    etag_for_json(&normalized)
}

fn remove_system_sample_time(node: &mut serde_json::Value) {
    if let Some(system) = node
        .get_mut("system")
        .and_then(|value| value.as_object_mut())
    {
        system.remove("sampled_at");
    }
}

async fn cluster(headers: HeaderMap, State(state): State<GatewayState>) -> Response {
    let peers = state.all_peers().await;
    let fingerprint = cluster_fingerprint(&state, &peers).await;
    let mut cache = state.cluster_cache.lock().await;
    if let Some(snapshot) = cache.get(&fingerprint, Instant::now(), CLUSTER_CACHE_TTL) {
        return conditional_json_with_version(
            &headers,
            &snapshot.body,
            &snapshot.etag,
            snapshot.version,
        );
    }
    let self_id = state.network.node_id.clone();

    // Fan out to every peer at once; a slow peer must not serialise the others.
    let futures: Vec<_> = peers
        .iter()
        .map(|p| {
            let is_self = p.node_id == self_id;
            let url = p.gateway_url();
            // Cliente pinado no node_id do destino: com o cluster em TLS, o
            // compartilhado não tem como verificar o certificado do peer e toda
            // chamada de controle falharia - foi assim que a tela de implantar
            // parou de enxergar as fatias dos outros nós.
            let http = state.peer_http.for_peer(&p.node_id);
            async move {
                if is_self || url.is_none() {
                    return (None, None);
                }
                let http = match http {
                    Ok(c) => c,
                    Err(e) => return (None, Some(format!("sem cliente: {e}"))),
                };
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
        crate::pipeline::plan_pipeline(&peers, &active, None, total, &state.loads)
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

    let body = json!({
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
    });

    let etag = cluster_etag(&body);
    let snapshot = cache.store(fingerprint, body, etag, Instant::now());
    conditional_json_with_version(&headers, &snapshot.body, &snapshot.etag, snapshot.version)
}

/// Como uma máquina carregou o modelo: o `/health` do servidor de inferência
/// dela, cru.
///
/// Fica fora do `/api/ui/cluster` de propósito. O Painel recarrega o cluster em
/// ciclo curto, e isto é detalhe que só interessa com o card aberto: buscar por
/// demanda evita pagar um `/health` por peer a cada atualização de tela, e um
/// peer que não responde deixa de derrubar a listagem inteira.
///
/// **Vale para qualquer nó, inclusive este.** A entrada local do `all_peers`
/// tem `inference_url` como as outras, e é o mesmo caminho que o
/// `ready_for_pipeline` já percorre - um só código, então o card do nó local
/// nunca mostra menos que o card de um remoto.
async fn peer_runtime(State(state): State<GatewayState>, Path(node_id): Path<String>) -> Response {
    let Some(peer) = state
        .all_peers()
        .await
        .into_iter()
        .find(|p| p.node_id == node_id)
    else {
        return (StatusCode::NOT_FOUND, "máquina desconhecida").into_response();
    };

    let Some(base) = peer.inference_url() else {
        // O peer existe e não serve inferência: não é erro de rede, é o
        // estado normal de uma máquina sem fatia implantada.
        return Json(json!({ "loaded": false, "reason": "sem servidor de inferência" }))
            .into_response();
    };

    let http = match state.peer_http.for_peer(&node_id) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("sem cliente para {node_id}: {e}"),
            )
                .into_response()
        }
    };

    let url = format!("{base}/health");
    match tokio::time::timeout(PEER_TIMEOUT, http.get(&url).send()).await {
        Ok(Ok(r)) if r.status().is_success() => match r.json::<serde_json::Value>().await {
            Ok(v) => Json(json!({ "loaded": true, "health": v })).into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, format!("resposta inválida: {e}")).into_response(),
        },
        Ok(Ok(r)) => Json(json!({ "loaded": false, "reason": format!("HTTP {}", r.status()) }))
            .into_response(),
        Ok(Err(e)) => Json(json!({ "loaded": false, "reason": e.to_string() })).into_response(),
        Err(_) => Json(json!({ "loaded": false, "reason": "timeout" })).into_response(),
    }
}

/// Each peer's installed models, so the setup screen can say who is missing what.
async fn cluster_models(State(state): State<GatewayState>) -> impl IntoResponse {
    let peers = state.all_peers().await;
    let self_id = state.network.node_id.clone();
    let local = local_catalog(&state).await;
    // O nome que este nó anuncia, não o hostname cru do SO. Ver `models`.
    let local_host = state.network.hostname.clone();

    let futures: Vec<_> = peers
        .iter()
        .filter(|p| p.node_id != self_id)
        .map(|p| {
            let url = p.gateway_url();
            let http = state.peer_http.for_peer(&p.node_id);
            let node_id = p.node_id.clone();
            async move {
                let Some(base) = url else {
                    return (
                        node_id,
                        None,
                        Some("sem porta de gateway anunciada".to_string()),
                    );
                };
                let http = match http {
                    Ok(c) => c,
                    Err(e) => return (node_id, None, Some(format!("sem cliente: {e}"))),
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
            // Um peer que não respondeu continua sendo uma máquina com nome:
            // a tabela local de peers guarda o que ele anunciou. Sem isto o
            // objeto saía **sem `hostname`** e a tela caía no `node_id`, ou
            // seja, um prefixo hexadecimal no lugar do nome.
            None => out.push(json!({
                "node_id": node_id,
                "hostname": peers
                    .iter()
                    .find(|p| p.node_id == node_id)
                    .map(|p| p.hostname.clone())
                    .filter(|h| !h.is_empty()),
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
    /// Parâmetros usados para cobrar o KV junto com o peso. Ausentes usam os
    /// defaults da tela de runtime e os tipos globais da instalação.
    #[serde(default)]
    pub n_ctx: Option<u32>,
    #[serde(default)]
    pub max_sessions: Option<u32>,
    #[serde(default)]
    pub cache_type_k: Option<KvCacheType>,
    #[serde(default)]
    pub cache_type_v: Option<KvCacheType>,
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
            // Resolvido antes de mover `req`: `target` empresta dele.
            let cliente = match state.peer_http.for_peer(target) {
                Ok(c) => c,
                Err(e) => return bad_request(format!("sem cliente para o peer {target}: {e}")),
            };
            let mut forwarded = req;
            // Clear the hop marker so the receiving peer measures locally.
            forwarded.layout_node_id = None;
            return match cliente
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
    let layout_path = path.clone();
    let layout = match tokio::task::spawn_blocking(move || gguf::read_layout(&layout_path)).await {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => return bad_request(format!("não foi possível ler o GGUF: {e}")),
        Err(e) => return bad_request(format!("leitura do GGUF falhou: {e}")),
    };

    let info = match tokio::task::spawn_blocking(move || gguf::read_info(&path)).await {
        Ok(Ok(info)) => info,
        Ok(Err(e)) => {
            return bad_request(format!("não foi possível ler a geometria KV do GGUF: {e}"))
        }
        Err(e) => return bad_request(format!("leitura da geometria KV falhou: {e}")),
    };

    let reserve = req.reserve_pct.unwrap_or(DEFAULT_RESERVE_PCT);
    let defaults = RuntimeParams::default();
    let installation = state.auth.read().await;
    let kv = plan::KvPlanParams {
        n_ctx: req.n_ctx.unwrap_or(defaults.n_ctx),
        max_sessions: req.max_sessions.unwrap_or(defaults.max_sessions),
        cache_type_k: req.cache_type_k.unwrap_or(installation.cache_type_k),
        cache_type_v: req.cache_type_v.unwrap_or(installation.cache_type_v),
    };
    match plan::plan_split_with_kv(&req.model_path, &layout, &info, &req.budgets, reserve, kv) {
        Ok(p) => Json(p).into_response(),
        Err(e) => bad_request(e.to_string()),
    }
}

// ── Applying a configuration ─────────────────────────────────────────────────

/// Which inference engine the operator asks the runtime to use for a model.
/// `Auto` keeps the runtime's model-metadata decision; the explicit variants
/// are persisted with the deployment so every peer receives the same choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceMode {
    #[default]
    Auto,
    Dense,
    Moe,
}

/// Sampling and context knobs. Defaults match `run-p1.sh`, which is the
/// configuration the two-peer cluster was validated with.
///
/// `PartialEq` porque o registro de implantação guarda uma cópia por nó
/// (`deployment::DeploymentNode::config`) e comparar dois registros é como se
/// verifica que uma reimplantação mudou alguma coisa.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RuntimeParams {
    /// Engine preference. `Auto` is backwards-compatible and lets the runtime
    /// choose from the actual GGUF metadata.
    #[serde(default)]
    pub inference_mode: InferenceMode,
    #[serde(default = "d_gpu_layers")]
    pub n_gpu_layers: u32,
    #[serde(default = "d_n_ctx")]
    pub n_ctx: u32,
    #[serde(default = "d_true")]
    pub offload_kqv: bool,
    /// Deixa o `common_fit_params` do llama.cpp ajustar a alocação à memória
    /// livre. No TP o runtime desliga esta opção, porque o fit nativo não
    /// suporta `split-mode tensor`.
    #[serde(default = "d_true")]
    pub fit_memory: bool,
    #[serde(default = "d_n_batch")]
    pub n_batch: u32,
    #[serde(default = "d_max_new")]
    pub max_new_tokens: u32,
    /// Generations allowed in flight at once. Each generation gets the full
    /// context window; raising this value multiplies the KV-cache reservation.
    #[serde(default = "d_max_sessions")]
    pub max_sessions: u32,
    #[serde(default = "d_batching_enabled")]
    pub batching_enabled: bool,
    #[serde(default = "d_max_queue_requests")]
    pub max_queue_requests: u32,
    #[serde(default = "d_batch_window_ms")]
    pub batch_window_ms: u32,
    #[serde(default = "d_max_batch_sequences")]
    pub max_batch_sequences: u32,
    #[serde(default = "d_max_batch_tokens")]
    pub max_batch_tokens: u32,
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
    /// Predição multi-token pelo bloco `nextn` do modelo. Interruptor
    /// separado do de cima de propósito: são mecanismos diferentes, e só
    /// este usa peso treinado do modelo. Ver `crate::mtp`.
    #[serde(default)]
    pub mtp_enabled: bool,
    #[serde(default = "d_mtp_draft_tokens")]
    pub mtp_draft_tokens: u32,
    #[serde(default = "d_mtp_min_probability")]
    pub mtp_min_probability: f32,
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
fn d_batching_enabled() -> bool {
    true
}
fn d_max_queue_requests() -> u32 {
    128
}
fn d_batch_window_ms() -> u32 {
    4
}
fn d_max_batch_sequences() -> u32 {
    u32::MAX
}
fn d_max_batch_tokens() -> u32 {
    u32::MAX
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
fn d_mtp_draft_tokens() -> u32 {
    3
}
fn d_mtp_min_probability() -> f32 {
    0.0
}

impl Default for RuntimeParams {
    fn default() -> Self {
        Self {
            inference_mode: InferenceMode::Auto,
            n_gpu_layers: d_gpu_layers(),
            n_ctx: d_n_ctx(),
            offload_kqv: d_true(),
            fit_memory: d_true(),
            n_batch: d_n_batch(),
            max_new_tokens: d_max_new(),
            max_sessions: d_max_sessions(),
            batching_enabled: d_batching_enabled(),
            max_queue_requests: d_max_queue_requests(),
            batch_window_ms: d_batch_window_ms(),
            max_batch_sequences: d_max_batch_sequences(),
            max_batch_tokens: d_max_batch_tokens(),
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
            mtp_enabled: false,
            mtp_draft_tokens: d_mtp_draft_tokens(),
            mtp_min_probability: d_mtp_min_probability(),
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
            inference_mode: c.inference_mode,
            n_gpu_layers: c.n_gpu_layers,
            n_ctx: c.n_ctx,
            offload_kqv: c.offload_kqv,
            fit_memory: c.fit_memory,
            n_batch: c.n_batch,
            max_new_tokens: c.max_new_tokens,
            max_sessions: c.max_sessions,
            batching_enabled: c.batching_enabled,
            max_queue_requests: c.max_queue_requests,
            batch_window_ms: c.batch_window_ms,
            max_batch_sequences: c.max_batch_sequences,
            max_batch_tokens: c.max_batch_tokens,
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
            mtp_enabled: c.mtp_enabled,
            mtp_draft_tokens: c.mtp_draft_tokens,
            mtp_min_probability: c.mtp_min_probability,
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
    /// Nome operacional da implantação, separado da identidade técnica do
    /// modelo. Vazio mantém compatibilidade com chamadas antigas.
    #[serde(default)]
    pub deployment_name: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    pub peer_count: u32,
    pub peer_index: u32,
    #[serde(default)]
    pub primary: bool,
    /// Identidade de tensor parallelism deste nó (caminho C). `None` = nó sem
    /// TP. Presente, **todo** rank extrai o próprio shard de tensor - o rank 0
    /// inclusive. O `role` só diz quem hospeda a coletiva (`rank0_url`); ver
    /// `tp_shard_to_extract`.
    #[serde(default)]
    pub tp: Option<crate::network::TpIdentity>,
    /// Set when the source is already a shard (or covers the whole model) and
    /// no slicing is needed.
    #[serde(default)]
    pub skip_split: bool,
    /// A implantação de que este nó vai fazer parte. Ver `deployment.rs`.
    #[serde(default)]
    pub deploy_id: String,
    /// Hash da parte que este nó tem que carregar, quando quem manda já sabe
    /// qual é - o caso normal numa implantação, em que o `.pom` já existe.
    ///
    /// É o que faz o reaproveitamento ser uma pergunta sobre **conteúdo** em
    /// vez de sobre nome de arquivo: ou a parte com este hash está no disco, ou
    /// ela não está, e adivinhar não é uma das opções.
    #[serde(default)]
    pub part_sha256: Option<String>,
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

    fn no_embeddings_evaluation(&self) -> crate::capabilities::NoEmbeddingsEvaluation {
        crate::capabilities::evaluate_no_embeddings(&[crate::capabilities::PeerTopology {
            node_id: self.base_name.clone(),
            start_layer: Some(self.start_layer),
            end_layer: Some(self.end_layer),
            total_layers: Some(self.total_layers),
            peer_index: Some(self.peer_index),
            peer_count: Some(self.peer_count),
        }])
    }
}

fn model_label(req: &ApplyRequest) -> String {
    req.model_label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::model::ModelProfile::guess_from_name(&req.base_name).name)
}

fn deployment_name(req: &ApplyRequest) -> String {
    normalize_deployment_name(Some(&req.deployment_name), &req.base_name)
        .unwrap_or_else(|_| req.base_name.clone())
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
    /// Campos opcionais usados por downloads de modelos remotos. Jobs de
    /// fatiamento e aplicação permanecem com estes campos ausentes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_downloaded: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_bytes_per_sec: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
        repo_id: None,
        revision: None,
        filename: None,
        quantization: None,
        download_status: None,
        bytes_downloaded: None,
        total_bytes: None,
        speed_bytes_per_sec: None,
        eta_seconds: None,
        local_path: None,
        error: None,
    }
}

/// Atualiza o estado rico de um download sem criar outro armazenamento de
/// progresso. O semáforo e o token vivem no serviço do provider; este job é a
/// fonte de verdade que a UI consulta com ETag.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn set_model_download_progress(
    id: &str,
    status: &'static str,
    progress: Option<f64>,
    bytes_downloaded: Option<u64>,
    total_bytes: Option<u64>,
    speed_bytes_per_sec: Option<u64>,
    eta_seconds: Option<u64>,
    local_path: Option<String>,
    error: Option<String>,
) {
    let terminal = matches!(status, "completed" | "cancelled" | "failed");
    let state = match status {
        "completed" => "done",
        "cancelled" | "failed" => "failed",
        _ => "running",
    };
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(id) {
        job.state = state;
        job.download_status = Some(status);
        job.progress = progress.map(|pct| pct.clamp(0.0, 100.0));
        job.bytes_downloaded = bytes_downloaded;
        job.total_bytes = total_bytes;
        job.speed_bytes_per_sec = speed_bytes_per_sec;
        job.eta_seconds = eta_seconds;
        if local_path.is_some() {
            job.local_path = local_path;
        }
        if error.is_some() {
            job.error = error.clone();
            job.message = error.unwrap_or_default();
        }
        job.step = status.to_string();
        if terminal {
            job.finished_at = Some(chrono::Utc::now().to_rfc3339());
            if status == "completed" {
                job.progress = Some(100.0);
            }
        }
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
async fn list_jobs(headers: HeaderMap) -> Response {
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
    conditional_json(&headers, &json!({ "jobs": list }))
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
        if job.progress.is_none_or(|current| pct > current) {
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
    if req.params.no_embeddings {
        if let Err(reason) =
            crate::capabilities::validate_no_embeddings(&req.no_embeddings_evaluation(), true)
        {
            return bad_request(reason);
        }
    }
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

/// Qual shard de tensor este apply tem que extrair, se algum.
///
/// **Todo** rank de um grupo tensor-parallel extrai o próprio, o rank 0
/// inclusive. O `role` (`primary`/`worker`) diz quem hospeda a coletiva
/// (`rank0_url`), **não** quem fatia - e confundir as duas coisas foi o defeito
/// de 17/08: a condição era `role == "worker"`, então o primary caía no ramo
/// `skip_split` e servia o GGUF inteiro, rodando atenção e FFN em largura cheia
/// contra workers em meia largura. O `_tp0of2.gguf` nunca chegava a existir no
/// disco, e o `_tp1of2.gguf` do worker era a única metade do grupo.
fn tp_shard_to_extract(req: &ApplyRequest) -> Option<&crate::network::TpIdentity> {
    req.tp.as_ref().filter(|_| !req.skip_split)
}

async fn source_model_sha256(path: &std::path::Path) -> anyhow::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::pom::hash_file(&path).map(|(sha256, _)| sha256))
        .await
        .context("calcular o hash do modelo original")?
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ModelIdentityRequest {
    pub path: String,
    #[serde(default = "default_true")]
    pub hash: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelIdentityResponse {
    pub canonical_path: String,
    pub sha256: Option<String>,
}

/// Proves the identity of a file selected for deployment without hashing the
/// catalog during ordinary polling. The path must already be in this peer's
/// catalog, so a caller cannot turn the route into an arbitrary file reader.
async fn model_identity(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ModelIdentityRequest>,
) -> Response {
    match model_identity_local(&state, &req.path, req.hash).await {
        Ok(identity) => Json(identity).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(crate) async fn model_identity_local(
    state: &GatewayState,
    requested: &str,
    calculate_hash: bool,
) -> Result<ModelIdentityResponse, String> {
    let requested = std::path::Path::new(requested);
    let canonical = requested
        .canonicalize()
        .map_err(|e| format!("não foi possível resolver o arquivo do modelo: {e}"))?;
    let catalog = local_catalog(state).await;
    let listed = catalog.models.iter().any(|model| {
        std::path::Path::new(&model.path)
            .canonicalize()
            .ok()
            .is_some_and(|path| path == canonical)
    });
    if !listed {
        return Err(format!(
            "o arquivo {} não pertence ao catálogo deste peer",
            requested.display()
        ));
    }
    let sha256 = if calculate_hash {
        Some(
            source_model_sha256(&canonical)
                .await
                .map_err(|e| format!("não foi possível calcular a identidade do GGUF: {e}"))?,
        )
    } else {
        None
    };
    Ok(ModelIdentityResponse {
        canonical_path: canonical.to_string_lossy().into_owned(),
        sha256,
    })
}

/// Sela uma parte cortada aqui no manifesto do modelo original.
///
/// O `apply` corta **uma** faixa, para esta máquina. Antes isso ia parar numa
/// pasta com o nome do modelo e sem manifesto nenhum, e o catálogo mostrava um
/// `<sha256>.gguf` órfão que não dizia de que modelo era. Um manifesto de uma
/// parte só resolve as duas coisas: dá nome à fatia e a coloca na pasta
/// compartilhada do modelo, sem perder alternativas anteriores.
fn seal_single_part(
    shard_dir: &std::path::Path,
    scratch: &std::path::Path,
    req: &ApplyRequest,
    outcome: &plan::SliceOutcome,
    source_sha256: impl Into<String>,
    kind: crate::pom::PomKind,
    tp: Option<crate::pom::PomTp>,
    node_id: &str,
) -> anyhow::Result<String> {
    let parte = crate::pom::PomPart {
        sha256: outcome.sha256.clone(),
        bytes: outcome.bytes,
        start_layer: req.start_layer,
        end_layer: req.end_layer,
        is_first: req.is_first(),
        is_last: req.is_last(),
        tp,
    };
    let mut manifesto = crate::pom::PomManifest::new(
        crate::pom::PomModel {
            name: req.base_name.clone(),
            label: model_label(req),
            total_layers: req.total_layers,
            quant: std::path::Path::new(&req.source_model_path)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(crate::catalog::parse_quant),
        },
        kind,
        node_id,
        source_sha256,
        Vec::new(),
    );
    match kind {
        crate::pom::PomKind::TensorParallel => manifesto.tp_parts.push(parte),
        crate::pom::PomKind::Layers => manifesto.parts.push(parte),
        crate::pom::PomKind::Combined => {
            anyhow::bail!("uma parte isolada não pode selar um manifesto combinado")
        }
    }
    let destino = crate::pom::finalize_run(shard_dir, scratch, &manifesto)?;
    Ok(crate::pom::part_path(&destino, &outcome.sha256)?
        .to_string_lossy()
        .into_owned())
}

async fn run_apply(state: GatewayState, req: ApplyRequest, job_id: &str) -> anyhow::Result<String> {
    let model_path = if let Some(tp) = tp_shard_to_extract(&req) {
        set_job(
            job_id,
            "running",
            "fatiando",
            &format!("extraindo shard de tensor rank {}/{}", tp.rank, tp.size),
        )
        .await;
        let shard_dir = state.shard_dir().await;
        let source_sha256 =
            source_model_sha256(std::path::Path::new(&req.source_model_path)).await?;
        // Já tenho esta parte? A busca é por conteúdo, em todas as rodadas -
        // a pasta de destino depende do manifesto, que só existe depois.
        if let Some(pronta) = req
            .part_sha256
            .as_deref()
            .and_then(|sha| crate::transfer::find_part(&shard_dir, sha))
        {
            info!("shard de tensor já em disco: {}", pronta.display());
            pronta.to_string_lossy().into_owned()
        } else {
            let scratch = crate::pom::scratch_dir(&shard_dir);
            let progress_job = job_id.to_string();
            let outcome = plan::extract_tp_shard(
                &req.source_model_path,
                &scratch,
                None,
                tp.size,
                tp.rank,
                move |pct| {
                    let id = progress_job.clone();
                    tokio::spawn(async move { set_job_progress(&id, pct).await });
                },
            )
            .await?;
            let caminho = seal_single_part(
                &shard_dir,
                &scratch,
                &req,
                &outcome,
                source_sha256,
                crate::pom::PomKind::TensorParallel,
                Some(crate::pom::PomTp {
                    rank: tp.rank,
                    size: tp.size,
                }),
                &state.network.node_id,
            )?;
            invalidate_catalog().await;
            caminho
        }
    } else if let Some(parts) = req.merge.clone() {
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

        let shard_dir = state.shard_dir().await;
        let source_sha256 =
            source_model_sha256(std::path::Path::new(&req.source_model_path)).await?;
        if let Some(pronta) = req
            .part_sha256
            .as_deref()
            .and_then(|sha| crate::transfer::find_part(&shard_dir, sha))
        {
            info!("fatia já em disco: {}", pronta.display());
            pronta.to_string_lossy().into_owned()
        } else {
            let scratch = crate::pom::scratch_dir(&shard_dir);
            let progress_job = job_id.to_string();
            let outcome = plan::extract_shard(
                &req.source_model_path,
                &scratch,
                None,
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
            let caminho = seal_single_part(
                &shard_dir,
                &scratch,
                &req,
                &outcome,
                source_sha256,
                crate::pom::PomKind::Layers,
                None,
                &state.network.node_id,
            )?;
            invalidate_catalog().await;

            info!(
                "shard pronto: {} ({:.1} GB)",
                caminho,
                outcome.bytes as f64 / 1_073_741_824.0
            );
            caminho
        }
    };

    let shard_dir = state.shard_dir().await;
    let shard_root = shard_dir
        .canonicalize()
        .unwrap_or_else(|_| shard_dir.clone());
    if std::path::Path::new(&model_path)
        .canonicalize()
        .ok()
        .is_some_and(|path| path.starts_with(&shard_root))
    {
        enforce_shard_storage(&state, &model_path).await?;
    }

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
    let deployment = deployment_name(&req);

    let inference_port = state
        .network
        .inference_port
        .ok_or_else(|| anyhow::anyhow!("este nó subiu sem --inference-port"))?;

    let cfg = NodeConfig {
        deploy_id: req.deploy_id.clone(),
        model_path: model_path.clone(),
        model_label: label,
        deployment_name: deployment,
        start_layer: req.start_layer,
        end_layer: req.end_layer,
        total_layers: req.total_layers,
        peer_count: req.peer_count.max(1),
        peer_index: req.peer_index,
        primary: req.primary,
        tp: req.tp.clone(),
        inference_mode: req.params.inference_mode,
        inference_port,
        single_peer: req.single_peer(),
        no_embeddings: req.no_embeddings(),
        ..NodeConfig::from_params(&req.params, req.tp.is_none())
    };

    // Liga/desliga a coletiva TP antes de subir o servidor: o callback tem que
    // estar registrado no llama.cpp antes do primeiro decode. Reaplicar muda o
    // grupo só num reinício (o estado do tp.rs é OnceLock, por desenho da v1).
    match &req.tp {
        Some(tp) if tp.size > 1 && tp.rank0_url.is_some() => {
            let rank0 = tp.members.first();
            crate::tp::register_tp_reduce_with_urls(
                tp.rank,
                tp.size,
                tp.rank0_url.clone().unwrap(),
                rank0
                    .map(|member| member.url_candidates.clone())
                    .unwrap_or_default(),
                rank0
                    .map(|member| member.addr_candidates.clone())
                    .unwrap_or_default(),
                tp.rank0_node_id.clone(),
                rank0.and_then(|m| m.libp2p_peer_id.clone()),
                rank0.is_some_and(|m| m.supports_tunnel),
                Some(state.auth.clone()),
            );
        }
        _ => crate::tp::unregister_tp_reduce(),
    }

    state.runtime.apply(cfg).await?;
    state.network.set_tp(req.tp.clone());
    Ok(model_path)
}

async fn enforce_shard_storage(
    state: &GatewayState,
    current_model_path: &str,
) -> anyhow::Result<()> {
    let (shard_dir, max_bytes, active_path) = {
        let cfg = state.auth.read().await;
        let active = state.runtime.config().await.map(|c| c.model_path);
        (cfg.shard_dir_path(), cfg.shard_max_bytes(), active)
    };
    let mut protected = vec![std::path::PathBuf::from(current_model_path)];
    if let Some(active) = active_path {
        protected.push(std::path::PathBuf::from(active));
    }
    let removed = tokio::task::spawn_blocking(move || {
        crate::storage::enforce_limit(&shard_dir, max_bytes, &protected)
    })
    .await??;
    if !removed.is_empty() {
        info!(
            "retenção de fatias removeu {} diretório(s) antigo(s)",
            removed.len()
        );
        invalidate_catalog().await;
    }
    Ok(())
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
    // Reaproveitar a junção exige saber o hash do resultado, e quem manda o
    // pedido sabe: é a mesma parte que o `.pom` lista. A busca é por conteúdo,
    // em todas as rodadas - a pasta depende do manifesto, que só existe depois.
    if let Some(caminho) = req
        .part_sha256
        .as_deref()
        .and_then(|sha| crate::transfer::find_part(&shard_dir, sha))
    {
        info!(
            "modelo junto já existe, reaproveitando: {}",
            caminho.display()
        );
        invalidate_catalog().await;
        return Ok(caminho.to_string_lossy().into_owned());
    }
    // O nome da pasta vem do manifesto do modelo original associado às partes.
    let out_dir = crate::pom::scratch_dir(&shard_dir);

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
    // Write to a temporary name and rename when complete, so a merge cut short
    // (kill, power loss) never leaves a half file under the final name where a
    // later apply would reuse it as a finished model. The unique suffix also
    // keeps two concurrent merges of the same range from interleaving writes.
    // O nome final só se conhece no fim: é o hash do que saiu.
    std::fs::create_dir_all(&out_dir).with_context(|| format!("criar {}", out_dir.display()))?;
    let temp = out_dir.join(format!("{}.part", uuid::Uuid::new_v4().simple()));
    let temp_cleanup = temp.clone();
    let progress_job = job_id.to_string();
    let destino_dir = out_dir.clone();

    let (output, bytes) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(std::path::PathBuf, u64)> {
            let escrito = crate::splitter::merge(&merge_parts, &temp, move |pct| {
                let id = progress_job.clone();
                tokio::spawn(async move { set_job_progress(&id, pct).await });
            })?;
            let dst = crate::pom::part_path(&destino_dir, &escrito.sha256)?;
            // Um destino já ocupado é outra junção que terminou primeiro. Com
            // endereço de conteúdo os dois arquivos são byte a byte o mesmo,
            // então ficar com o que já está pronto é o certo - e no Windows o
            // rename nem substituiria.
            if dst.exists() {
                let _ = std::fs::remove_file(&temp);
                return Ok((dst, escrito.bytes));
            }
            std::fs::rename(&temp, &dst)
                .with_context(|| format!("mover {} para {}", temp.display(), dst.display()))?;
            Ok((dst, escrito.bytes))
        })
        .await
        .context("spawn_blocking para splitter")?
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp_cleanup);
        })?;

    // A junção é uma parte nova, e uma parte sem manifesto é um arquivo de nome
    // ilegível no catálogo. Um manifesto de uma parte só a nomeia e põe a
    // manifesto na pasta do modelo.
    let sha = output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let parte = crate::pom::PomPart {
        sha256: sha.clone(),
        bytes,
        start_layer: start,
        end_layer: end,
        is_first: start == 0,
        is_last: end + 1 >= req.total_layers,
        tp: None,
    };
    let source_ids: Vec<String> = parts
        .iter()
        .filter_map(|part| {
            crate::pom::manifest_for_part_path(std::path::Path::new(&part.source_model_path))
                .map(|manifest| manifest.pom_id)
        })
        .collect();
    let source_sha256 = source_ids.first().cloned();
    if source_ids
        .iter()
        .any(|id| Some(id) != source_sha256.as_ref())
    {
        anyhow::bail!("as fatias pertencem a modelos originais diferentes");
    }
    let source_sha256 = source_sha256
        .ok_or_else(|| anyhow::anyhow!("as fatias não têm manifesto do modelo original"))?;
    let mut manifesto = crate::pom::PomManifest::new(
        crate::pom::PomModel {
            name: req.base_name.clone(),
            label: model_label(req),
            total_layers: req.total_layers,
            quant: std::path::Path::new(&req.source_model_path)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(crate::catalog::parse_quant),
        },
        crate::pom::PomKind::Layers,
        String::new(),
        source_sha256,
        Vec::new(),
    );
    manifesto.parts.push(parte);
    let pasta = crate::pom::finalize_run(&shard_dir, &out_dir, &manifesto)?;
    let final_path = crate::pom::part_path(&pasta, &sha)?;

    invalidate_catalog().await;

    info!(
        "modelo junto pronto: {} ({:.1} GB)",
        final_path.display(),
        bytes as f64 / 1_073_741_824.0
    );
    Ok(final_path.to_string_lossy().into_owned())
}

// ── Slicing a whole model, here, on demand ───────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SliceMode {
    Pipeline,
    TensorParallel,
}

impl Default for SliceMode {
    fn default() -> Self {
        Self::Pipeline
    }
}

#[derive(Debug, Serialize)]
pub struct TpSliceRank {
    pub rank: u32,
    pub start_layer: u32,
    pub end_layer: u32,
}

#[derive(Debug, Serialize)]
pub struct TpSlicePlan {
    pub mode: SliceMode,
    pub model_bytes: u64,
    pub total_layers: u32,
    pub tp_size: u32,
    pub ranks: Vec<TpSliceRank>,
    pub complete: bool,
    pub blocked: bool,
    pub warnings: Vec<String>,
}

/// Validate a TP cut once and give the UI a useful preview without writing
/// any shard. Every rank contains the complete layer range; the tensor
/// columns and rows, not the layers, are what differ between ranks.
fn tensor_slice_plan(path: &std::path::Path, tp_size: u32) -> anyhow::Result<TpSlicePlan> {
    if !(2..=64).contains(&tp_size) {
        anyhow::bail!("escolha entre 2 e 64 ranks TP");
    }
    crate::splitter::validate_tensor_parallel(path, tp_size)?;
    let layout = crate::gguf::read_layout(path)?;
    let total_layers = layout.layer_count();
    if total_layers == 0 {
        anyhow::bail!("este arquivo não declara camadas");
    }
    Ok(TpSlicePlan {
        mode: SliceMode::TensorParallel,
        model_bytes: layout.total_bytes,
        total_layers,
        tp_size,
        ranks: (0..tp_size)
            .map(|rank| TpSliceRank {
                rank,
                start_layer: 0,
                end_layer: total_layers - 1,
            })
            .collect(),
        complete: true,
        blocked: false,
        warnings: Vec::new(),
    })
}

fn tensor_slice_manifest(
    base_name: String,
    label: String,
    total_layers: u32,
    node_id: String,
    source_sha256: String,
    quant: Option<String>,
    tp_parts: Vec<crate::pom::PomPart>,
) -> crate::pom::PomManifest {
    let mut manifest = crate::pom::PomManifest::new(
        crate::pom::PomModel {
            name: base_name,
            label,
            total_layers,
            quant,
        },
        crate::pom::PomKind::TensorParallel,
        node_id,
        source_sha256,
        Vec::new(),
    );
    manifest.tp_parts = tp_parts;
    manifest
}

#[derive(Debug, Deserialize)]
pub struct SliceRequest {
    /// Whole model on *this* machine.
    pub model_path: String,
    /// Target size of each slice, in order. A slice with no size takes what is
    /// left of the model. Tensor Parallelism does not use this list.
    #[serde(default)]
    pub slices: Vec<plan::SizedSlice>,
    /// Catalog name of the model, used to name the slices. Without it the name
    /// comes from the file, which for an Ollama blob is a digest.
    #[serde(default)]
    pub name: Option<String>,
    /// Pipeline layers or tensor-parallel ranks.
    #[serde(default)]
    pub mode: SliceMode,
    /// Number of TP ranks when `mode` is `tensor_parallel`.
    #[serde(default)]
    pub tp_size: Option<u32>,
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
    #[serde(default)]
    pub mode: SliceMode,
    #[serde(default)]
    pub tp_size: Option<u32>,
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

/// Checa, sem escrever nada, se um modelo local dá para partir por tensor
/// parallelism no caminho C (classificação dos tensores, divisibilidade das
/// dimensões, weight-tying). O coordenador do deploy chama isto no primary
/// antes de aceitar o grupo; a resposta é sempre 200, com `ok: false` + `reason`
/// quando o modelo não serve — a checagem em si é uma resposta válida, não um
/// erro HTTP.
#[derive(Debug, Deserialize)]
pub struct TpCheckRequest {
    pub model_path: String,
    pub tp_size: u32,
}

async fn tp_check(_auth: ConnectionAuth, Json(req): Json<TpCheckRequest>) -> Response {
    let path = std::path::Path::new(&req.model_path);
    if !path.is_file() {
        return Json(json!({
            "ok": false,
            "reason": format!("modelo não encontrado nesta máquina: {}", req.model_path),
        }))
        .into_response();
    }
    match crate::splitter::validate_tensor_parallel(path, req.tp_size) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => Json(json!({ "ok": false, "reason": e.to_string() })).into_response(),
    }
}

/// What a list of target sizes turns into: layer ranges, real sizes, coverage.
async fn slice_plan(_auth: ConnectionAuth, Json(req): Json<SlicePlanRequest>) -> Response {
    if matches!(req.mode, SliceMode::TensorParallel) {
        let Some(tp_size) = req.tp_size else {
            return bad_request("informe o tamanho do grupo TP");
        };
        return match tensor_slice_plan(std::path::Path::new(&req.model_path), tp_size) {
            Ok(plan) => Json(plan).into_response(),
            Err(error) => bad_request(error.to_string()),
        };
    }
    let layout = match layout_of(std::path::Path::new(&req.model_path)).await {
        Ok(l) => l,
        Err(e) => return bad_request(e),
    };
    let planned = match req.equal_parts {
        Some(n) if (1..=64).contains(&n) => plan::plan_equal(&layout, n),
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
    if matches!(req.mode, SliceMode::Pipeline) && req.slices.is_empty() {
        return bad_request("nenhuma fatia na lista");
    }
    if matches!(req.mode, SliceMode::Pipeline) && req.slices.len() > 64 {
        return bad_request("no máximo 64 fatias");
    }

    let layout = match layout_of(&source).await {
        Ok(l) => l,
        Err(e) => return bad_request(e),
    };

    if matches!(req.mode, SliceMode::TensorParallel) {
        let Some(tp_size) = req.tp_size else {
            return bad_request("informe o tamanho do grupo TP");
        };
        if let Err(error) = tensor_slice_plan(&source, tp_size) {
            return bad_request(error.to_string());
        }
        let source_sha256 = match source_model_sha256(&source).await {
            Ok(hash) => hash,
            Err(error) => {
                return bad_request(format!("não foi possível identificar o modelo: {error:#}"))
            }
        };
        let total_layers = layout.layer_count();
        let base_name = req
            .name
            .as_deref()
            .and_then(plan::base_name_from_label)
            .unwrap_or_else(|| plan::base_name_of(&req.model_path));
        let shard_dir = state.shard_dir().await;
        let scratch = crate::pom::scratch_dir(&shard_dir);
        let id = uuid::Uuid::new_v4().simple().to_string();
        let label = req.name.clone().unwrap_or_else(|| base_name.clone());
        jobs().write().await.insert(
            id.clone(),
            new_job(
                &id,
                "slice",
                format!("Fatiando {base_name} em {tp_size} shards TP"),
                "preparando",
            ),
        );
        let job_id = id.clone();
        let node_id = state.network.node_id.clone();
        let raiz = shard_dir.clone();
        tokio::spawn(async move {
            match run_tp_slice(
                state,
                source,
                shard_dir,
                scratch,
                base_name,
                label,
                total_layers,
                node_id,
                source_sha256,
                tp_size,
                &job_id,
            )
            .await
            {
                Ok(manifest) => {
                    let msg = format!(
                        "{} shards TP em {} · pom {}",
                        manifest.tp_parts.len(),
                        crate::pom::dir_for(&raiz, &manifest).display(),
                        crate::pom::short_id(&manifest.pom_id)
                    );
                    set_job(&job_id, "done", "pronto", &msg).await;
                }
                Err(error) => {
                    warn!("fatiamento TP falhou: {error:#}");
                    set_job(&job_id, "failed", "erro", &format!("{error:#}")).await;
                }
            }
        });
        return Json(json!({ "job_id": id })).into_response();
    }

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
    let source_sha256 = match source_model_sha256(&source).await {
        Ok(hash) => hash,
        Err(error) => {
            return bad_request(format!("não foi possível identificar o modelo: {error:#}"))
        }
    };

    let base_name = req
        .name
        .as_deref()
        .and_then(plan::base_name_from_label)
        .unwrap_or_else(|| plan::base_name_of(&req.model_path));
    // A pasta definitiva leva o início do hash do modelo original. Corta-se
    // aqui e a rodada se muda para o nome definitivo no fim, em
    // `pom::finalize_run`.
    let shard_dir = state.shard_dir().await;
    let out_dir = crate::pom::scratch_dir(&shard_dir);
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
    let label = req.name.clone().unwrap_or_else(|| base_name.clone());
    let node_id = state.network.node_id.clone();
    let raiz = shard_dir.clone();
    tokio::spawn(async move {
        match run_slice(
            state,
            source,
            shard_dir,
            out_dir,
            base_name,
            label,
            ranges,
            total_layers,
            node_id,
            source_sha256,
            &job_id,
        )
        .await
        {
            Ok(m) => {
                let msg = format!(
                    "{} fatia{} em {} · pom {}",
                    m.parts.len(),
                    if m.parts.len() == 1 { "" } else { "s" },
                    crate::pom::dir_for(&raiz, &m).display(),
                    crate::pom::short_id(&m.pom_id)
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

/// Extract every tensor-parallel rank from one local GGUF and seal the whole
/// group in the source model's unified manifest.
#[allow(clippy::too_many_arguments)]
async fn run_tp_slice(
    state: GatewayState,
    source: std::path::PathBuf,
    shard_dir: std::path::PathBuf,
    scratch: std::path::PathBuf,
    base_name: String,
    label: String,
    total_layers: u32,
    node_id: String,
    source_sha256: String,
    tp_size: u32,
    job_id: &str,
) -> anyhow::Result<crate::pom::PomManifest> {
    let source_path = source.to_string_lossy().into_owned();
    let mut shards = Vec::with_capacity(tp_size as usize);

    for rank in 0..tp_size {
        set_job(
            job_id,
            "running",
            "fatiando",
            &format!("extraindo shard TP rank {}/{}", rank + 1, tp_size),
        )
        .await;

        let progress_job = job_id.to_string();
        let outcome =
            plan::extract_tp_shard(&source_path, &scratch, None, tp_size, rank, move |pct| {
                let id = progress_job.clone();
                tokio::spawn(async move { set_job_progress(&id, pct).await });
            })
            .await?;

        shards.push(crate::pom::PomPart {
            sha256: outcome.sha256.clone(),
            bytes: outcome.bytes,
            start_layer: 0,
            end_layer: total_layers - 1,
            is_first: true,
            is_last: true,
            tp: Some(crate::pom::PomTp {
                rank,
                size: tp_size,
            }),
        });
        enforce_shard_storage(&state, &outcome.output_path).await?;

        invalidate_catalog().await;
        if let Some(job) = jobs().write().await.get_mut(job_id) {
            job.outputs.push(outcome.output_path);
        }
    }

    let manifest = tensor_slice_manifest(
        base_name,
        label,
        total_layers,
        node_id,
        source_sha256,
        source
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(crate::catalog::parse_quant),
        shards,
    );
    let destination = crate::pom::finalize_run(&shard_dir, &scratch, &manifest)?;
    info!("fatiamento TP guardado em {}", destination.display());
    invalidate_catalog().await;
    Ok(manifest)
}

/// Corta o modelo inteiro nas faixas pedidas e mescla as partes no manifesto v1.
///
/// É **aqui** que um fatiamento vira uma coisa só: as partes saem nomeadas pelo
/// próprio hash e o manifesto, escrito no fim, é o que diz de que modelo cada
/// parte saiu. Sem ele as partes seriam arquivos soltos de nome
/// ilegível - o manifesto é o que substitui a faixa que antes vivia no nome.
#[allow(clippy::too_many_arguments)]
async fn run_slice(
    state: GatewayState,
    source: std::path::PathBuf,
    shard_dir: std::path::PathBuf,
    scratch: std::path::PathBuf,
    base_name: String,
    label: String,
    ranges: Vec<(u32, u32)>,
    total_layers: u32,
    node_id: String,
    source_sha256: String,
    job_id: &str,
) -> anyhow::Result<crate::pom::PomManifest> {
    let parts = ranges.len() as u32;
    let mut pecas: Vec<crate::pom::PomPart> = Vec::new();

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
            &scratch,
            // Rodada nova: ninguém sabe o hash antes de cortar.
            None,
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
        pecas.push(crate::pom::PomPart {
            sha256: outcome.sha256.clone(),
            bytes: outcome.bytes,
            start_layer: start,
            end_layer: end,
            is_first: start == 0,
            is_last: end + 1 >= total_layers,
            tp: None,
        });

        // A retenção também precisa acontecer no fluxo de fatiamento isolado:
        // o operador pode criar fatias agora e implantá-las somente depois.
        // A saída recém-criada fica protegida, enquanto diretórios antigos
        // elegíveis são removidos para manter o limite configurado.
        enforce_shard_storage(&state, &outcome.output_path).await?;

        // Show up in the catalog as each one lands, not only at the end.
        invalidate_catalog().await;
        let mut map = jobs().write().await;
        if let Some(job) = map.get_mut(job_id) {
            job.outputs.push(outcome.output_path);
        }
    }

    let quant = source
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(crate::catalog::parse_quant);
    let manifesto = crate::pom::PomManifest::new(
        crate::pom::PomModel {
            name: base_name,
            label,
            total_layers,
            quant,
        },
        crate::pom::PomKind::Layers,
        node_id,
        source_sha256,
        pecas,
    );
    // Cobrir o modelo inteiro é o caso normal, mas fatiar só um pedaço é
    // legítimo (o operador escolhe as faixas). Dizer isso em voz alta evita a
    // descoberta lá na frente, na implantação, de que o conjunto não fecha.
    if !manifesto.complete() {
        warn!(
            "as fatias de {} não cobrem as {total_layers} camadas do modelo",
            manifesto.model.name
        );
    }

    // A rodada inteira se muda de uma vez para `<modelo>-<hash do modelo>`, e o
    // `.pom` é selado lá por último: um manifesto na pasta é, por construção,
    // um fatiamento inteiro que existe em disco.
    let destino = crate::pom::finalize_run(&shard_dir, &scratch, &manifesto)?;
    info!("fatiamento guardado em {}", destino.display());
    invalidate_catalog().await;

    Ok(manifesto)
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

/// Outros parâmetros para a fatia que este nó já serve.
///
/// Só os parâmetros: o que identifica a fatia (arquivo, faixa de camadas,
/// posição no pipeline) não está aqui e não pode ser mudado por esta porta.
/// Ver `NodeConfig::with_params`.
#[derive(Debug, Deserialize)]
pub struct ReconfigureRequest {
    pub params: RuntimeParams,
}

/// Recarrega o modelo desta máquina com outros parâmetros.
///
/// É o caminho de "reimplantar mexendo num parâmetro": nada é planejado, nada
/// é copiado e nenhuma faixa muda de dono - o nó relê o **mesmo** arquivo com,
/// digamos, outro `n_ctx`. Passar pelo `deploy` inteiro para isso significaria
/// re-varrer o cluster e cunhar uma implantação nova, quando o que mudou cabe
/// num `apply`.
///
/// Recusa quando não há nada carregado: reconfigurar o vazio não tem sentido, e
/// inventar uma fatia a partir de parâmetros soltos seria adivinhar qual.
async fn reconfigure(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ReconfigureRequest>,
) -> Response {
    match reconfigure_local(state, req.params).await {
        Ok(cfg) => Json(json!({
            "reconfigured": true,
            "model": cfg.model_label,
            "start_layer": cfg.start_layer,
            "end_layer": cfg.end_layer,
        }))
        .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// A reconfiguração em si, sem HTTP em volta.
///
/// Existe separada porque o coordenador de um `redeploy` também é um nó da
/// implantação: quando a máquina a reconfigurar é ele mesmo, chamar a própria
/// rota por rede seria dar a volta no mundo para falar consigo.
pub(crate) async fn reconfigure_local(
    state: GatewayState,
    params: RuntimeParams,
) -> Result<NodeConfig, String> {
    let Some(atual) = state.runtime.config().await else {
        return Err("este nó não está servindo nada para reconfigurar".into());
    };

    let cfg = atual.with_params(&params);
    info!(
        "reconfigurando {} (camadas {}-{}) com parâmetros novos",
        cfg.model_label, cfg.start_layer, cfg.end_layer
    );

    state
        .runtime
        .apply(cfg.clone())
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(cfg)
}

/// A qual máquina o "sair da rede" se refere.
#[derive(Debug, Default, Deserialize)]
pub struct LeaveRequest {
    /// Máquina que deve sair. Ausente, ou o próprio `node_id`, significa esta.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// Take a node off the network: unload, stop announcing, forget the peers.
///
/// The HTTP gateway keeps serving, because the way back in is this same screen.
///
/// Corpo ausente ou sem `node_id` mantém o comportamento antigo — este nó sai —,
/// que é o que o menu "Este nó" e a tela de Rede sempre pediram. Com `node_id`
/// de outra máquina o pedido é **repassado** à mesma rota lá, como
/// `dev/update` já faz: o painel só fala com o próprio nó (caminhos relativos
/// em `lib/api.ts`), e um card de peer no Painel precisa alcançar aquela
/// máquina. Quem se desliga é sempre o dono da rede daquele processo; este nó
/// não tem como detachar outro por fora.
async fn leave_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    body: Option<Json<LeaveRequest>>,
) -> Response {
    let target = body.and_then(|Json(b)| b.node_id).unwrap_or_default();
    if !target.is_empty() && target != state.network.node_id {
        return forward_leave(&state, &target).await;
    }

    state.runtime.stop().await;
    state.network.detach().await;
    info!("nó saiu da rede pela interface");
    Json(json!({ "detached": true })).into_response()
}

/// Repassa o "sair da rede" ao peer escolhido, sem `node_id` — lá ele é local.
async fn forward_leave(state: &GatewayState, node_id: &str) -> Response {
    let Some(base) = state
        .all_peers()
        .await
        .iter()
        .find(|p| p.node_id == node_id)
        .and_then(|p| p.gateway_url())
    else {
        return bad_request("peer desconhecido ou sem porta de gateway anunciada");
    };
    let key = state.auth.read().await.connection_key.clone();
    let Ok(cliente) = state.peer_http.for_peer(node_id) else {
        return bad_request(format!("sem cliente para o peer {node_id}"));
    };
    match cliente
        .post(format!("{base}/api/ui/leave"))
        .bearer_auth(&key)
        .json(&json!({}))
        .timeout(PEER_TIMEOUT)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!("{node_id} saiu da rede a pedido deste nó");
            Json(json!({ "detached": true, "node_id": node_id })).into_response()
        }
        Ok(r) => bad_request(format!("a máquina recusou: HTTP {}", r.status())),
        Err(e) => bad_request(format!("máquina inacessível: {e}")),
    }
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

        let Ok(cliente) = state.peer_http.for_peer(&peer.node_id) else {
            continue;
        };

        // Busy peers are out: a node serves one range at a time.
        match cliente
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

        let Ok(r) = cliente
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
        // O sucessor entra na **mesma** implantação: é a mesma faixa do mesmo
        // conjunto, só que noutra máquina.
        deploy_id: cfg.deploy_id.clone(),
        source_model_path: candidate.path.clone(),
        // O sucessor carrega exatamente o arquivo que este nó carregava, e o
        // nome de uma parte já é o hash dela: é daí que a identidade sai.
        part_sha256: std::path::Path::new(&candidate.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| crate::pom::is_sha256_hex(s))
            .map(|s| s.to_string()),
        base_name: cfg.model_label.clone(),
        model_label: Some(cfg.model_label.clone()),
        deployment_name: cfg.deployment_name.clone(),
        start_layer: cfg.start_layer,
        end_layer: cfg.end_layer,
        total_layers: cfg.total_layers,
        peer_count: cfg.peer_count,
        peer_index: cfg.peer_index,
        primary: cfg.primary,
        tp: cfg.tp.clone(),
        skip_split: true,
        merge: None,
        params: RuntimeParams::from(&cfg),
    };

    let key = state.auth.read().await.connection_key.clone();
    let cliente = match state.peer_http.for_peer(&candidate.node_id) {
        Ok(c) => c,
        Err(e) => return bad_request(format!("sem cliente para {}: {e}", candidate.node_id)),
    };
    match cliente
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
    /// Hash da parte que este alvo carrega, quando o `.pom` já a conhece. É o
    /// que dispensa o destino de adivinhar um nome de arquivo.
    #[serde(default)]
    pub part_sha256: Option<String>,
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
    /// Override por alvo, calculado a partir da topologia do plano. `None`
    /// preserva o comportamento automático do peer final.
    #[serde(default)]
    pub no_embeddings: Option<bool>,
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
    /// Identidade de tensor parallelism deste alvo (caminho C). Quando presente
    /// (primary ou worker de um grupo), o apply extrai o shard/registra a
    /// coletiva em vez de só fatiar por camadas.
    #[serde(default)]
    pub tp: Option<crate::network::TpIdentity>,
}

#[derive(Debug, Deserialize)]
pub struct LaunchRequest {
    /// A implantação que este launch está subindo. Cunhado por
    /// `deploy::deploy_endpoint`; vazio num launch avulso, que não passou por
    /// implantação nenhuma.
    #[serde(default)]
    pub deploy_id: String,
    pub base_name: String,
    /// The identity selected in the deployment screen, propagated unchanged
    /// to every apply request. Missing values keep older callers compatible.
    #[serde(default)]
    pub model_label: Option<String>,
    /// Nome operacional exibido em jobs, status e telemetria.
    #[serde(default)]
    pub deployment_name: String,
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
pub(crate) fn params_for(base: &RuntimeParams, target: &LaunchTarget) -> RuntimeParams {
    let mut params = base.clone();
    if let Some(gpu) = target.n_gpu_layers {
        params.n_gpu_layers = gpu;
    }
    if let Some(kqv) = target.offload_kqv {
        params.offload_kqv = kqv;
    }
    if let Some(no_embeddings) = target.no_embeddings {
        params.no_embeddings = no_embeddings;
    }
    params
}

fn launch_no_embeddings_evaluation(
    req: &LaunchRequest,
) -> crate::capabilities::NoEmbeddingsEvaluation {
    let mut ranges = Vec::new();
    for target in &req.targets {
        if target.tp.is_none() {
            let range = (target.start_layer, target.end_layer);
            if !ranges.contains(&range) {
                ranges.push(range);
            }
        }
    }
    let pipeline_count = ranges.len() as u32;
    let peers = req
        .targets
        .iter()
        .map(|target| {
            let derived_index = if target.tp.is_some() {
                Some(0)
            } else {
                ranges
                    .iter()
                    .position(|range| *range == (target.start_layer, target.end_layer))
                    .map(|value| value as u32)
            };
            crate::capabilities::PeerTopology {
                node_id: target.node_id.clone(),
                start_layer: Some(target.start_layer),
                end_layer: Some(target.end_layer),
                total_layers: Some(req.total_layers),
                peer_index: target.peer_index.or(derived_index),
                peer_count: target.peer_count.or_else(|| {
                    if target.tp.is_some() {
                        Some(1)
                    } else {
                        Some(pipeline_count)
                    }
                }),
            }
        })
        .collect::<Vec<_>>();
    crate::capabilities::evaluate_no_embeddings(&peers)
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
    let deployment = match normalize_deployment_name(Some(&req.deployment_name), &req.base_name) {
        Ok(name) => name,
        Err(error) => return bad_request(error),
    };
    if let Some(t) = req.targets.iter().find(|t| t.end_layer >= req.total_layers) {
        return bad_request(format!(
            "a fatia L{}-{} passa do fim de um modelo de {} camadas",
            t.start_layer, t.end_layer, req.total_layers
        ));
    }

    if req.params.no_embeddings {
        if let Err(reason) = crate::capabilities::validate_no_embeddings(
            &launch_no_embeddings_evaluation(&req),
            true,
        ) {
            return bad_request(reason);
        }
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
        let Ok(cliente) = state.peer_http.for_peer(&target.node_id) else {
            warn!("sem cliente para {} em stop-inference", target.node_id);
            continue;
        };
        match cliente
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
            deployment_name: deployment.clone(),
            start_layer: target.start_layer,
            end_layer: target.end_layer,
            total_layers: req.total_layers,
            peer_count: target.peer_count.unwrap_or(peer_count),
            peer_index: target.peer_index.unwrap_or(index as u32),
            primary: req.primary_node_id.as_deref() == Some(target.node_id.as_str()),
            tp: target.tp.clone(),
            skip_split: target.skip_split,
            deploy_id: req.deploy_id.clone(),
            part_sha256: target.part_sha256.clone(),
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
        let cliente = match state.peer_http.for_peer(&target.node_id) {
            Ok(c) => c,
            Err(e) => {
                results.push(json!({
                    "node_id": target.node_id,
                    "dispatched": false,
                    "error": format!("sem cliente para o peer: {e}"),
                }));
                continue;
            }
        };
        match cliente
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
        let root = std::env::temp_dir().join(format!("pom-del-{}", std::process::id()));
        let blobs = root.join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(root.join("manifests")).unwrap();
        let blob = blobs.join("sha256-abc");
        std::fs::write(&blob, b"GGUF").unwrap();

        assert!(is_ollama_blob(&blob));

        // Um `blobs/` sem `manifests/` ao lado não é o store do Ollama — o
        // LM Studio também tem uma pasta com esse nome, e vazia.
        let outro = std::env::temp_dir().join(format!("pom-del2-{}", std::process::id()));
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

/// "Sair da rede" no card de uma máquina.
#[cfg(test)]
mod leave_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_with};
    use axum::routing::post as axum_post;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Uma máquina que atende `/api/ui/leave` e conta os pedidos.
    async fn fake_machine(status: StatusCode) -> (u16, Arc<AtomicU32>) {
        let hits = Arc::new(AtomicU32::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/api/ui/leave",
            axum_post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (status, Json(json!({ "detached": true })))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, hits)
    }

    /// O card de um peer manda o nó local repassar: o painel só fala com o
    /// próprio nó, e quem se desliga é sempre o dono daquele processo.
    #[tokio::test]
    async fn leaving_for_a_peer_is_forwarded_to_that_peer() {
        let (port, hits) = fake_machine(StatusCode::OK).await;
        let state = state_with("eu", vec![peer("outro", "ksia", port)]).await;

        let resp = leave_network(
            ConnectionAuth,
            State(state.clone()),
            Some(Json(LeaveRequest {
                node_id: Some("outro".into()),
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_of(resp).await["node_id"], "outro");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // E este nó continua na rede: o pedido era sobre a outra máquina.
        assert!(
            !state.network.is_detached(),
            "repassar não pode desligar quem repassou"
        );
    }

    /// Corpo ausente e o próprio `node_id` são a mesma coisa: este nó sai. É o
    /// caminho que o menu "Este nó" e a tela de Rede sempre usaram.
    #[tokio::test]
    async fn leaving_without_a_target_still_detaches_this_node() {
        let state = state_with("eu", vec![]).await;
        let resp = leave_network(ConnectionAuth, State(state.clone()), None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.network.is_detached());

        let state = state_with("eu", vec![]).await;
        let _ = leave_network(
            ConnectionAuth,
            State(state.clone()),
            Some(Json(LeaveRequest {
                node_id: Some("eu".into()),
            })),
        )
        .await;
        assert!(state.network.is_detached());
    }

    /// Uma máquina que não responde é recusa com motivo, não um "saiu" falso.
    #[tokio::test]
    async fn a_peer_that_refuses_is_not_reported_as_gone() {
        let (port, _) = fake_machine(StatusCode::INTERNAL_SERVER_ERROR).await;
        let state = state_with("eu", vec![peer("outro", "ksia", port)]).await;

        let resp = leave_network(
            ConnectionAuth,
            State(state.clone()),
            Some(Json(LeaveRequest {
                node_id: Some("outro".into()),
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!state.network.is_detached());

        // Peer que nem existe na tabela também não vira sucesso.
        let resp = leave_network(
            ConnectionAuth,
            State(state.clone()),
            Some(Json(LeaveRequest {
                node_id: Some("fantasma".into()),
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

#[cfg(test)]
mod catalogo_sob_tls_tests {
    use super::*;
    use crate::gateway::testkit::state_with;
    use crate::network::PeerInfo;

    /// Porta 1, fechada de proposito: o que se afirma aqui nao e que a chamada
    /// deu certo, e sim **por qual cliente** ela saiu.
    fn peer_com_gateway(node_id: &str) -> PeerInfo {
        crate::gateway::testkit::peer(node_id, node_id, 1)
    }

    /// A tela de implantar lista as fatias perguntando a cada peer, e essas
    /// chamadas tem que sair pelo cliente com pinning.
    ///
    /// Este teste existe por um defeito real: o plano de controle continuou
    /// usando o cliente compartilhado e sem pinning depois do TLS entrar, e com
    /// a politica ligada **toda** consulta a peer falhava calada - a tela dizia
    /// "nenhuma maquina do cluster tem fatias de <modelo>" e nao havia como
    /// implantar nada. O cache por peer so ganha entrada quando alguem pediu um
    /// cliente pinado, entao ele e a prova de que o caminho certo foi usado.
    #[tokio::test]
    async fn cluster_models_fala_com_os_peers_pelo_cliente_pinado() {
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        let state = state_with("eu", vec![peer_com_gateway("outro")]).await;
        state.peer_http.apply_cluster_policy(true);
        assert!(state.peer_http.cached_peers().is_empty());

        let _ = cluster_models(State(state.clone())).await;

        assert_eq!(
            state.peer_http.cached_peers(),
            vec!["outro".to_string()],
            "a consulta de catalogo saiu por um cliente sem pinning"
        );
        state.peer_http.apply_cluster_policy(false);
    }

    /// Mesma prova para o levantamento da tela de implantar (`deploy::survey`),
    /// que e por onde o defeito apareceu para o operador.
    #[tokio::test]
    async fn a_lista_de_maquinas_para_implantar_tambem_e_pinada() {
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        let state = state_with("eu", vec![peer_com_gateway("outro")]).await;
        state.peer_http.apply_cluster_policy(true);

        let _ = crate::deploy::survey_for_tests(&state, "qwen35").await;

        // O proprio no tambem entra: o levantamento pergunta o catalogo dele
        // pela mesma rota HTTP, e sob TLS isso tambem precisa de certificado
        // verificado. O que importa e o peer estar la.
        assert!(
            state
                .peer_http
                .cached_peers()
                .contains(&"outro".to_string()),
            "o levantamento saiu por um cliente sem pinning: {:?}",
            state.peer_http.cached_peers()
        );
        state.peer_http.apply_cluster_policy(false);
    }
}

#[cfg(test)]
mod tls_settings_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};

    /// Só o principal decide. A checagem é **no servidor**: esconder o botão na
    /// tela é conveniência, não controle.
    #[tokio::test]
    async fn no_comum_nao_pode_ligar_tls() {
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.role = crate::config::Role::Node;

        let resp = save_tls(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTlsRequest { tls_required: true }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            !state.auth.read().await.tls_required,
            "403 não pode ter gravado nada"
        );
        assert!(!state.network.tls_enabled());
    }

    /// No principal, virar a flag vale na hora: config gravada, anúncio novo, e
    /// os clientes trocados. Sem reinício, e nos dois sentidos.
    #[tokio::test]
    async fn principal_liga_e_desliga_sem_reinicio() {
        // Este teste mexe no esquema global das URLs; ver a nota em
        // `peer_client::POLITICA_GLOBAL`.
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.role = crate::config::Role::Principal;

        let resp = save_tls(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTlsRequest { tls_required: true }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_of(resp).await["tls_required"], true);
        assert!(state.auth.read().await.tls_required);
        assert!(
            state.network.tls_enabled(),
            "o anúncio precisa dizer a verdade"
        );
        assert!(state.peer_http.tls_enabled());

        let resp = save_tls(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveTlsRequest {
                tls_required: false,
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!state.auth.read().await.tls_required);
        assert!(!state.network.tls_enabled());
        assert!(!state.peer_http.tls_enabled());
    }

    /// A tela precisa de duas coisas diferentes: se está ligado, e se **este**
    /// nó pode mexer.
    #[tokio::test]
    async fn node_info_conta_estado_e_autoridade() {
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.role = crate::config::Role::Node;
        let info = node_info(State(state.clone())).await;
        assert!(!info.0.tls_required);
        assert!(!info.0.tls_authority, "nó comum não decide");

        state.auth.write().await.role = crate::config::Role::Principal;
        state.auth.write().await.tls_required = true;
        let info = node_info(State(state)).await;
        assert!(info.0.tls_required);
        assert!(info.0.tls_authority);
    }
}

#[cfg(test)]
mod advertise_addr_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};

    /// A resposta já traz o efetivo, para a tela confirmar o efeito sem uma
    /// segunda volta — é o próprio motivo de existir deste campo na resposta.
    #[tokio::test]
    async fn pinning_an_address_takes_effect_immediately_and_live() {
        let state = state_with("eu", vec![]).await;

        let resp = save_advertise_addr(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveAdvertiseAddrRequest {
                advertise_addr: "203.0.113.9:42777".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = body_of(resp).await;
        assert_eq!(body["advertise_addr"], "203.0.113.9:42777");
        assert_eq!(body["effective_address"], "203.0.113.9:42777");

        // Live: o mesmo `Network` já anuncia o valor novo, sem reinício.
        assert_eq!(state.network.announce_addr(), "203.0.113.9:42777");
        // E persistido: uma releitura do config em disco vê o mesmo valor.
        assert_eq!(
            state.auth.read().await.advertise_addr.as_deref(),
            Some("203.0.113.9:42777")
        );
    }

    /// String vazia limpa o pino, mesma convenção de `SavePathsRequest.shard_dir`.
    #[tokio::test]
    async fn an_empty_value_clears_the_pin() {
        let state = state_with("eu", vec![]).await;
        state.network.set_advertise_addr(Some("203.0.113.9:42777"));

        let resp = save_advertise_addr(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveAdvertiseAddrRequest {
                advertise_addr: "".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(state.auth.read().await.advertise_addr, None);
    }

    /// Um valor sem forma reconhecível é recusado, não gravado quebrado —
    /// mesmo espírito de `check_dir` para pastas de modelo.
    #[tokio::test]
    async fn a_malformed_value_is_rejected_not_saved() {
        let state = state_with("eu", vec![]).await;

        let resp = save_advertise_addr(
            ConnectionAuth,
            State(state.clone()),
            Json(SaveAdvertiseAddrRequest {
                advertise_addr: "[not-a-valid-ipv6]".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(state.auth.read().await.advertise_addr, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_folder_cannot_equal_or_nest_inside_a_whole_model_folder() {
        let root = std::env::temp_dir().join(format!("pom-paths-{}", std::process::id()));
        let whole = root.join("whole");
        let nested = whole.join("slices");
        let sibling = root.join("slices");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();

        let whole = whole.to_string_lossy().into_owned();
        assert!(reject_nested_shard_dir(&[whole.clone()], &whole).is_err());
        assert!(reject_nested_shard_dir(&[whole.clone()], &nested.to_string_lossy()).is_err());
        assert!(reject_nested_shard_dir(&[whole], &sibling.to_string_lossy()).is_ok());

        let _ = std::fs::remove_dir_all(root);
    }

    /// Um alvo TP runtime extrai o próprio rank, mas um alvo que já aponta para
    /// `tp_parts` usa o arquivo existente e não o fatia novamente.
    #[test]
    fn every_rank_of_a_tp_group_extracts_its_own_shard() {
        let req = |tp: Option<crate::network::TpIdentity>| ApplyRequest {
            deploy_id: String::new(),
            part_sha256: None,
            source_model_path: "/m/whole.gguf".into(),
            base_name: "whole".into(),
            model_label: None,
            deployment_name: "whole".into(),
            start_layer: 0,
            end_layer: 31,
            total_layers: 32,
            peer_count: 1,
            peer_index: 0,
            primary: true,
            tp,
            skip_split: false,
            merge: None,
            params: RuntimeParams::default(),
        };
        let ident = |rank: u32, role: &str| crate::network::TpIdentity {
            role: role.into(),
            group_id: "g".into(),
            size: 2,
            rank,
            rank0_url: Some("http://127.0.0.1:8181".into()),
            rank0_node_id: Some("primary-id".into()),
            members: Vec::new(),
        };

        let primary = req(Some(ident(0, "primary")));
        let escolhido = tp_shard_to_extract(&primary).expect("o rank 0 runtime também fatia");
        assert_eq!(escolhido.rank, 0);
        assert_eq!(
            (escolhido.rank, escolhido.size),
            (0, 2),
            "o shard que faltava no disco"
        );

        let worker = req(Some(ident(1, "worker")));
        assert_eq!(tp_shard_to_extract(&worker).unwrap().rank, 1);

        let mut pre_sliced = req(Some(ident(0, "primary")));
        pre_sliced.skip_split = true;
        assert!(
            tp_shard_to_extract(&pre_sliced).is_none(),
            "rank TP já fatiado deve ser carregado diretamente"
        );

        assert!(
            tp_shard_to_extract(&req(None)).is_none(),
            "nó sem TP não fatia por tensor"
        );
    }

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
            part_sha256: None,
            node_id: "a".into(),
            source_model_path: "/m.gguf".into(),
            start_layer: 0,
            end_layer: 15,
            skip_split: true,
            merge: None,
            n_gpu_layers: gpu,
            offload_kqv: kqv,
            no_embeddings: None,
            peer_index: None,
            peer_count: None,
            tp: None,
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
            deploy_id: String::new(),
            part_sha256: None,
            source_model_path: "/m.gguf".into(),
            base_name: "m".into(),
            model_label: None,
            deployment_name: "m".into(),
            start_layer: start,
            end_layer: end,
            total_layers: total,
            peer_count: peers,
            peer_index: 0,
            primary: true,
            tp: None,
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
        assert!(p.batching_enabled);
        assert_eq!(p.max_queue_requests, 128);
        assert_eq!(p.batch_window_ms, 4);
        assert_eq!(p.max_batch_sequences, u32::MAX);
        assert_eq!(p.max_batch_tokens, u32::MAX);
        assert!(p.no_thinking);
        assert!(p.offload_kqv);
        assert!(p.fit_memory);
    }

    #[test]
    fn inference_mode_defaults_to_auto_and_round_trips() {
        let default: RuntimeParams = serde_json::from_str("{}").unwrap();
        assert_eq!(default.inference_mode, InferenceMode::Auto);

        let explicit: RuntimeParams = serde_json::from_value(serde_json::json!({
            "inference_mode": "moe"
        }))
        .unwrap();
        assert_eq!(explicit.inference_mode, InferenceMode::Moe);
        let encoded = serde_json::to_value(explicit).unwrap();
        assert_eq!(encoded["inference_mode"], "moe");
    }

    #[test]
    fn params_accept_partial_overrides() {
        let p: RuntimeParams = serde_json::from_str(
            r#"{"n_ctx":250000,"no_thinking":false,"offload_kqv":false,"fit_memory":false}"#,
        )
        .unwrap();
        assert_eq!(p.n_ctx, 250000);
        assert!(!p.no_thinking);
        assert!(!p.offload_kqv);
        assert!(!p.fit_memory);
        assert_eq!(p.top_k, 40, "campos ausentes mantêm o padrão");
    }

    #[test]
    fn selected_model_label_is_not_normalized_to_a_profile_name() {
        let mut request = req(0, 31, 32, 1);
        request.model_label = Some("Qwen3.5-9B-Instruct".into());

        assert_eq!(model_label(&request), "Qwen3.5-9B-Instruct");
    }

    #[test]
    fn sealing_layer_and_tp_parts_merges_them_under_the_source_model() {
        let root = std::env::temp_dir().join(format!(
            "pom-webui-seal-source-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let scratch = root.join(".scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let source_sha256 = "a".repeat(64);
        let layer_sha256 = "b".repeat(64);
        let tp_sha256 = "c".repeat(64);

        let mut layer_req = req(0, 14, 32, 2);
        layer_req.source_model_path = "/models/original.gguf".into();
        layer_req.base_name = "Llama-3.1-8B-Instruct-Q4_K_M.gguf".into();
        std::fs::write(
            crate::pom::part_path(&scratch, &layer_sha256).unwrap(),
            b"layer",
        )
        .unwrap();
        seal_single_part(
            &root,
            &scratch,
            &layer_req,
            &plan::SliceOutcome {
                output_path: crate::pom::part_path(&scratch, &layer_sha256)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                bytes: 5,
                sha256: layer_sha256.clone(),
                reused: false,
            },
            source_sha256.clone(),
            crate::pom::PomKind::Layers,
            None,
            "node-a",
        )
        .unwrap();

        let mut tp_req = req(0, 31, 32, 1);
        tp_req.source_model_path = "/models/original.gguf".into();
        tp_req.base_name = "Llama-3.1-8B-Instruct-Q4_K_M.gguf".into();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(crate::pom::part_path(&scratch, &tp_sha256).unwrap(), b"tp").unwrap();
        seal_single_part(
            &root,
            &scratch,
            &tp_req,
            &plan::SliceOutcome {
                output_path: crate::pom::part_path(&scratch, &tp_sha256)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                bytes: 2,
                sha256: tp_sha256.clone(),
                reused: false,
            },
            source_sha256.clone(),
            crate::pom::PomKind::TensorParallel,
            Some(crate::pom::PomTp { rank: 0, size: 2 }),
            "node-b",
        )
        .unwrap();

        let dir = crate::pom::locate_source_dir(&root, &source_sha256).unwrap();
        assert_eq!(
            dir.file_name().unwrap().to_string_lossy(),
            "Llama-3.1-8B-Instruct-Q4_K_M-aaaaaaaa"
        );
        let manifestos = crate::pom::load_dir(&dir);
        assert_eq!(manifestos.len(), 1);
        let manifest = &manifestos[0];
        assert_eq!(manifest.pom_id, source_sha256);
        assert_eq!(manifest.parts.len(), 1);
        assert_eq!(manifest.tp_parts.len(), 1);
        assert_eq!(manifest.kind, crate::pom::PomKind::Combined);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn tensor_slice_plan_lists_one_full_model_range_per_rank() {
        let root = std::env::temp_dir().join(format!(
            "pom-webui-tp-plan-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("model.gguf");
        std::fs::write(&source, crate::splitter::test_gguf_bytes(4)).unwrap();

        let plan = tensor_slice_plan(&source, 2).unwrap();

        assert_eq!(plan.mode, SliceMode::TensorParallel);
        assert_eq!(plan.tp_size, 2);
        assert_eq!(plan.total_layers, 4);
        assert_eq!(plan.ranks.len(), 2);
        assert_eq!(
            plan.ranks
                .iter()
                .map(|rank| (rank.rank, rank.start_layer, rank.end_layer))
                .collect::<Vec<_>>(),
            vec![(0, 0, 3), (1, 0, 3)]
        );

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn tensor_slice_request_does_not_require_pipeline_slices() {
        let request: SliceRequest = serde_json::from_str(
            r#"{"model_path":"/models/model.gguf","mode":"tensor_parallel","tp_size":2}"#,
        )
        .unwrap();

        assert!(request.slices.is_empty());
        assert_eq!(request.mode, SliceMode::TensorParallel);
        assert_eq!(request.tp_size, Some(2));
    }

    #[test]
    fn tensor_slice_manifest_keeps_all_ranks_in_tp_parts() {
        let manifest = tensor_slice_manifest(
            "Llama-3.1-8B-Instruct-Q4_K_M.gguf".into(),
            "Llama".into(),
            32,
            "node-a".into(),
            "a".repeat(64),
            None,
            vec![
                crate::pom::PomPart {
                    sha256: "b".repeat(64),
                    bytes: 10,
                    start_layer: 0,
                    end_layer: 31,
                    is_first: true,
                    is_last: true,
                    tp: Some(crate::pom::PomTp { rank: 0, size: 2 }),
                },
                crate::pom::PomPart {
                    sha256: "c".repeat(64),
                    bytes: 11,
                    start_layer: 0,
                    end_layer: 31,
                    is_first: true,
                    is_last: true,
                    tp: Some(crate::pom::PomTp { rank: 1, size: 2 }),
                },
            ],
        );

        assert_eq!(manifest.kind, crate::pom::PomKind::TensorParallel);
        assert!(manifest.parts.is_empty());
        assert_eq!(manifest.tp_parts.len(), 2);
        manifest.validate().unwrap();
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
            let root = std::env::temp_dir().join(format!("pom-webui-merge-{}", std::process::id()));
            let parts_dir = root.join("parts");
            std::fs::create_dir_all(&parts_dir).unwrap();
            let source_sha256 = "d".repeat(64);
            let write_part = |dir: &std::path::Path, start: u32, end: u32| {
                std::fs::create_dir_all(dir).unwrap();
                let tmp = dir.join("tmp");
                std::fs::write(&tmp, crate::splitter::test_gguf_bytes(16)).unwrap();
                let (sha256, bytes) = crate::pom::hash_file(&tmp).unwrap();
                let path = crate::pom::part_path(dir, &sha256).unwrap();
                std::fs::rename(&tmp, &path).unwrap();
                let manifest = crate::pom::PomManifest::new(
                    crate::pom::PomModel {
                        name: "m".into(),
                        label: "m".into(),
                        total_layers: 32,
                        quant: None,
                    },
                    crate::pom::PomKind::Layers,
                    "test",
                    source_sha256.clone(),
                    vec![crate::pom::PomPart {
                        sha256: sha256.clone(),
                        bytes,
                        start_layer: start,
                        end_layer: end,
                        is_first: start == 0,
                        is_last: end == 31,
                        tp: None,
                    }],
                );
                crate::pom::write_atomic(dir, &manifest).unwrap();
                path
            };
            let a = write_part(&parts_dir.join("a"), 0, 15);
            let b = write_part(&parts_dir.join("b"), 16, 31);

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
            // O nome é o conteúdo: quem junta não escolhe como chamar o
            // resultado, o hash é que o nomeia.
            let nome = out_path.file_stem().unwrap().to_string_lossy().into_owned();
            assert!(
                crate::pom::is_sha256_hex(&nome),
                "o modelo junto sai endereçado por conteúdo, não por convenção de nome: {nome}"
            );
            assert_eq!(
                crate::pom::hash_file(out_path).unwrap().0,
                nome,
                "e o hash do arquivo tem que ser o nome dele"
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
            std::fs::write(out_path, &marker).unwrap();
            let again = merge_parts(root.clone(), &req, &parts, "job-merge-2")
                .await
                .unwrap();
            assert_eq!(again, out);
            assert_eq!(
                std::fs::read(out_path).unwrap(),
                marker,
                "o modelo junto existente foi reaproveitado, não reescrito"
            );

            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn merge_parts_refuses_one_part_or_a_gap() {
        block_on(async {
            let root =
                std::env::temp_dir().join(format!("pom-webui-merge-bad-{}", std::process::id()));
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

/// **A máquina tem um nome, e é um só.**
///
/// `node_name` do `pom.json` é como o operador batiza a máquina, e é o nome
/// que o nó anuncia aos peers (`network.hostname`). O catálogo respondia com
/// `sysmon`, o hostname cru do sistema operacional — e o seletor "Enviar
/// para..." da tela de fatias, que lê daí, era o único lugar do aplicativo que
/// chamava a máquina por outro nome. Numa máquina com dois nós era pior: os
/// dois relatavam o mesmo hostname do SO, a tela via colisão e desempatava
/// com o prefixo do `node_id`.
#[cfg(test)]
mod node_name_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_named};

    #[tokio::test]
    async fn o_catalogo_local_se_apresenta_pelo_nome_do_no() {
        let state = state_named("eu", "atena", vec![]).await;

        let corpo = body_of(models(State(state)).await.into_response()).await;

        assert_eq!(
            corpo["hostname"], "atena",
            "o catálogo tem que dizer o nome do nó, não o hostname do SO"
        );
    }

    #[tokio::test]
    async fn o_catalogo_do_cluster_tambem() {
        let state = state_named("eu", "atena", vec![]).await;

        let corpo = body_of(cluster_models(State(state)).await.into_response()).await;

        let eu = &corpo["peers"][0];
        assert_eq!(eu["is_self"], true);
        assert_eq!(eu["hostname"], "atena");
    }

    /// Um peer que não respondeu continua sendo uma máquina com nome: o que
    /// ele anunciou está na tabela local. Sem isto o objeto saía **sem
    /// `hostname`** e a tela caía no `node_id` — um prefixo hexadecimal no
    /// lugar do nome da máquina, que é o rótulo errado do relato.
    #[tokio::test]
    async fn um_peer_que_nao_respondeu_ainda_tem_nome() {
        // A porta do peer falso não tem ninguém ouvindo: a consulta falha.
        let mut mudo = peer("outro", "ksia", 1);
        mudo.hostname = "ksia".into();
        let state = state_named("eu", "atena", vec![mudo]).await;

        let corpo = body_of(cluster_models(State(state)).await.into_response()).await;

        let outro = corpo["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["node_id"] == "outro")
            .expect("o peer entra na lista mesmo sem responder");
        assert!(!outro["error"].is_null(), "e a falha continua reportada");
        assert_eq!(
            outro["hostname"], "ksia",
            "sem nome a tela cai no node_id, que é um prefixo hexadecimal"
        );
    }
}

/// `/api/ui/peer/:node_id/runtime` - como aquela máquina carregou o modelo.
#[cfg(test)]
mod peer_runtime_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, peer, state_with};

    /// Um servidor de inferência que responde `/health` com a colocação.
    async fn fake_inference() -> u16 {
        let app = Router::new().route(
            "/health",
            get(|| async {
                Json(json!({
                    "status": "ok",
                    "offload_kqv": true,
                    "placement": { "n_layer": 1, "layers": [] },
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        port
    }

    #[tokio::test]
    async fn unknown_machine_is_a_404_and_not_an_empty_answer() {
        let state = state_with("eu", vec![]).await;
        let r = peer_runtime(State(state), Path("ninguem".to_string())).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    /// Máquina na rede sem servidor de inferência não é falha de rede: é o
    /// estado normal de quem não recebeu fatia. A tela precisa distinguir os
    /// dois, então isto responde 200 dizendo que não há o que mostrar.
    #[tokio::test]
    async fn a_machine_without_an_inference_server_is_not_an_error() {
        let state = state_with("eu", vec![peer("outro", "ksia", 9)]).await;
        let r = peer_runtime(State(state), Path("outro".to_string())).await;
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(body["loaded"], false);
        assert!(body["reason"].is_string(), "a tela precisa do motivo");
    }

    /// O `/health` do peer chega inteiro, sem passar por um tipo intermediário:
    /// um campo novo no servidor de inferência aparece na tela sem que esta
    /// rota mude.
    #[tokio::test]
    async fn the_peers_health_is_forwarded_whole() {
        let port = fake_inference().await;
        let mut p = peer("outro", "ksia", 9);
        p.inference_port = Some(port);
        let state = state_with("eu", vec![p]).await;

        let r = peer_runtime(State(state), Path("outro".to_string())).await;
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(body["loaded"], true);
        assert_eq!(body["health"]["offload_kqv"], true);
        assert_eq!(body["health"]["placement"]["n_layer"], 1);
    }
}

#[cfg(test)]
mod state_transport_tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::header::{ETAG, IF_NONE_MATCH};

    #[test]
    fn the_etag_changes_when_the_ui_payload_changes() {
        let before = etag_for_json(&json!({ "jobs": [] }));
        let after = etag_for_json(&json!({
            "jobs": [{ "id": "job-1", "state": "running" }]
        }));

        assert_ne!(
            before, after,
            "uma mudança visível precisa invalidar a versão"
        );
    }

    #[test]
    fn the_etag_stays_when_the_ui_payload_does_not_change() {
        let first = etag_for_json(&json!({ "offers": [] }));
        let second = etag_for_json(&json!({ "offers": [] }));

        assert_eq!(first, second, "o mesmo retrato precisa manter a versão");
    }

    #[test]
    fn cluster_version_ignores_only_the_sampling_timestamp() {
        let first = json!({
            "node": { "system": { "sampled_at": "2026-01-01T00:00:00Z", "ram": { "used_mb": 10 } } }
        });
        let second = json!({
            "node": { "system": { "sampled_at": "2026-01-01T00:00:01Z", "ram": { "used_mb": 10 } } }
        });
        let changed = json!({
            "node": { "system": { "sampled_at": "2026-01-01T00:00:01Z", "ram": { "used_mb": 11 } } }
        });

        assert_eq!(cluster_etag(&first), cluster_etag(&second));
        assert_ne!(cluster_etag(&second), cluster_etag(&changed));
    }

    #[tokio::test]
    async fn an_matching_etag_returns_304_without_a_body() {
        let payload = json!({ "jobs": [] });
        let first = conditional_json(&HeaderMap::new(), &payload);
        let etag = first.headers().get(ETAG).cloned().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag);

        let second = conditional_json(&headers, &payload);
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert!(
            to_bytes(second.into_body(), usize::MAX)
                .await
                .unwrap()
                .is_empty(),
            "304 não pode remandar o JSON"
        );
    }
}

#[cfg(test)]
mod chat_telemetry_tests {
    use super::*;

    #[test]
    fn directional_report_is_kept_only_for_verified_sources_and_requested_targets() {
        let requested = ["a", "b", "c"].into_iter().map(str::to_string).collect();
        let mut output = Vec::new();
        append_chat_telemetry_measurements(
            &mut output,
            "a",
            RttReport {
                source_node_id: "wrong-source".into(),
                measurements: vec![crate::route::RttMeasurement {
                    target_node_id: "b".into(),
                    rtt_ms: 12.5,
                    transport: "libp2p".into(),
                }],
            },
            &requested,
        );
        assert!(output.is_empty(), "a resposta não pode trocar a origem");

        append_chat_telemetry_measurements(
            &mut output,
            "a",
            RttReport {
                source_node_id: "a".into(),
                measurements: vec![
                    crate::route::RttMeasurement {
                        target_node_id: "b".into(),
                        rtt_ms: 12.5,
                        transport: "libp2p".into(),
                    },
                    crate::route::RttMeasurement {
                        target_node_id: "outside-pipeline".into(),
                        rtt_ms: 99.0,
                        transport: "libp2p".into(),
                    },
                ],
            },
            &requested,
        );
        assert_eq!(
            output,
            vec![ChatTelemetryMeasurement {
                from_node_id: "a".into(),
                to_node_id: "b".into(),
                rtt_ms: 12.5,
                transport: "libp2p".into(),
            }]
        );
    }
}
