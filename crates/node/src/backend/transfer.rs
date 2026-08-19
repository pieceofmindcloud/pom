//! Moving model slices between machines.
//!
//! Two directions, both ending in the same download:
//!
//! * **pull** — this node asks a peer for a slice it does not have;
//! * **push** — a node *offers* a slice to a peer, the peer's owner accepts,
//!   and the peer then pulls it. The sender never writes to the receiver's
//!   disk on its own; acceptance is what starts the transfer. A development
//!   machine can waive that click with `POM_DEV_AUTO_ACCEPT_TRANSFERS`,
//!   which only the *receiver* can set and which is off everywhere else —
//!   see [`dev_auto_accept_env`].
//!
//! The bytes travel over the peers' existing HTTP gateway rather than the
//! libp2p bitswap behaviour: that is the path already proven to carry traffic
//! between these machines (the inference pipeline uses it), and it gives byte
//! ranges — so an interrupted multi-gigabyte transfer resumes instead of
//! starting over.
//!
//! What may leave a machine depends on who chose the file. A **pull** names a
//! path on a machine where nobody chose anything, so it is confined to the
//! models directory. A **push** is this machine's own operator choosing one
//! filename from the catalog; the backend resolves that basename against the
//! configured roots and the peer receives an opaque grant id. See
//! [`TransferState::grants`].
//!
//! Received files are always written under `<pasta de fatias>/recebidos/` — the
//! directory configured in `pom.json`, `models/` until someone changes it —
//! and a file only takes its final name once its MD5 matches the sender's.

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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Read/written in chunks — a slice is gigabytes, it never sits in memory.
const CHUNK: usize = 1024 * 1024;

/// Development-only: take every incoming offer without asking a human.
///
/// The whole point of an offer is that nobody writes gigabytes to somebody
/// else's disk without their consent, so this is **off unless the receiving
/// machine sets it**, and it is the receiver's own variable — a sender cannot
/// turn it on remotely. It exists because testing the push direction by hand
/// means driving two browsers to move one file. Never set it on a machine whose
/// disk you care about.
fn dev_auto_accept_env() -> String {
    crate::brand::env_var("DEV_AUTO_ACCEPT_TRANSFERS")
}

/// Whether this process was started with the auto-accept gate open.
///
/// Read once: a variable flipped after start would make two offers in the same
/// session behave differently, and "why did that one need a click?" is not a
/// question worth having.
fn dev_auto_accept() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        auto_accept_set(
            crate::brand::env("DEV_AUTO_ACCEPT_TRANSFERS")
                .ok()
                .as_deref(),
        )
    })
}

/// A gate this dangerous does not open by accident: unset, empty, `0` and
/// `false` all mean off, so `export …=` in a stale shell profile cannot leave a
/// machine silently taking whatever the network offers it.
fn auto_accept_set(raw: Option<&str>) -> bool {
    match raw.map(str::trim) {
        None | Some("") | Some("0") => false,
        Some(v) => !v.eq_ignore_ascii_case("false"),
    }
}

/// Headroom demanded on top of the file size before a download starts.
const DISK_HEADROOM_MB: u64 = 512;

/// Stable wire values consumed by the web UI. Keep these as protocol terms;
/// presentation is localized in the client.
const TRANSFER_RUNNING: &str = "running";
const TRANSFER_VERIFYING: &str = "verifying";
const TRANSFER_DONE: &str = "done";
const TRANSFER_FAILED: &str = "failed";
const TRANSFER_CANCELLED: &str = "cancelled";

/// Direções do mesmo job. O id é um só nas duas máquinas; o que muda é de que
/// lado dele cada uma está.
const TRANSFER_SENDING: &str = "sending";
const TRANSFER_RECEIVING: &str = "receiving";

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
        .route("/api/transfer/jobs/:id/cancel", post(cancel_job))
        // Push direction, driven from this node's screen
        .route("/api/transfer/send", post(send_to_peer))
}

// ── Shared state ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct TransferJob {
    pub id: String,
    pub name: String,
    /// `sending` ou `receiving`. O mesmo id existe nas duas máquinas, com
    /// direções opostas — é assim que os dois lados falam da mesma
    /// transferência sem inventar um segundo vocabulário.
    pub direction: &'static str,
    /// A outra máquina. Substitui `from_host`, que mentia na direção de envio.
    pub peer_host: String,
    /// Arquivo local envolvido. A exclusão precisa saber o que está em trânsito.
    pub path: String,
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
    /// Vazio, e de propósito: **a oferta não carrega digest**.
    ///
    /// Quem confere o arquivo é o receptor, com o MD5 que ele mesmo pede em
    /// `/api/transfer/manifest` depois de aceitar — este campo nunca foi lido
    /// por ninguém. Calculá-lo aqui custava uma leitura do arquivo inteiro
    /// *antes* de a oferta sair, e era isso que segurava a notificação do
    /// outro lado por minutos. O campo continua na estrutura porque um peer de
    /// versão anterior o exige no corpo do pedido.
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
    /// Ids cancelados. Guardados mesmo para job que ainda não existe aqui: o
    /// aviso do outro lado pode chegar antes de o download começar, e sem isso
    /// a transferência arrancaria já cancelada e ninguém a pararia.
    cancelled: RwLock<HashSet<String>>,
}

impl TransferState {
    /// Registra uma transferência em curso. Chamado pelos dois lados, com o
    /// mesmo id.
    pub async fn start_job(
        &self,
        id: &str,
        name: &str,
        direction: &'static str,
        peer_host: &str,
        path: &str,
        bytes_total: u64,
    ) {
        self.jobs.write().await.insert(
            id.to_string(),
            TransferJob {
                id: id.to_string(),
                name: name.to_string(),
                direction,
                peer_host: peer_host.to_string(),
                path: path.to_string(),
                state: TRANSFER_RUNNING.into(),
                bytes_done: 0,
                bytes_total,
                bytes_per_sec: 0.0,
                eta_secs: None,
                detail: String::new(),
            },
        );
    }

    /// Progresso, com a mesma aritmética nos dois lados.
    ///
    /// `moved` é o que andou nesta tentativa, que não é `done` quando a
    /// transferência retomou de um `.part`: usar `done` inflaria a velocidade
    /// com bytes que já estavam no disco.
    pub async fn progress(&self, id: &str, done: u64, moved: u64, elapsed: Duration) {
        let secs = elapsed.as_secs_f64().max(0.001);
        let rate = moved as f64 / secs;
        let mut jobs = self.jobs.write().await;
        if let Some(j) = jobs.get_mut(id) {
            j.bytes_done = done;
            j.bytes_per_sec = rate;
            j.eta_secs = if rate > 1.0 {
                Some(((j.bytes_total.saturating_sub(done)) as f64 / rate) as u64)
            } else {
                None
            };
        }
    }

    pub async fn is_cancelled(&self, id: &str) -> bool {
        self.cancelled.read().await.contains(id)
    }

    /// Marca o cancelamento e rotula o job com quem pediu.
    ///
    /// "Cancelado por ksia" e "falhou" são coisas diferentes para quem olha a
    /// tela: a primeira foi decisão de alguém, a segunda é motivo para tentar
    /// de novo.
    pub async fn cancel(&self, id: &str, by: &str) {
        self.cancelled.write().await.insert(id.to_string());
        let mut jobs = self.jobs.write().await;
        if let Some(j) = jobs.get_mut(id) {
            if j.state != TRANSFER_DONE {
                j.state = TRANSFER_CANCELLED.into();
                j.detail = format!("cancelado por {by}");
            }
        }
    }
}

pub type Transfers = Arc<TransferState>;

// ── Path safety ──────────────────────────────────────────────────────────────

/// Assinatura de um arquivo GGUF: os 4 primeiros bytes.
const GGUF_MAGIC: &[u8; 4] = b"GGUF";

/// O único tipo de arquivo que pode sair desta máquina ou ser apagado por ela.
///
/// O conteúdo e a extensão precisam confirmar que é um GGUF.
pub(crate) fn ensure_gguf(path: &Path) -> Result<(), String> {
    use std::io::Read;

    if !path.is_file() {
        return Err(format!("não é um arquivo: {}", path.display()));
    }

    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
    {
        return Err(format!(
            "só arquivos com extensão .gguf podem ser transferidos: {}",
            path.display()
        ));
    }

    let mut head = [0u8; 4];
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("não deu para abrir {}: {e}", path.display()))?;
    file.read_exact(&mut head)
        .map_err(|_| format!("{} é curto demais para ser um GGUF", path.display()))?;
    if &head != GGUF_MAGIC {
        return Err(format!(
            "{} não é um GGUF (assinatura {head:02x?})",
            path.display()
        ));
    }
    Ok(())
}

fn valid_transfer_filename(raw: &str) -> Result<&str, String> {
    let filename = raw.trim();
    let path = Path::new(filename);
    if filename.is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || path.file_name().and_then(|name| name.to_str()) != Some(filename)
    {
        return Err("a transferência aceita apenas o nome de um arquivo, sem caminho".to_string());
    }
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
    {
        return Err("só arquivos com extensão .gguf podem ser transferidos".to_string());
    }
    Ok(filename)
}

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
async fn servable_path(state: &GatewayState, q: &PathQuery) -> Result<PathBuf, String> {
    let path = if let Some(id) = q.grant.as_deref() {
        grant_path(state, id).await?
    } else {
        let requested = q
            .path
            .as_deref()
            .ok_or_else(|| "informe ?grant= ou ?path=".to_string())?;
        safe_model_path(&state.shard_dir().await, requested)?
    };
    // Também no caminho da concessão: é por ali que sai um arquivo de fora de
    // `models/`, e era a única porta sem checagem de tipo.
    ensure_gguf(&path)?;
    Ok(path)
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
    /// Id cunhado pelo receptor. Ausente quando o peer é de uma versão antiga:
    /// a transferência funciona igual, só sem visão deste lado.
    #[serde(default)]
    pub job: Option<String>,
    /// `node_id` do receptor. Resolvido para nome pela tabela de peers local —
    /// nada que descreva o outro lado vem escrito no pedido.
    #[serde(default)]
    pub peer: Option<String>,
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
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
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

    // O lado que envia entra na própria lista de transferências. Ele é quem
    // transmite os bytes, então já sabe o progresso: não há relato de volta
    // para se perder, e o número continua certo com a tela do outro fechada.
    if let Some(job_id) = q.job.as_deref() {
        let peer_host = peer_label(&state, q.peer.as_deref()).await;
        state
            .transfers
            .start_job(
                job_id,
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("fatia.gguf"),
                TRANSFER_SENDING,
                &peer_host,
                &path.to_string_lossy(),
                total,
            )
            .await;
    }

    let reader = tokio_util::io::ReaderStream::with_capacity(file, CHUNK);
    let body = match q.job.clone() {
        Some(job_id) => Body::from_stream(served_stream(state.clone(), job_id, reader, start)),
        None => Body::from_stream(reader),
    };
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

/// Nome legível do peer que está do outro lado, a partir do `node_id` que ele
/// mandou. A tradução sai da tabela local: o pedido nunca traz o nome pronto.
async fn peer_label(state: &GatewayState, node_id: Option<&str>) -> String {
    let Some(node_id) = node_id else {
        return "desconhecido".to_string();
    };
    let short = |id: &str| id[..12.min(id.len())].to_string();
    state
        .all_peers()
        .await
        .iter()
        .find(|p| p.node_id == node_id)
        .map(|p| {
            if p.hostname.is_empty() {
                short(&p.node_id)
            } else {
                p.hostname.clone()
            }
        })
        .unwrap_or_else(|| short(node_id))
}

/// Envolve o stream do arquivo para contar o que já saiu e para parar cedo
/// quando o job é cancelado.
fn served_stream(
    state: GatewayState,
    job_id: String,
    inner: tokio_util::io::ReaderStream<tokio::fs::File>,
    already: u64,
) -> impl futures::Stream<Item = std::io::Result<axum::body::Bytes>> {
    use futures::StreamExt;

    let started = Instant::now();
    futures::stream::unfold(
        (
            inner,
            state,
            job_id,
            already,
            already,
            started,
            Instant::now(),
        ),
        |(mut inner, state, job_id, mut done, already, started, mut last)| async move {
            if state.transfers.is_cancelled(&job_id).await {
                // Terminar o corpo aqui corta a transferência: o receptor vê o
                // stream acabar antes do Content-Length prometido.
                return None;
            }
            let chunk = inner.next().await?;
            if let Ok(ref b) = chunk {
                done += b.len() as u64;
                if last.elapsed() >= Duration::from_millis(500) {
                    last = Instant::now();
                    state
                        .transfers
                        .progress(
                            &job_id,
                            done,
                            done.saturating_sub(already),
                            started.elapsed(),
                        )
                        .await;
                }
            }
            Some((chunk, (inner, state, job_id, done, already, started, last)))
        },
    )
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
    /// Opcional: um remetente atualizado não calcula digest para oferecer, e um
    /// antigo ainda manda o dele. Nenhum dos dois é lido — ver
    /// [`TransferOffer::md5`].
    #[serde(default)]
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
    if let Err(e) = valid_transfer_filename(&req.name) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
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
    info!(
        "📥 oferta de fatia recebida: {} ({} bytes)",
        offer.name, offer.size_bytes
    );
    let id = offer.id.clone();

    // Development gate, off by default and read from *this* machine's
    // environment: the sender has no way to ask for it. See `dev_auto_accept_env`.
    if dev_auto_accept() {
        warn!(
            "⚙️  {} ligado — aceitando {} sem confirmação",
            dev_auto_accept_env(),
            offer.name
        );
        return match spawn_download(
            state.clone(),
            offer.from_node_id.clone(),
            offer.from_gateway.clone(),
            offer.remote_path.clone(),
            offer.grant.clone(),
            offer.name.clone(),
            offer.from_host.clone(),
        )
        .await
        {
            Ok(job_id) => Json(json!({
                "accepted_for_review": false,
                "auto_accepted": true,
                "offer_id": id,
                "job_id": job_id,
            }))
            .into_response(),
            // The offer still goes on the shelf: an auto-accept that could not
            // start is a transfer the operator can still take by hand, and
            // swallowing it would lose the file with only a log line to say so.
            Err(e) => {
                warn!("aceite automático de {} falhou: {e}", offer.name);
                state.transfers.offers.write().await.push(offer);
                Json(json!({
                    "accepted_for_review": true,
                    "auto_accept_failed": e,
                    "offer_id": id,
                }))
                .into_response()
            }
        };
    }

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
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "oferta não existe" })),
                )
                    .into_response()
            }
        }
    };

    match spawn_download(
        state.clone(),
        offer.from_node_id.clone(),
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
    if let Err(e) = valid_transfer_filename(&req.name) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    let peers = state.all_peers().await;
    let Some(peer) = peers.iter().find(|p| p.node_id == req.node_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "peer desconhecido" })),
        )
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

    match spawn_download(
        state.clone(),
        peer.node_id.clone(),
        base,
        req.remote_path,
        None,
        req.name,
        host,
    )
    .await
    {
        Ok(job_id) => Json(json!({ "started": true, "job_id": job_id })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn list_jobs(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    let mut jobs: Vec<TransferJob> = state
        .transfers
        .jobs
        .read()
        .await
        .values()
        .cloned()
        .collect();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    Json(json!({ "jobs": jobs })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct CancelRequest {
    /// Máquina que pediu o cancelamento, para o outro lado poder dizer quem foi.
    #[serde(default)]
    pub by: Option<String>,
    /// `true` quando a chamada veio do peer. Evita o eco: quem foi avisado não
    /// avisa de volta.
    #[serde(default)]
    pub from_peer: bool,
}

/// Cancela uma transferência.
///
/// A mesma rota serve para a tela local e para o aviso que o outro lado manda —
/// um verbo só, com a mesma semântica nas duas pontas.
async fn cancel_job(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
    body: Option<Json<CancelRequest>>,
) -> Response {
    let req = body.map(|Json(b)| b).unwrap_or(CancelRequest {
        by: None,
        from_peer: false,
    });
    let by = req.by.unwrap_or_else(|| state.network.hostname.clone());

    let peer_host = state
        .transfers
        .jobs
        .read()
        .await
        .get(&id)
        .map(|j| j.peer_host.clone());

    state.transfers.cancel(&id, &by).await;
    info!("🛑 transferência {id} cancelada por {by}");

    // Avisar é o que separa "cancelado" de "caiu a rede" na tela do outro lado.
    // Se o aviso não chegar, o cancelamento local continua valendo e o outro
    // lado cai no caminho de conexão morta, que reporta o motivo real.
    if !req.from_peer {
        if let Some(host) = peer_host {
            notify_peer_cancel(&state, &id, &by, &host).await;
        }
    }

    Json(json!({ "cancelled": true })).into_response()
}

/// Repassa o cancelamento ao peer que está do outro lado desta transferência.
async fn notify_peer_cancel(state: &GatewayState, id: &str, by: &str, peer_host: &str) {
    let peers = state.all_peers().await;
    let Some(peer) = peers
        .iter()
        .find(|p| p.hostname == peer_host || p.node_id.starts_with(peer_host))
    else {
        warn!("não deu para avisar {peer_host} do cancelamento de {id}: peer desconhecido");
        return;
    };
    let Some(base) = peer.gateway_url() else {
        warn!("não deu para avisar {peer_host}: peer não anuncia porta de gateway");
        return;
    };
    let key = state.auth.read().await.connection_key.clone();
    let Ok(cliente) = state.peer_http.for_peer(&peer.node_id) else {
        warn!("não deu para avisar {peer_host}: sem cliente para o peer");
        return;
    };
    let sent = cliente
        .post(format!("{base}/api/transfer/jobs/{id}/cancel"))
        .bearer_auth(&key)
        .json(&json!({ "by": by, "from_peer": true }))
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    if let Err(e) = sent {
        warn!("aviso de cancelamento não chegou em {peer_host}: {e}");
    }
}

#[derive(Debug, Deserialize)]
pub struct SendRequest {
    /// Peer that should receive the file.
    pub to_node_id: String,
    /// Basename from this node's catalog. Paths are deliberately not accepted.
    pub filename: String,
}

/// Resolve the basename the local operator selected against this node's catalog.
///
/// The browser never gets permission to choose a path. A duplicate basename is
/// rejected rather than guessed, because guessing would make a transfer depend
/// on scan order and could send a different model than the operator saw.
async fn catalog_send_path(state: &GatewayState, filename: &str) -> Result<PathBuf, String> {
    let filename = valid_transfer_filename(filename)?;
    let model_dirs = state.model_dirs().await;
    let shard_dir = state.shard_dir().await;
    let catalog =
        tokio::task::spawn_blocking(move || crate::catalog::discover(&model_dirs, &shard_dir))
            .await
            .map_err(|e| format!("falha ao consultar o catálogo local: {e}"))?;
    let matches: Vec<PathBuf> = catalog
        .models
        .into_iter()
        .filter(|model| {
            Path::new(&model.path)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(filename)
        })
        .map(|model| PathBuf::from(model.path))
        .collect();
    match matches.as_slice() {
        [] => Err(format!("arquivo não encontrado no catálogo local: {filename}")),
        [path] => local_send_path(path.to_string_lossy().as_ref()),
        _ => Err(format!(
            "há mais de um arquivo chamado {filename} no catálogo local; renomeie um deles antes de transferir"
        )),
    }
}

/// Validate a path already resolved by the local catalog before granting it.
fn local_send_path(requested: &str) -> Result<PathBuf, String> {
    let full = Path::new(requested)
        .canonicalize()
        .map_err(|_| format!("arquivo não encontrado: {requested}"))?;
    if !full.is_file() {
        return Err(format!("não é um arquivo: {requested}"));
    }
    ensure_gguf(&full)?;
    Ok(full)
}

/// Offer one of our slices to a peer.
///
/// This only knocks on the door: the peer stores the offer and its owner
/// decides. We never write to another machine's disk uninvited.
///
/// **Bater na porta não pode custar o preço de ler o arquivo.** Este caminho já
/// calculou o MD5 da fatia antes de postar a oferta, e como o digest é uma
/// leitura do arquivo inteiro, o sino do outro lado só acendia quando ela
/// terminava: medidos 3 min 23 s para uma fatia de 4,2 GB no cartão SD
/// (~20 MB/s). O cache por (caminho, mtime, tamanho) mascarava o atraso no
/// segundo envio do mesmo arquivo, e era daí que vinha o "às vezes" do relato.
/// O número nem era usado: quem confere o download é o receptor, com o MD5 que
/// ele pede em `/api/transfer/manifest` depois de aceitar. Aqui basta o
/// `stat` — tamanho e nome, que é o que a notificação mostra.
async fn send_to_peer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SendRequest>,
) -> Response {
    let path = match catalog_send_path(&state, &req.filename).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };

    let size = match tokio::fs::metadata(&path).await {
        Ok(m) => m.len(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("stat falhou: {e}") })),
            )
                .into_response()
        }
    };

    let peers = state.all_peers().await;
    let Some(peer) = peers.iter().find(|p| p.node_id == req.to_node_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "peer desconhecido" })),
        )
            .into_response();
    };
    let Some(peer_base) = peer.gateway_url() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer não anuncia porta de gateway" })),
        )
            .into_response();
    };

    let name = req.filename.trim().to_string();
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
        // A pinned address beats one read off the routing table: with privacy
        // extensions the outbound source is a temporary IPv6 that rotates, and a
        // multi-gigabyte download outlives it.
        // O ESQUEMA em vigor, nunca "http" fixo. Com o cluster em TLS, quem
        // aceitava discava http e o gateway respondia 403 em texto puro
        // ("este cluster exige TLS entre nós; refaça a chamada em https"); o
        // `.json()` do outro lado caía nessa frase e o operador via
        // "manifesto inválido: error decoding response body" (18/08).
        "from_gateway": crate::peer_client::peer_base_url(
            &state
                .network
                .advertised_host()
                .unwrap_or_else(|| local_ip_for(&peer_base)),
            state.gateway_port,
            crate::peer_client::tls_ativo(),
        ),
        "grant": grant,
        "remote_path": path.to_string_lossy(),
        "name": name,
        "size_bytes": size,
        // Sempre vazio. Viaja só porque um peer de versão anterior exige o
        // campo — ver `TransferOffer::md5`.
        "md5": "",
        "start_layer": shard.map(|(a, _)| a),
        "end_layer": shard.map(|(_, b)| b),
    });

    let key = state.auth.read().await.connection_key.clone();
    let cliente = match state.peer_http.for_peer(&req.to_node_id) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("sem cliente para o peer: {e}") })),
            )
                .into_response()
        }
    };
    match cliente
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

/// Our address as seen from the peer we are talking to, ready to drop into a URL.
///
/// The peer will open a connection back to us, so the offer has to carry an
/// address that works from *its* side — `127.0.0.1` would send it to itself.
/// That is exactly what happened to every IPv6 peer: the host was pulled out
/// with `split(':')`, which on `http://[2804:...]:8090` yields `"[2804"`, fails
/// to parse as an address, and fell back to loopback. The receiver then
/// downloaded from its own machine.
fn local_ip_for(peer_base: &str) -> String {
    match url_host(peer_base).parse::<std::net::IpAddr>() {
        Ok(ip) => local_ip_towards(ip),
        Err(_) => "127.0.0.1".to_string(),
    }
}

/// Host part of `http://host:port`, with IPv6 brackets stripped.
fn url_host(base: &str) -> &str {
    let rest = base
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    if let Some(inner) = rest.strip_prefix('[') {
        return inner.split(']').next().unwrap_or("127.0.0.1");
    }
    rest.split(':').next().unwrap_or("127.0.0.1")
}

/// Ask the OS which local address it would use to reach `dest`.
///
/// Returned bracketed when it is IPv6, because the only caller interpolates it
/// straight into a URL.
pub(crate) fn local_ip_towards(dest: std::net::IpAddr) -> String {
    // A connected UDP socket performs no traffic but fixes the route. It has to
    // be bound in the destination's family: connecting a v4 socket to a v6
    // address fails outright.
    let bind: &str = if dest.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    std::net::UdpSocket::bind(bind)
        .and_then(|s| {
            s.connect((dest, 9))?;
            s.local_addr()
        })
        .map(|a| match a.ip() {
            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
            ip => ip.to_string(),
        })
        .unwrap_or_else(|_| {
            if dest.is_ipv6() {
                "[::1]".to_string()
            } else {
                "127.0.0.1".to_string()
            }
        })
}

// ── The download itself ──────────────────────────────────────────────────────

/// Prazo do manifesto.
///
/// O manifesto é onde o MD5 do arquivo é finalmente calculado, e um digest é
/// uma leitura do arquivo inteiro: no cartão SD do captain, 4,2 GB levam
/// 3 min 23 s. Os 120 s de antes só passavam porque o remetente já tinha
/// hasheado ao oferecer — o que era exatamente o defeito. Com o hash de volta
/// para onde ele é preciso, o prazo tem que caber num volume lento; ele corre
/// em segundo plano, então esperar aqui não prende ninguém.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Registra a transferência e faz o resto — manifesto, espaço em disco e
/// download — em segundo plano.
///
/// Consultar o outro lado **não** acontece antes de devolver: o manifesto pode
/// levar minutos (ver [`MANIFEST_TIMEOUT`]), e aceitar uma oferta é um clique,
/// não uma espera. O que o operador ganha em troca é o job já na lista de
/// transferências, dizendo em que etapa está; um erro de manifesto vira job
/// `failed` com o motivo, no mesmo lugar onde ele já procuraria.
/// `peer_id` é o node_id de quem serve os bytes: é o que fixa o certificado do
/// outro lado quando o cluster está em TLS. Vem da oferta (`from_node_id`) ou
/// do `PeerInfo` que o operador escolheu.
/// O esquema que **esta** máquina vai discar, seja qual for o que a oferta traz.
///
/// A oferta é escrita pelo remetente, e um nó de versão anterior grava `http://`
/// fixo. Confiar nisso com o cluster em TLS rende 403 em texto puro
/// ("este cluster exige TLS entre nós"), que o `.json()` transforma em
/// "manifesto inválido: error decoding response body" - uma mensagem que não
/// aponta para lugar nenhum. A política local é a que vale para discar, então é
/// ela que decide.
fn normaliza_esquema(url: &str) -> String {
    let scheme = crate::peer_client::peer_scheme();
    match url.split_once("://") {
        Some((atual, resto)) if atual != scheme => format!("{scheme}://{resto}"),
        _ => url.to_string(),
    }
}

async fn spawn_download(
    state: GatewayState,
    peer_id: String,
    base_url: String,
    remote_path: String,
    grant: Option<String>,
    name: String,
    from_host: String,
) -> Result<String, String> {
    valid_transfer_filename(&name)?;
    // A oferta traz o esquema que o REMETENTE gravou; quem disca é esta
    // máquina, então a política local é a que vale. Ver `normaliza_esquema`.
    let base_url = normaliza_esquema(&base_url);
    let key = state.auth.read().await.connection_key.clone();

    // O id nasce aqui e viaja no pedido: o outro lado registra o mesmo id, e é
    // isso que faz "cancelar" valer para a transferência inteira, dos dois
    // lados, em vez de só para a metade local.
    let job_id = uuid::Uuid::new_v4().simple().to_string();

    // A grant, when we have one, is the only locator the serving side needs —
    // and the only one that reaches a file outside its `models/`.
    let mut locator: Vec<(&str, String)> = match &grant {
        Some(g) => vec![("grant", g.clone())],
        None => vec![("path", remote_path.clone())],
    };
    // Opcionais de propósito: um peer que ainda não tenha esta versão ignora os
    // dois e a transferência funciona como antes, só sem visão do lado que envia.
    locator.push(("job", job_id.clone()));
    locator.push(("peer", state.network.node_id.clone()));

    // O diretório é a única coisa que precisa dar certo já: sem ele não há
    // transferência nenhuma para pôr na lista.
    let dir = state.shard_dir().await.join("recebidos");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("não deu para criar {}: {e}", dir.display()))?;

    // `bytes_total` 0 até o manifesto responder — a barra fica indeterminada, o
    // que é a verdade: ainda não se sabe o tamanho.
    state
        .transfers
        .start_job(
            &job_id,
            &name,
            TRANSFER_RECEIVING,
            &from_host,
            &dir.join(&name).to_string_lossy(),
            0,
        )
        .await;
    set_detail(&state, &job_id, "conferindo o arquivo na origem").await;

    let task_state = state.clone();
    let task_job_id = job_id.clone();
    tokio::spawn(async move {
        let job_id = task_job_id;
        let dest = dir.join(&name);
        let part = dir.join(format!("{name}.part"));
        let outcome = async {
            let (total, md5) = fetch_manifest(
                &task_state,
                &peer_id,
                &base_url,
                &key,
                &locator,
                &from_host,
                &name,
            )
            .await?;
            {
                let mut jobs = task_state.transfers.jobs.write().await;
                if let Some(j) = jobs.get_mut(&job_id) {
                    j.bytes_total = total;
                    j.detail = String::new();
                }
            }
            run_download(
                &task_state,
                &job_id,
                &peer_id,
                &base_url,
                &key,
                &locator,
                &part,
                &dest,
                total,
                &md5,
            )
            .await
        }
        .await;

        if let Err(e) = outcome {
            warn!("transferência de {name} falhou: {e}");
            // A partial file must never look like a usable slice.
            let _ = tokio::fs::remove_file(&part).await;
            let mut jobs = task_state.transfers.jobs.write().await;
            if let Some(j) = jobs.get_mut(&job_id) {
                // `cancel` já rotulou este job com quem pediu; sobrescrever
                // trocaria uma decisão de alguém por um "falhou" anônimo.
                if j.state != TRANSFER_CANCELLED {
                    j.state = TRANSFER_FAILED.into();
                    j.detail = e;
                }
            }
        }
    });

    Ok(job_id)
}

/// Etapa corrente de um job, para a tela ter o que dizer enquanto se espera.
async fn set_detail(state: &GatewayState, job_id: &str, detail: &str) {
    let mut jobs = state.transfers.jobs.write().await;
    if let Some(j) = jobs.get_mut(job_id) {
        j.detail = detail.to_string();
    }
}

/// Tamanho e digest do arquivo do outro lado — o que decide se vale começar.
async fn fetch_manifest(
    state: &GatewayState,
    peer_id: &str,
    base_url: &str,
    key: &str,
    locator: &[(&str, String)],
    from_host: &str,
    name: &str,
) -> Result<(u64, String), String> {
    let resp = state
        .peer_http
        .for_peer(peer_id)
        .map_err(|e| e.to_string())?
        .get(format!("{base_url}/api/transfer/manifest"))
        .bearer_auth(key)
        .query(locator)
        .timeout(MANIFEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("não deu para falar com {from_host}: {e}"))?;

    // Conferir o status ANTES de tentar ler JSON. Um erro do gateway costuma
    // vir em texto puro - o 403 da catraca de TLS é literalmente a frase "este
    // cluster exige TLS entre nós; refaça a chamada em https" -, e o `.json()`
    // em cima dele produzia "manifesto inválido: error decoding response body",
    // que não aponta para lugar nenhum. A explicação do outro lado é a melhor
    // que existe: repetir ela.
    let status = resp.status();
    if !status.is_success() {
        let corpo = resp.text().await.unwrap_or_default();
        let corpo = corpo.trim();
        return Err(if corpo.is_empty() {
            format!("{from_host} recusou o manifesto: HTTP {status}")
        } else {
            format!("{from_host} recusou o manifesto (HTTP {status}): {corpo}")
        });
    }

    let manifest: serde_json::Value = resp
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

    Ok((total, md5))
}

#[allow(clippy::too_many_arguments)]
async fn run_download(
    state: &GatewayState,
    job_id: &str,
    peer_id: &str,
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
    let already = tokio::fs::metadata(part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let already = already.min(total);

    let mut req = state
        .peer_http
        .for_peer(peer_id)
        .map_err(|e| e.to_string())?
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
        // Junto com o status vai a frase do outro lado: ver a nota gêmea em
        // `fetch_manifest`.
        let status = resp.status();
        let corpo = resp.text().await.unwrap_or_default();
        let corpo = corpo.trim();
        return Err(if corpo.is_empty() {
            format!("peer respondeu {status}")
        } else {
            format!("peer respondeu {status}: {corpo}")
        });
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
        if state.transfers.is_cancelled(job_id).await {
            drop(file);
            // Cancelar é desistir: o parcial sai do disco. Guardá-lo ocuparia
            // gigabytes que a tela não mostra.
            let _ = tokio::fs::remove_file(part).await;
            return Err("cancelado".to_string());
        }
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        done += chunk.len() as u64;

        if last_report.elapsed() >= Duration::from_millis(500) {
            last_report = Instant::now();
            state
                .transfers
                .progress(
                    job_id,
                    done,
                    done.saturating_sub(already),
                    started.elapsed(),
                )
                .await;
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
            j.state = TRANSFER_VERIFYING.into();
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
        return Err(format!(
            "MD5 não confere (esperado {expected_md5}, obtido {got})"
        ));
    }

    // No Windows o rename não substitui: se o destino existe, ele falha com
    // "acesso negado" em vez de trocar o arquivo. Chegar aqui com o destino
    // ocupado significa que o arquivo foi rebaixado enquanto este download
    // corria, e o que acabou de passar pelo MD5 é o bom.
    #[cfg(windows)]
    if dest.exists() {
        let _ = tokio::fs::remove_file(dest).await;
    }

    tokio::fs::rename(part, dest)
        .await
        .map_err(|e| e.to_string())?;

    let mut jobs = state.transfers.jobs.write().await;
    if let Some(j) = jobs.get_mut(job_id) {
        j.state = TRANSFER_DONE.into();
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

    /// O aceite automático escreve gigabytes no disco de quem recebe sem
    /// perguntar. Ele só liga quando alguém escreveu um valor de verdade na
    /// variável desta máquina - variável ausente, vazia ou desligada continuam
    /// exigindo o clique, que é o padrão em produção.
    #[test]
    fn auto_accept_stays_shut_unless_explicitly_opened() {
        assert!(
            !auto_accept_set(None),
            "sem a variável, ninguém aceita nada"
        );
        assert!(!auto_accept_set(Some("")));
        assert!(!auto_accept_set(Some("  ")));
        assert!(!auto_accept_set(Some("0")));
        assert!(!auto_accept_set(Some("false")));
        assert!(!auto_accept_set(Some("FALSE")));

        assert!(auto_accept_set(Some("1")));
        assert!(auto_accept_set(Some("true")));
        assert!(auto_accept_set(Some(" 1 ")));
    }

    /// O remetente precisa aparecer na própria lista, com direção e o mesmo id
    /// que o receptor cunhou - é o id compartilhado que torna o cancelamento
    /// simétrico possível.
    #[tokio::test]
    async fn a_served_download_registers_a_sending_job() {
        let transfers = TransferState::default();

        transfers
            .start_job(
                "job-1",
                "fatia_0_15.gguf",
                TRANSFER_SENDING,
                "ksia",
                "/models/fatia_0_15.gguf",
                4_096,
            )
            .await;

        let jobs = transfers.jobs.read().await;
        let job = jobs.get("job-1").expect("job registrado");
        assert_eq!(job.direction, TRANSFER_SENDING);
        assert_eq!(job.peer_host, "ksia");
        assert_eq!(job.bytes_total, 4_096);
        assert_eq!(job.state, TRANSFER_RUNNING);
        assert_eq!(job.path, "/models/fatia_0_15.gguf");
    }

    /// Cancelar é idempotente e não depende de o job existir: o aviso do outro
    /// lado pode chegar antes de a transferência ter sido registrada aqui.
    #[tokio::test]
    async fn cancelling_marks_the_job_and_holds_for_a_job_not_yet_known() {
        let transfers = TransferState::default();
        transfers
            .start_job(
                "job-1",
                "f.gguf",
                TRANSFER_RECEIVING,
                "ksia",
                "/m/f.gguf",
                10,
            )
            .await;

        transfers.cancel("job-1", "macbook").await;
        assert!(transfers.is_cancelled("job-1").await);
        {
            let jobs = transfers.jobs.read().await;
            let job = jobs.get("job-1").unwrap();
            assert_eq!(job.state, TRANSFER_CANCELLED);
            assert!(job.detail.contains("macbook"), "detail: {}", job.detail);
        }

        // Duas vezes é igual a uma.
        transfers.cancel("job-1", "macbook").await;
        assert!(transfers.is_cancelled("job-1").await);

        // Um id que ainda não chegou aqui fica marcado assim mesmo, para o loop
        // que começar depois já nascer cancelado.
        transfers.cancel("job-desconhecido", "ksia").await;
        assert!(transfers.is_cancelled("job-desconhecido").await);
    }

    /// A regra é conjunção: assinatura no cabeçalho E extensão `.gguf`.
    #[test]
    fn only_real_gguf_files_pass_the_gate() {
        let dir = std::env::temp_dir().join(format!("pom-gguf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let ok = dir.join("fatia.gguf");
        std::fs::write(&ok, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&ok).is_ok());

        // Blob sem extensão não é uma transferência permitida.
        let blob = dir.join("sha256-abc123");
        std::fs::write(&blob, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&blob).is_err());

        // O nome mente sobre o conteúdo.
        let fake = dir.join("mentira.gguf");
        std::fs::write(&fake, b"NOTG____").unwrap();
        assert!(ensure_gguf(&fake).is_err());

        // Conteúdo certo, extensão errada: ainda é não.
        let wrong_ext = dir.join("modelo.txt");
        std::fs::write(&wrong_ext, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&wrong_ext).is_err());

        // Maiúsculas não importam.
        let upper = dir.join("FATIA.GGUF");
        std::fs::write(&upper, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&upper).is_ok());

        // Menor que a própria assinatura.
        let tiny = dir.join("curto.gguf");
        std::fs::write(&tiny, b"GG").unwrap();
        assert!(ensure_gguf(&tiny).is_err());

        assert!(ensure_gguf(&dir).is_err(), "diretório não é arquivo");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// O que o operador escolhe na própria tela ainda precisa ser um GGUF.
    #[test]
    fn the_operator_cannot_send_a_non_gguf() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-nogguf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("anotacoes.txt");
        std::fs::write(&f, b"nada a ver").unwrap();

        assert!(local_send_path(f.to_str().unwrap()).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_peer_cannot_ask_for_files_outside_the_models_dir() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/ok.gguf"), b"GGUF\x03\x00\x00\x00").unwrap();

        assert!(safe_model_path(&dir, "sub/ok.gguf").is_ok());
        assert!(safe_model_path(&dir, "../../../etc/passwd").is_err());
        assert!(safe_model_path(&dir, "/etc/passwd").is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_absolute_path_inside_the_models_dir_is_allowed() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-abs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("slice.gguf");
        std::fs::write(&f, b"GGUF\x03\x00\x00\x00").unwrap();

        let resolved = safe_model_path(&dir, f.to_str().unwrap()).unwrap();
        assert_eq!(resolved.file_name().unwrap(), "slice.gguf");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A path is never accepted from the transfer API, even when it points to
    /// a valid GGUF outside the configured catalog.
    #[test]
    fn a_path_cannot_be_used_as_a_transfer_filename() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-any-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("solto.gguf");
        std::fs::write(&f, b"GGUF\x03\x00\x00\x00").unwrap();

        assert!(valid_transfer_filename(f.to_str().unwrap()).is_err());
        assert!(valid_transfer_filename("../solto.gguf").is_err());
        assert!(valid_transfer_filename("solto.txt").is_err());
        assert_eq!(valid_transfer_filename("solto.gguf").unwrap(), "solto.gguf");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Choosing a file is one thing; a directory is not a transfer.
    #[test]
    fn a_directory_is_not_sendable() {
        let dir = std::env::temp_dir().join(format!("pom-xfer-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        assert!(local_send_path(dir.to_str().unwrap()).is_err());
        assert!(local_send_path("/nao/existe/em/lugar/nenhum.gguf").is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Quanto tempo a oferta leva para sair desta máquina.
///
/// O defeito que estes testes guardam: a oferta era postada **depois** do MD5
/// do arquivo inteiro, então o sino do outro lado só acendia quando a leitura
/// terminava. Medido no cartão SD do captain, `md5` de uma fatia de 4,2 GB
/// custa 3 min 23 s (~20 MB/s) - exatamente o "às vezes 1 ou 2 minutos" do
/// relato, com o cache de digest por (caminho, mtime, tamanho) mascarando o
/// atraso no segundo envio do mesmo arquivo.
#[cfg(test)]
mod offer_latency_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Um GGUF de verdade, grande o bastante para que lê-lo inteiro apareça no
    /// relógio se alguém voltar a ler.
    fn write_gguf(path: &Path, mib: usize) {
        let mut body = Vec::with_capacity(mib * 1024 * 1024);
        body.extend_from_slice(GGUF_MAGIC);
        body.resize(mib * 1024 * 1024, 0x5a);
        std::fs::write(path, body).unwrap();
    }

    /// O peer que recebe a oferta, reduzido ao que importa: ele anota o corpo
    /// que chegou e responde. É um servidor HTTP de verdade, na loopback.
    async fn fake_peer() -> (u16, Arc<RwLock<Vec<serde_json::Value>>>) {
        let seen: Arc<RwLock<Vec<serde_json::Value>>> = Arc::new(RwLock::new(Vec::new()));
        let captured = seen.clone();
        let app = Router::new().route(
            "/api/transfer/offer",
            post(move |Json(body): Json<serde_json::Value>| {
                let captured = captured.clone();
                async move {
                    captured.write().await.push(body);
                    Json(json!({ "accepted_for_review": true }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, seen)
    }

    /// Um nó remetente completo, com um peer já na tabela apontando para
    /// `peer_port`. Nada de UDP sai daqui: a tabela é semeada à mão.
    async fn sender_state(peer_port: u16) -> GatewayState {
        crate::gateway::testkit::state_with(
            "eu",
            vec![crate::gateway::testkit::peer("peer-b", "peer-b", peer_port)],
        )
        .await
    }

    /// A oferta é um convite, não a transferência: ela só precisa dizer o que é
    /// o arquivo e onde buscá-lo. O MD5 quem confere é o receptor, depois de
    /// Quem disca decide o esquema, não a oferta.
    ///
    /// A oferta é escrita pelo remetente e um nó de versão anterior grava
    /// `http://` fixo. Com o cluster em TLS isso rendia 403 em texto puro, e o
    /// `.json()` em cima dessa frase virava "manifesto inválido: error decoding
    /// response body" - erro que não aponta para nada (18/08).
    #[test]
    fn the_dialled_scheme_comes_from_local_policy_not_from_the_offer() {
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();

        crate::peer_client::set_tls_ativo(true);
        assert_eq!(
            normaliza_esquema("http://ksia:8080"),
            "https://ksia:8080",
            "oferta antiga em http tem que ser promovida"
        );
        assert_eq!(normaliza_esquema("https://ksia:8080"), "https://ksia:8080");
        assert_eq!(
            normaliza_esquema("http://[2804:d55::1]:8080"),
            "https://[2804:d55::1]:8080",
            "IPv6 entre colchetes sobrevive"
        );

        crate::peer_client::set_tls_ativo(false);
        assert_eq!(
            normaliza_esquema("https://ksia:8080"),
            "http://ksia:8080",
            "cluster em claro rebaixa a oferta"
        );

        crate::peer_client::set_tls_ativo(false);
    }

    /// aceitar, com o número que ele mesmo pede em `/api/transfer/manifest` -
    /// hashear aqui era trabalho que ninguém consumia e que segurava o sino do
    /// outro lado pelo tempo de ler o arquivo inteiro.
    #[tokio::test]
    async fn an_offer_leaves_without_reading_the_whole_file() {
        // Serializa com os testes que mexem na política de TLS, e fixa "em
        // claro": este teste disca um peer falso em HTTP puro. Sem isto, um
        // teste paralelo que liga a política fazia o cliente tentar TLS contra
        // o servidor falso, a oferta não era entregue, o `let _ =` engolia o
        // erro e a asserção estourava em `offers[0]` com a lista vazia - a
        // intermitência que perseguiu a suíte o dia 18/08 inteiro.
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        crate::peer_client::set_tls_ativo(false);

        let dir = std::env::temp_dir().join(format!("pom-offer-fast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fatia_0_15.gguf");
        write_gguf(&file, 24);

        let (port, seen) = fake_peer().await;
        let state = sender_state(port).await;
        state.auth.write().await.shard_dir = Some(dir.to_string_lossy().into());

        let started = Instant::now();
        let resp = send_to_peer(
            crate::auth::ConnectionAuth,
            State(state.clone()),
            Json(SendRequest {
                to_node_id: "peer-b".into(),
                filename: "fatia_0_15.gguf".into(),
            }),
        )
        .await;
        let elapsed = started.elapsed();
        assert_eq!(resp.status(), StatusCode::OK, "a oferta tem que ser aceita");

        let offers = seen.read().await;
        assert_eq!(offers.len(), 1, "o peer tem que ter recebido a oferta");
        let offer = &offers[0];
        assert_eq!(offer["name"], "fatia_0_15.gguf");
        assert_eq!(offer["size_bytes"], 24 * 1024 * 1024);
        assert_eq!(offer["start_layer"], 0);
        assert_eq!(offer["end_layer"], 15);

        // A prova de que nada foi lido: o cache de digest desta máquina segue
        // vazio. Ele é populado por `file_digest` e por mais nada, então uma
        // entrada aqui significa que alguém leu o arquivo inteiro antes de
        // bater na porta do vizinho.
        assert!(
            state.transfers.digests.read().await.is_empty(),
            "a oferta não pode hashear o arquivo: é o que fazia o sino do outro lado demorar"
        );

        // Cinturão e suspensórios: com o hash fora do caminho, o convite sai em
        // milissegundos mesmo aqui, onde o arquivo é pequeno.
        assert!(
            elapsed < Duration::from_secs(2),
            "oferta demorou {elapsed:?}; ela não pode depender do tamanho do arquivo"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// O tamanho anunciado tem que ser o real: ele é o que o outro lado mostra
    /// na notificação antes de decidir aceitar gigabytes.
    #[tokio::test]
    async fn the_offer_carries_the_real_size_without_a_digest() {
        // Serializa com os testes que mexem na política de TLS, e fixa "em
        // claro": este teste disca um peer falso em HTTP puro. Sem isto, um
        // teste paralelo que liga a política fazia o cliente tentar TLS contra
        // o servidor falso, a oferta não era entregue, o `let _ =` engolia o
        // erro e a asserção estourava em `offers[0]` com a lista vazia - a
        // intermitência que perseguiu a suíte o dia 18/08 inteiro.
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        crate::peer_client::set_tls_ativo(false);

        let dir = std::env::temp_dir().join(format!("pom-offer-size-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("solto.gguf");
        write_gguf(&file, 3);

        let (port, seen) = fake_peer().await;
        let state = sender_state(port).await;
        state.auth.write().await.shard_dir = Some(dir.to_string_lossy().into());

        let _ = send_to_peer(
            crate::auth::ConnectionAuth,
            State(state.clone()),
            Json(SendRequest {
                to_node_id: "peer-b".into(),
                filename: "solto.gguf".into(),
            }),
        )
        .await;

        let offers = seen.read().await;
        assert_eq!(offers[0]["size_bytes"], 3 * 1024 * 1024);
        // Compatibilidade: um peer antigo exige o campo, então ele continua
        // viajando - vazio, que é como se diz "ainda não sei".
        assert_eq!(offers[0]["md5"], "");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Aceitar não pode ficar preso ao outro lado. O manifesto é onde o MD5
    /// finalmente é calculado, e num volume lento isso são minutos: se o clique
    /// esperasse por ele, o operador teria trocado uma espera por outra.
    #[tokio::test]
    async fn accepting_returns_before_the_other_side_answers() {
        // Serializa com os testes que mexem na política de TLS, e fixa "em
        // claro": este teste disca um peer falso em HTTP puro. Sem isto, um
        // teste paralelo que liga a política fazia o cliente tentar TLS contra
        // o servidor falso, a oferta não era entregue, o `let _ =` engolia o
        // erro e a asserção estourava em `offers[0]` com a lista vazia - a
        // intermitência que perseguiu a suíte o dia 18/08 inteiro.
        let _trava = crate::peer_client::POLITICA_GLOBAL.lock();
        crate::peer_client::set_tls_ativo(false);

        let slow_hits = Arc::new(AtomicU64::new(0));
        let hits = slow_hits.clone();
        let app = Router::new().route(
            "/api/transfer/manifest",
            get(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    // O outro lado está lendo um arquivo de gigabytes.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Json(json!({ "size_bytes": 1, "md5": "x" }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let state = sender_state(port).await;
        let dir = std::env::temp_dir().join(format!("pom-accept-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        state.auth.write().await.shard_dir = Some(dir.to_string_lossy().into());

        let started = Instant::now();
        let job_id = spawn_download(
            state.clone(),
            "peer-b".into(),
            format!("http://127.0.0.1:{port}"),
            "/qualquer/fatia.gguf".into(),
            None,
            "fatia.gguf".into(),
            "peer-b".into(),
        )
        .await
        .expect("aceitar registra a transferência na hora");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "aceitar esperou o manifesto: {:?}",
            started.elapsed()
        );

        // E o trabalho existe de verdade: o job está na lista, com o outro lado
        // já sendo consultado em segundo plano.
        let jobs = state.transfers.jobs.read().await;
        assert!(jobs.contains_key(&job_id), "o job tem que aparecer na tela");
        drop(jobs);
        for _ in 0..50 {
            if slow_hits.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(
            slow_hits.load(Ordering::SeqCst),
            1,
            "o manifesto tem que ter sido pedido em segundo plano"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod ipv6_tests {
    use super::*;

    /// An offer carries the address the receiver will fetch from. For an IPv6
    /// peer it used to carry `127.0.0.1`, pointing the receiver at itself.
    #[test]
    fn the_host_of_a_url_survives_ipv6_brackets() {
        assert_eq!(url_host("http://192.168.100.138:8090"), "192.168.100.138");
        assert_eq!(
            url_host("http://[2804:d55:5579:1000:89f3:3f94:7c07:e183]:8090"),
            "2804:d55:5579:1000:89f3:3f94:7c07:e183"
        );
        assert_eq!(url_host("http://[::1]:8090"), "::1");
    }

    /// Whatever the routing table answers has to be usable in a URL, which for
    /// IPv6 means brackets.
    #[test]
    fn an_ipv6_source_address_comes_back_bracketed() {
        let out = local_ip_for("http://[::1]:8090");
        assert!(
            out.starts_with('[') && out.ends_with(']'),
            "esperado bracketed, veio {out}"
        );
        assert!(
            format!("http://{out}:8090").parse::<reqwest::Url>().is_ok(),
            "o endereco tem que formar uma URL valida: {out}"
        );
    }
}
