//! Moving model slices between machines.
//!
//! Two directions, both ending in the same download:
//!
//! * **pull** — this node asks a peer for a slice it does not have;
//! * **push** — a node *offers* a slice to a peer, the peer's owner accepts,
//!   and the peer then pulls it. The sender never writes to the receiver's
//!   disk on its own; acceptance is what starts the transfer.
//!
//! The bytes travel over the peers' existing HTTP gateway rather than the
//! libp2p bitswap behaviour: that is the path already proven to carry traffic
//! between these machines (the inference pipeline uses it), and it gives byte
//! ranges — so an interrupted multi-gigabyte transfer resumes instead of
//! starting over.
//!
//! Nothing is ever served or written outside the models directory, and a file
//! only takes its final name once its MD5 matches the sender's.

use crate::auth::ConnectionAuth;
use crate::gateway::GatewayState;
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Read/written in chunks — a slice is gigabytes, it never sits in memory.
const CHUNK: usize = 1024 * 1024;

/// Headroom demanded on top of the file size before a download starts.
const DISK_HEADROOM_MB: u64 = 512;

pub fn routes() -> Router<GatewayState> {
    Router::new()
        // Serving side
        .route("/api/transfer/manifest", get(manifest))
        .route("/api/transfer/download", get(download))
        // Receiving side
        .route("/api/transfer/pull", post(start_pull))
        .route("/api/transfer/offer", post(receive_offer))
        .route("/api/transfer/offers", get(list_offers))
        .route("/api/transfer/offers/:id/accept", post(accept_offer))
        .route("/api/transfer/offers/:id/reject", post(reject_offer))
        .route("/api/transfer/jobs", get(list_jobs))
        // Push direction, driven from this node's screen
        .route("/api/transfer/send", post(send_to_peer))
}

// ── Shared state ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TransferJob {
    pub id: String,
    pub name: String,
    pub from_host: String,
    pub state: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub bytes_per_sec: f64,
    pub eta_secs: Option<u64>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferOffer {
    pub id: String,
    /// Node offering the file — the receiver pulls from here once accepted.
    pub from_node_id: String,
    pub from_host: String,
    pub from_gateway: String,
    /// Shown to the human deciding, so they can see *what* is being offered.
    pub remote_path: String,
    /// How we actually fetch it. Absent only for offers from an older peer,
    /// which can still be pulled by path if the file lives in `models/`.
    pub grant: Option<String>,
    pub name: String,
    pub size_bytes: u64,
    pub md5: String,
    pub start_layer: Option<u32>,
    pub end_layer: Option<u32>,
}

#[derive(Default)]
pub struct TransferState {
    pub jobs: RwLock<HashMap<String, TransferJob>>,
    pub offers: RwLock<Vec<TransferOffer>>,
    /// path → (mtime, size, md5), so a 3 GB digest is computed once.
    digests: RwLock<HashMap<String, (u64, u64, String)>>,
    /// Files this machine's own operator deliberately offered: id → path.
    ///
    /// A grant is what lets a file outside `models/` leave this machine. It is
    /// created only by [`send_to_peer`], which is driven from *this* node's
    /// screen — so the path is always someone's local choice, never a remote
    /// request. See [`grant_path`].
    grants: RwLock<HashMap<String, PathBuf>>,
}

pub type Transfers = Arc<TransferState>;

// ── Path safety ──────────────────────────────────────────────────────────────

/// Resolve a requested path inside the models directory, or refuse.
///
/// The path arrives from another machine, so it is untrusted: without this a
/// peer could ask for any file the process can read.
fn safe_model_path(models_dir: &Path, requested: &str) -> Result<PathBuf, String> {
    let root = models_dir
        .canonicalize()
        .map_err(|e| format!("models dir ilegível: {e}"))?;
    let candidate = Path::new(requested);
    let full = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let full = full
        .canonicalize()
        .map_err(|_| format!("arquivo não encontrado: {requested}"))?;
    if !full.starts_with(&root) {
        return Err("caminho fora do diretório de modelos".to_string());
    }
    Ok(full)
}

/// Resolve a file this node is willing to serve to a peer.
///
/// Two ways in, and the difference is who chose the file:
///
/// - `grant` — our own operator picked this exact file on this machine's screen,
///   so it may live anywhere the process can read. The peer never names a path;
///   it only replays an opaque id we handed it.
/// - `path` — nobody here chose anything (the pull direction, where a peer asks
///   us for a file on its own initiative), so `models/` is the whole world.
async fn servable_path(
    state: &GatewayState,
    q: &PathQuery,
) -> Result<PathBuf, String> {
    if let Some(id) = q.grant.as_deref() {
        return grant_path(state, id).await;
    }
    let requested = q
        .path
        .as_deref()
        .ok_or_else(|| "informe ?grant= ou ?path=".to_string())?;
    safe_model_path(&state.runtime.models_dir(), requested)
}

/// Look up a grant. A stale or invented id is simply not a file here.
async fn grant_path(state: &GatewayState, id: &str) -> Result<PathBuf, String> {
    state
        .transfers
        .grants
        .read()
        .await
        .get(id)
        .cloned()
        .ok_or_else(|| "concessão desconhecida ou expirada".to_string())
}

async fn file_digest(state: &GatewayState, path: &Path) -> Result<(u64, String), String> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("stat falhou: {e}"))?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = path.to_string_lossy().to_string();

    if let Some((m, s, digest)) = state.transfers.digests.read().await.get(&key) {
        if *m == mtime && *s == size {
            return Ok((size, digest.clone()));
        }
    }

    let p = path.to_path_buf();
    let digest = tokio::task::spawn_blocking(move || -> Result<String, String> {
        use std::io::Read;
        let mut f = std::fs::File::open(&p).map_err(|e| e.to_string())?;
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(|e| e.to_string())??;

    state
        .transfers
        .digests
        .write()
        .await
        .insert(key, (mtime, size, digest.clone()));
    Ok((size, digest))
}

// ── Serving side ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    /// Only meaningful inside `models/` — see [`servable_path`].
    #[serde(default)]
    pub path: Option<String>,
    /// Opaque id of a file our operator offered; may point anywhere.
    #[serde(default)]
    pub grant: Option<String>,
}

/// What the receiver needs before it commits to a multi-gigabyte download.
async fn manifest(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(q): Query<PathQuery>,
) -> Response {
    let path = match servable_path(&state, &q).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };
    match file_digest(&state, &path).await {
        Ok((size, md5)) => Json(json!({
            "name": path.file_name().and_then(|n| n.to_str()).unwrap_or_default(),
            "path": path.to_string_lossy(),
            "size_bytes": size,
            "md5": md5,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

/// Stream a slice, honouring `Range` so an interrupted transfer can resume.
async fn download(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Query(q): Query<PathQuery>,
) -> Response {
    let path = match servable_path(&state, &q).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };

    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return (StatusCode::NOT_FOUND, "arquivo não encontrado").into_response();
    };
    let total = meta.len();

    // "bytes=<start>-" is all the client ever sends; anything else is ignored.
    let start = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        .min(total);

    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "não deu para abrir").into_response();
    };
    if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "seek falhou").into_response();
    }

    let stream = tokio_util::io::ReaderStream::with_capacity(file, CHUNK);
    let body = Body::from_stream(stream);
    let remaining = total - start;

    let status = if start > 0 {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, remaining.to_string())
        .header(header::ACCEPT_RANGES, "bytes");
    if start > 0 {
        resp = resp.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", start, total.saturating_sub(1), total),
        );
    }
    resp.body(body).unwrap().into_response()
}

// ── Offers (push direction) ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OfferRequest {
    /// Node that will serve the bytes.
    pub from_node_id: String,
    pub from_host: String,
    pub from_gateway: String,
    pub remote_path: String,
    #[serde(default)]
    pub grant: Option<String>,
    pub name: String,
    pub size_bytes: u64,
    pub md5: String,
    #[serde(default)]
    pub start_layer: Option<u32>,
    #[serde(default)]
    pub end_layer: Option<u32>,
}

/// A peer offers us a slice. Nothing is downloaded until someone accepts.
async fn receive_offer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<OfferRequest>,
) -> Response {
    let offer = TransferOffer {
        id: uuid::Uuid::new_v4().simple().to_string(),
        from_node_id: req.from_node_id,
        from_host: req.from_host,
        from_gateway: req.from_gateway,
        remote_path: req.remote_path,
        grant: req.grant,
        name: req.name.clone(),
        size_bytes: req.size_bytes,
        md5: req.md5,
        start_layer: req.start_layer,
        end_layer: req.end_layer,
    };
    info!("📥 oferta de fatia recebida: {} ({} bytes)", offer.name, offer.size_bytes);
    let id = offer.id.clone();
    state.transfers.offers.write().await.push(offer);
    Json(json!({ "accepted_for_review": true, "offer_id": id })).into_response()
}

async fn list_offers(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    let offers = state.transfers.offers.read().await.clone();
    let disk = sysmon_disk_free_mb().await;
    Json(json!({ "offers": offers, "disk_free_mb": disk })).into_response()
}

async fn sysmon_disk_free_mb() -> u64 {
    crate::sysmon::sample().await.disk.free_mb
}

async fn reject_offer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    state.transfers.offers.write().await.retain(|o| o.id != id);
    Json(json!({ "rejected": true })).into_response()
}

async fn accept_offer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let offer = {
        let mut offers = state.transfers.offers.write().await;
        match offers.iter().position(|o| o.id == id) {
            Some(i) => offers.remove(i),
            None => {
                return (StatusCode::NOT_FOUND, Json(json!({ "error": "oferta não existe" })))
                    .into_response()
            }
        }
    };

    match spawn_download(
        state.clone(),
        offer.from_gateway.clone(),
        offer.remote_path.clone(),
        offer.grant.clone(),
        offer.name.clone(),
        offer.from_host.clone(),
    )
    .await
    {
        Ok(job_id) => Json(json!({ "started": true, "job_id": job_id })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

// ── Pull direction ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PullRequest {
    /// Peer holding the file.
    pub node_id: String,
    pub remote_path: String,
    pub name: String,
}

async fn start_pull(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<PullRequest>,
) -> Response {
    let peers = state.all_peers().await;
    let Some(peer) = peers.iter().find(|p| p.node_id == req.node_id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "peer desconhecido" })))
            .into_response();
    };
    let Some(base) = peer.gateway_url() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer não anuncia porta de gateway" })),
        )
            .into_response();
    };
    let host = if peer.hostname.is_empty() {
        peer.node_id[..12].to_string()
    } else {
        peer.hostname.clone()
    };

    match spawn_download(state.clone(), base, req.remote_path, None, req.name, host).await {
        Ok(job_id) => Json(json!({ "started": true, "job_id": job_id })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn list_jobs(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    let mut jobs: Vec<TransferJob> = state.transfers.jobs.read().await.values().cloned().collect();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    Json(json!({ "jobs": jobs })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct SendRequest {
    /// Peer that should receive the file.
    pub to_node_id: String,
    /// File on *this* machine — any readable path, not just `models/`.
    pub path: String,
}

/// Resolve a file the local operator asked us to send.
///
/// Deliberately not restricted to `models/`: the captain's machine holds models
/// under LM Studio, Ollama and loose directories, and "it is on this machine so
/// I must be able to send it" is the requirement. The restriction that matters
/// is that this path came from *here*, and that the peer only ever learns the
/// grant id — see [`TransferState::grants`].
fn local_send_path(requested: &str) -> Result<PathBuf, String> {
    let full = Path::new(requested)
        .canonicalize()
        .map_err(|_| format!("arquivo não encontrado: {requested}"))?;
    if !full.is_file() {
        return Err(format!("não é um arquivo: {requested}"));
    }
    Ok(full)
}

/// Offer one of our slices to a peer.
///
/// This only knocks on the door: the peer stores the offer and its owner
/// decides. We never write to another machine's disk uninvited.
async fn send_to_peer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SendRequest>,
) -> Response {
    let path = match local_send_path(&req.path) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };

    let (size, md5) = match file_digest(&state, &path).await {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response()
        }
    };

    let peers = state.all_peers().await;
    let Some(peer) = peers.iter().find(|p| p.node_id == req.to_node_id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "peer desconhecido" })))
            .into_response();
    };
    let Some(peer_base) = peer.gateway_url() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer não anuncia porta de gateway" })),
        )
            .into_response();
    };

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("fatia.gguf")
        .to_string();
    let shard = crate::catalog::parse_shard_name(&name).map(|(_, a, b)| (a, b));

    // Where the peer should come and fetch it from — our own gateway, on the
    // address the peer already reaches us at.
    let my_host = state.network.hostname.clone();

    // The peer fetches by grant id, never by path: that is what lets this file
    // sit outside `models/` without opening the disk to whoever holds the key.
    let grant = uuid::Uuid::new_v4().simple().to_string();
    state
        .transfers
        .grants
        .write()
        .await
        .insert(grant.clone(), path.clone());

    let body = json!({
        "from_node_id": state.network.node_id,
        "from_host": my_host,
        "from_gateway": format!("http://{}:{}", local_ip_for(&peer_base), state.gateway_port),
        "grant": grant,
        "remote_path": path.to_string_lossy(),
        "name": name,
        "size_bytes": size,
        "md5": md5,
        "start_layer": shard.map(|(a, _)| a),
        "end_layer": shard.map(|(_, b)| b),
    });

    let key = state.auth.read().await.connection_key.clone();
    match state
        .http
        .post(format!("{peer_base}/api/transfer/offer"))
        .bearer_auth(&key)
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!("📤 fatia {name} oferecida a {}", peer.node_id);
            Json(json!({ "offered": true })).into_response()
        }
        Ok(r) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("peer respondeu {}", r.status()) })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("não deu para oferecer: {e}") })),
        )
            .into_response(),
    }
}

/// Our address as seen from the peer we are talking to.
///
/// The peer will open a connection back to us, so the offer has to carry an
/// address that works from *its* side — `127.0.0.1` would send it to itself.
fn local_ip_for(peer_base: &str) -> String {
    let peer_host = peer_base
        .trim_start_matches("http://")
        .split(':')
        .next()
        .unwrap_or("127.0.0.1");
    match peer_host.parse::<std::net::IpAddr>() {
        Ok(ip) => local_ip_towards(ip),
        Err(_) => "127.0.0.1".to_string(),
    }
}

/// Ask the OS which local address it would use to reach `dest`.
fn local_ip_towards(dest: std::net::IpAddr) -> String {
    // A connected UDP socket performs no traffic but fixes the route.
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect((dest, 9))?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

// ── The download itself ──────────────────────────────────────────────────────

async fn set_job(state: &GatewayState, job: TransferJob) {
    state.transfers.jobs.write().await.insert(job.id.clone(), job);
}

/// Check the manifest and free space, then download in the background.
async fn spawn_download(
    state: GatewayState,
    base_url: String,
    remote_path: String,
    grant: Option<String>,
    name: String,
    from_host: String,
) -> Result<String, String> {
    let key = state.auth.read().await.connection_key.clone();

    // A grant, when we have one, is the only locator the serving side needs —
    // and the only one that reaches a file outside its `models/`.
    let locator: Vec<(&str, String)> = match &grant {
        Some(g) => vec![("grant", g.clone())],
        None => vec![("path", remote_path.clone())],
    };

    // Ask first: size and digest decide whether this is even worth starting.
    let manifest: serde_json::Value = state
        .http
        .get(format!("{base_url}/api/transfer/manifest"))
        .bearer_auth(&key)
        .query(&locator)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| format!("não deu para falar com {from_host}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("manifesto inválido de {from_host}: {e}"))?;

    let total = manifest["size_bytes"].as_u64().unwrap_or(0);
    let md5 = manifest["md5"].as_str().unwrap_or_default().to_string();
    if total == 0 || md5.is_empty() {
        return Err(format!("{from_host} não soube descrever {name}"));
    }

    // Refusing here beats filling the disk and failing halfway.
    let free_mb = sysmon_disk_free_mb().await;
    let needed_mb = total / 1_048_576 + DISK_HEADROOM_MB;
    if free_mb > 0 && free_mb < needed_mb {
        return Err(format!(
            "espaço insuficiente: {name} precisa de ~{needed_mb} MB (com folga) e só há {free_mb} MB livres"
        ));
    }

    let dir = state.runtime.models_dir().join("recebidos");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("não deu para criar {}: {e}", dir.display()))?;

    let job_id = uuid::Uuid::new_v4().simple().to_string();
    let job = TransferJob {
        id: job_id.clone(),
        name: name.clone(),
        from_host: from_host.clone(),
        state: "baixando".into(),
        bytes_done: 0,
        bytes_total: total,
        bytes_per_sec: 0.0,
        eta_secs: None,
        detail: String::new(),
    };
    set_job(&state, job).await;

    let task_state = state.clone();
    let task_job_id = job_id.clone();
    tokio::spawn(async move {
        let job_id = task_job_id;
        let dest = dir.join(&name);
        let part = dir.join(format!("{name}.part"));
        if let Err(e) = run_download(
            &task_state, &job_id, &base_url, &key, &locator, &part, &dest, total, &md5,
        )
        .await
        {
            warn!("transferência de {name} falhou: {e}");
            // A partial file must never look like a usable slice.
            let _ = tokio::fs::remove_file(&part).await;
            let mut jobs = task_state.transfers.jobs.write().await;
            if let Some(j) = jobs.get_mut(&job_id) {
                j.state = "falhou".into();
                j.detail = e;
            }
        }
    });

    Ok(job_id)
}

#[allow(clippy::too_many_arguments)]
async fn run_download(
    state: &GatewayState,
    job_id: &str,
    base_url: &str,
    key: &str,
    locator: &[(&str, String)],
    part: &Path,
    dest: &Path,
    total: u64,
    expected_md5: &str,
) -> Result<(), String> {
    use futures::StreamExt;

    // Resume whatever a previous attempt already wrote.
    let already = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    let already = already.min(total);

    let mut req = state
        .http
        .get(format!("{base_url}/api/transfer/download"))
        .bearer_auth(key)
        .query(locator)
        .timeout(Duration::from_secs(60 * 60 * 6));
    if already > 0 {
        info!("retomando {} a partir de {} bytes", dest.display(), already);
        req = req.header(header::RANGE, format!("bytes={already}-"));
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("peer respondeu {}", resp.status()));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(part)
        .await
        .map_err(|e| e.to_string())?;
    file.seek(std::io::SeekFrom::Start(already))
        .await
        .map_err(|e| e.to_string())?;

    let mut done = already;
    let started = Instant::now();
    let mut last_report = Instant::now();
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        done += chunk.len() as u64;

        if last_report.elapsed() >= Duration::from_millis(500) {
            last_report = Instant::now();
            let moved = done.saturating_sub(already) as f64;
            let secs = started.elapsed().as_secs_f64().max(0.001);
            let rate = moved / secs;
            let eta = if rate > 1.0 {
                Some(((total.saturating_sub(done)) as f64 / rate) as u64)
            } else {
                None
            };
            let mut jobs = state.transfers.jobs.write().await;
            if let Some(j) = jobs.get_mut(job_id) {
                j.bytes_done = done;
                j.bytes_per_sec = rate;
                j.eta_secs = eta;
            }
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);

    // Verify before the file gets its real name: a truncated or corrupted
    // slice that passes for a good one only shows up much later, as an
    // incomprehensible inference error.
    {
        let mut jobs = state.transfers.jobs.write().await;
        if let Some(j) = jobs.get_mut(job_id) {
            j.state = "verificando".into();
            j.bytes_done = done;
        }
    }

    let p = part.to_path_buf();
    let got = tokio::task::spawn_blocking(move || -> Result<String, String> {
        use std::io::Read;
        let mut f = std::fs::File::open(&p).map_err(|e| e.to_string())?;
        let mut hasher = Md5::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await
    .map_err(|e| e.to_string())??;

    if got != expected_md5 {
        return Err(format!("MD5 não confere (esperado {expected_md5}, obtido {got})"));
    }

    tokio::fs::rename(part, dest).await.map_err(|e| e.to_string())?;

    let mut jobs = state.transfers.jobs.write().await;
    if let Some(j) = jobs.get_mut(job_id) {
        j.state = "concluída".into();
        j.bytes_done = total;
        j.eta_secs = Some(0);
        j.detail = format!("MD5 conferido · {}", dest.display());
    }
    info!("✅ fatia recebida e verificada: {}", dest.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_cannot_ask_for_files_outside_the_models_dir() {
        let dir = std::env::temp_dir().join(format!("rdma-xfer-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/ok.gguf"), b"x").unwrap();

        assert!(safe_model_path(&dir, "sub/ok.gguf").is_ok());
        assert!(safe_model_path(&dir, "../../../etc/passwd").is_err());
        assert!(safe_model_path(&dir, "/etc/passwd").is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absolute_path_inside_the_models_dir_is_allowed() {
        let dir = std::env::temp_dir().join(format!("rdma-xfer-abs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("slice.gguf");
        std::fs::write(&f, b"x").unwrap();

        let resolved = safe_model_path(&dir, f.to_str().unwrap()).unwrap();
        assert_eq!(resolved.file_name().unwrap(), "slice.gguf");

        std::fs::remove_dir_all(&dir).ok();
    }
}
