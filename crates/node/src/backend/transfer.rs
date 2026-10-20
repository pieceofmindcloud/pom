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
//! p2p bitswap behaviour: that is the path already proven to carry traffic
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
//! Where the file lands depends on what it is:
//!
//! * a **part** of a slicing goes to
//!   `<slice folder>/<model>-<short hash of the original>/`, with the name being
//!   its own sha256 and the manifest named like the folder, written in the same
//!   folder before the bytes. The id comes from the
//!   hash of the original model, and new alternatives are merged into the manifest;
//!   it is the requested hash that checks what arrived - not a digest that the
//!   sender declared;
//! * any other file - a whole model, which belongs to no manifest -
//!   lands in `<slice folder>/recebidos/`, and only takes its final name
//!   when the MD5 matches.
//!
//! **The `.pom` is mandatory for a part, in both directions.** The sender
//! refuses to offer an orphan slice; the server refuses to deliver it; the receiver
//! refuses to accept it. A part with no manifest is a file nobody knows which
//! model it is from or which layers it carries, and which the screen would show as a
//! whole model - failing and saying what is missing is better than spreading the problem.
//!
//! The slice folder is the one configured in `pom.json`, `models/` until someone changes it.

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

/// Directions of the same job. The id is the same on both machines; what changes is which
/// side of it each one is on.
const TRANSFER_SENDING: &str = "sending";
const TRANSFER_RECEIVING: &str = "receiving";

/// A state one does not leave. The screen uses the same rule (`transferStatus.ts`).
fn is_finished(state: &str) -> bool {
    matches!(state, TRANSFER_DONE | TRANSFER_FAILED | TRANSFER_CANCELLED)
}

pub fn routes() -> Router<GatewayState> {
    Router::new()
        // Serving side
        .route("/api/transfer/manifest", get(manifest))
        .route("/api/transfer/pom", get(serve_pom))
        .route("/api/transfer/download", get(download))
        // Receiving side
        .route("/api/transfer/pull", post(start_pull))
        .route("/api/transfer/offer", post(receive_offer))
        .route("/api/transfer/offers", get(list_offers))
        .route("/api/transfer/offers/:id/accept", post(accept_offer))
        .route("/api/transfer/offers/:id/reject", post(reject_offer))
        // The verdict goes back to whoever offered. See `receive_answer`.
        .route("/api/transfer/offers/answer", post(receive_answer))
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
    /// `sending` or `receiving`. The same id exists on both machines, with
    /// opposite directions - this is how the two sides talk about the same
    /// transfer without inventing a second vocabulary.
    pub direction: &'static str,
    /// The other machine. Replaces `from_host`, which lied about the send direction.
    pub peer_host: String,
    /// Local file involved. Deletion needs to know what is in transit.
    pub path: String,
    pub state: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub bytes_per_sec: f64,
    pub eta_secs: Option<u64>,
    pub detail: String,
    /// Which model this transfer is from, and which layers it carries.
    ///
    /// **A part's name is its sha256**, and a hash does not say which model it is
    /// from nor which piece of it it carries. What knows both is the `.pom`, which already
    /// travels with the offer and is written before the first byte; without these fields
    /// the notification only had the hash to show. The `name` stays here as
    /// secondary identification - it is what names the file on disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_layer: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_layer: Option<u32>,
    /// When the job entered a terminal state, in RFC 3339.
    ///
    /// The bell lists transfers in progress; without this mark it has no way to
    /// show the one that **just** finished, and the outcome - completed, failed,
    /// refused - vanishes from the screen the instant it happens. That was the gap:
    /// the operator only found the result again on the Transfers screen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
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
    /// Empty, and on purpose: **the offer carries no digest**.
    ///
    /// The receiver is the one who checks the file, with the MD5 it asks for itself at
    /// `/api/transfer/manifest` after accepting - this field was never read
    /// by anyone. Computing it here cost a read of the whole file
    /// *before* the offer went out, and that is what held back the notification on the
    /// other side for minutes. The field stays in the struct because a peer of a
    /// previous version requires it in the request body.
    pub md5: String,
    /// The manifest of the slicing this file belongs to, when it is a
    /// part.
    ///
    /// Travels **together with the offer**, and not after: without it the other side receives
    /// a `<sha256>.gguf` that does not know which model it is from nor which layers it carries, and
    /// ends up storing it as if it were a whole model. It is a few kB of JSON
    /// against gigabytes of weights - sending it along is free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pom: Option<crate::pom::PomManifest>,
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
    /// Cancelled ids. Kept even for a job that does not exist here yet: the
    /// notice from the other side can arrive before the download starts, and without this
    /// the transfer would start already cancelled and nobody would stop it.
    cancelled: RwLock<HashSet<String>>,
    /// What this machine offered and has not had an answer to yet: `grant` -> offer.
    ///
    /// Without this the sender has no way of knowing which file the other side
    /// is talking about when the verdict arrives - the `grant` is the only id that
    /// both ends share before a job exists. See [`receive_answer`].
    offered: RwLock<HashMap<String, OfferedFile>>,
}

/// How a slice presents itself on the screen: the model and the layer range.
///
/// It comes from the `.pom`, and only from there. The file name is the sha256 and claims nothing
/// about any layer - deducing the range from the name was a real defect, and the
/// manifest is what knows.
#[derive(Debug, Clone)]
pub struct SliceLabel {
    pub model_label: String,
    pub start_layer: u32,
    pub end_layer: u32,
}

/// The descriptor of the part `sha256` inside a manifest, if it describes it.
pub fn slice_label(m: &crate::pom::PomManifest, sha256: &str) -> Option<SliceLabel> {
    m.parts
        .iter()
        .chain(m.tp_parts.iter())
        .find(|p| p.sha256 == sha256)
        .map(|p| SliceLabel {
            model_label: m.model.label.clone(),
            start_layer: p.start_layer,
            end_layer: p.end_layer,
        })
}

/// The descriptor of a file on disk, when it is a part of a slicing.
///
/// What identifies the part is the **path**: the `.pom` lives next to it, and the
/// name under which someone is sending it does not decide which round it belongs to.
fn slice_label_for_path(path: &Path) -> Option<SliceLabel> {
    let sha = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| crate::pom::is_sha256_hex(s))?;
    crate::pom::load_dir(path.parent()?)
        .iter()
        .find_map(|m| slice_label(m, sha))
}

/// An offer that left here, kept to give a name to the verdict that comes back.
#[derive(Debug, Clone)]
pub struct OfferedFile {
    pub to_node_id: String,
    /// Readable name of the destination, so the notification does not talk about a node hash.
    pub to_host: String,
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    /// Model and layers, so the verdict does not come back talking about a hash.
    pub slice: Option<SliceLabel>,
}

impl TransferState {
    /// Registers a transfer in progress. Called by both sides, with the
    /// same id.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_job(
        &self,
        id: &str,
        name: &str,
        direction: &'static str,
        peer_host: &str,
        path: &str,
        bytes_total: u64,
        slice: Option<&SliceLabel>,
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
                model_label: slice.map(|f| f.model_label.clone()),
                start_layer: slice.map(|f| f.start_layer),
                end_layer: slice.map(|f| f.end_layer),
                finished_at: None,
            },
        );
    }

    /// Closes a job in a terminal state, stamping the time.
    ///
    /// Every outcome goes through here, and for one reason: a terminal state
    /// is never rewritten by another. "Cancelled by ksia" is someone's decision and
    /// cannot become an anonymous "failed" when the loop notices that the stream
    /// died - there were two different places doing this check by hand, and the
    /// sending side did none.
    pub async fn finish(&self, id: &str, state: &str, detail: impl Into<String>) {
        let mut jobs = self.jobs.write().await;
        let Some(j) = jobs.get_mut(id) else { return };
        if is_finished(&j.state) {
            return;
        }
        j.state = state.to_string();
        j.detail = detail.into();
        j.finished_at = Some(chrono::Utc::now().to_rfc3339());
    }

    /// Progress, with the same arithmetic on both sides.
    ///
    /// `moved` is what moved in this attempt, which is not `done` when the
    /// transfer resumed from a `.part`: using `done` would inflate the speed
    /// with bytes that were already on disk.
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

    /// Marks the cancellation and labels the job with who asked for it.
    ///
    /// "Cancelled by ksia" and "failed" are different things to whoever looks at the
    /// screen: the first was someone's decision, the second is a reason to try
    /// again.
    pub async fn cancel(&self, id: &str, by: &str) {
        self.cancelled.write().await.insert(id.to_string());
        self.finish(id, TRANSFER_CANCELLED, format!("cancelled by {by}"))
            .await;
    }
}

pub type Transfers = Arc<TransferState>;

// ── Path safety ──────────────────────────────────────────────────────────────

/// The name a GGUF may have: extension `.gguf`, or none - Hugging Face
/// publishes files like that, and then only the signature says what it is. An
/// Ollama blob (`sha256-...`) does not count as an extensionless file
/// (`catalog::is_extensionless_name`).
fn has_gguf_name(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(crate::catalog::is_extensionless_name)
}

/// Signature of a GGUF file: the first 4 bytes.
const GGUF_MAGIC: &[u8; 4] = b"GGUF";

/// The only kind of file that may leave this machine or be deleted by it.
///
/// Both the content and the name must confirm it is a GGUF: extension `.gguf`, or
/// no extension (`has_gguf_name`). Any other extension is still refused.
pub(crate) fn ensure_gguf(path: &Path) -> Result<(), String> {
    use std::io::Read;

    if !path.is_file() {
        return Err(format!("not a file: {}", path.display()));
    }

    if !has_gguf_name(path) {
        return Err(format!(
            "only .gguf or extensionless files can be transferred: {}",
            path.display()
        ));
    }

    let mut head = [0u8; 4];
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("could not open {}: {e}", path.display()))?;
    file.read_exact(&mut head)
        .map_err(|_| format!("{} is too short to be a GGUF", path.display()))?;
    if &head != GGUF_MAGIC {
        return Err(format!(
            "{} is not a GGUF (signature {head:02x?})",
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
        return Err("the transfer accepts only a file name, without a path".to_string());
    }
    if !has_gguf_name(path) {
        return Err("only .gguf or extensionless files can be transferred".to_string());
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
        .map_err(|e| format!("models dir unreadable: {e}"))?;
    let candidate = Path::new(requested);
    let full = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let full = full
        .canonicalize()
        .map_err(|_| format!("file not found: {requested}"))?;
    if !full.starts_with(&root) {
        return Err("path outside the models directory".to_string());
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
    } else if let Some(sha) = q.sha.as_deref() {
        let short = &sha[..12.min(sha.len())];
        let found = find_part(&state.shard_dir().await, sha)
            .ok_or_else(|| format!("this machine does not have part {short}"))?;
        // A part is only servable together with the manifest that explains it. Serving
        // the bytes without having the `.pom` to go along delivers to the neighbor a
        // file it will not know how to name - and the other side would refuse
        // after moving gigabytes. The refusal belongs here, before them.
        let described = found.parent().is_some_and(|folder| {
            crate::pom::load_dir(folder).iter().any(|m| {
                m.parts
                    .iter()
                    .chain(m.tp_parts.iter())
                    .any(|p| p.sha256 == sha)
            })
        });
        if !described {
            return Err(format!(
                "part {short} is here but without a manifest; a slice is not served without the `.pom` that describes it"
            ));
        }
        found
    } else {
        let requested = q
            .path
            .as_deref()
            .ok_or_else(|| "provide ?grant=, ?sha= or ?path=".to_string())?;
        match safe_model_path(&state.shard_dir().await, requested) {
            Ok(path) => path,
            // A projector or MTP draft a deployment copies to the head or the
            // tail usually lives outside the slice folder. It is served only
            // when it is in this machine's own sidecar inventory - a path
            // anywhere else is refused as before.
            Err(error) => listed_sidecar(state, requested).await.ok_or(error)?,
        }
    };
    // Also on the grant path: it is the way a file outside
    // `models/` leaves, and it was the only door with no type check.
    ensure_gguf(&path)?;
    if let Ok(Some(count)) = crate::multipart::declared_split_count(&path) {
        return Err(format!(
            "this file belongs to a multipart GGUF with {count} parts; transfer the complete set or slice it with POM before distributing"
        ));
    }
    Ok(path)
}

/// `requested` when it is one of this machine's sidecars, compared resolved.
async fn listed_sidecar(state: &GatewayState, requested: &str) -> Option<PathBuf> {
    let roots = {
        let config = state.auth.read().await;
        let mut roots = vec![config.shard_dir_path()];
        roots.extend(config.model_dir_paths());
        roots
    };
    let wanted = std::fs::canonicalize(requested).ok()?;
    tokio::task::spawn_blocking(move || {
        crate::catalog::discover_sidecars(&roots)
            .into_iter()
            .find_map(|sidecar| {
                std::fs::canonicalize(&sidecar.path)
                    .ok()
                    .filter(|path| *path == wanted)
            })
    })
    .await
    .ok()
    .flatten()
}

/// Where the part with this hash is, if this machine has it.
///
/// Looks for `<sha256>.gguf` in each model folder under the slices directory.
/// The search deliberately does not use the model name: the same content may
/// be filed under different rounds or names on each machine, and what
/// identifies the part is the hash, not where it ended up.
///
/// The hash validation in `pom::part_path` is what keeps a `sha` coming from the
/// network from becoming `../`: without it this would be directory traversal served over
/// HTTP.
pub fn find_part(shard_dir: &Path, sha256: &str) -> Option<PathBuf> {
    if !crate::pom::is_sha256_hex(sha256) {
        return None;
    }
    for entry in std::fs::read_dir(shard_dir).ok()?.flatten() {
        let folder = entry.path();
        if !folder.is_dir() {
            continue;
        }
        if let Ok(p) = crate::pom::part_path(&folder, sha256) {
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
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
        .ok_or_else(|| "unknown or expired grant".to_string())
}

async fn file_digest(state: &GatewayState, path: &Path) -> Result<(u64, String), String> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("stat failed: {e}"))?;
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
    /// sha256 of a part. It is the locator that does not depend on any path:
    /// whoever asks does not need to know how this machine organized its disk, and what
    /// arrives either has this hash or is discarded. See [`find_part`].
    #[serde(default)]
    pub sha: Option<String>,
    /// Id minted by the receiver. Absent when the peer is of an old version:
    /// the transfer works the same, just without visibility on this side.
    #[serde(default)]
    pub job: Option<String>,
    /// `node_id` of the receiver. Resolved to a name by the local peer table -
    /// nothing describing the other side is written in the request.
    #[serde(default)]
    pub peer: Option<String>,
}

/// The manifest of the slicing that contains the requested part.
///
/// It exists for the **pull** path: whoever offers sends the `.pom` along with the
/// offer, but whoever pulls on their own (which is what deployment does when moving
/// slices) receives no offer at all. Without this the file arrives with a hash name and
/// nothing to explain it, and the destination's catalog shows it as a whole
/// model - which was the defect.
async fn serve_pom(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(q): Query<PathQuery>,
) -> Response {
    let Some(sha) = q.sha.as_deref().filter(|s| crate::pom::is_sha256_hex(s)) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "informe ?sha=<sha256>" })),
        )
            .into_response();
    };
    let shard_dir = state.shard_dir().await;
    let Some(path) = find_part(&shard_dir, sha) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "this machine does not have this part" })),
        )
            .into_response();
    };
    let manifesto = path.parent().and_then(|folder| {
        crate::pom::load_dir(folder).into_iter().find(|m| {
            m.parts
                .iter()
                .chain(m.tp_parts.iter())
                .any(|p| p.sha256 == sha)
        })
    });
    match manifesto {
        Some(m) => Json(m).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "the part exists but there is no manifest describing it" })),
        )
            .into_response(),
    }
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
        return (StatusCode::NOT_FOUND, "file not found").into_response();
    };
    let total = meta.len();

    // The direct download asks for "bytes=<start>-", to resume. The download
    // through the p2p tunnel asks for one closed piece at a time
    // ("bytes=<first>-<last>"), because the tunnel holds an answer in memory.
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let piece = range.and_then(crate::infer_tunnel::closed_range);
    let start = match piece {
        Some((first, _)) => first,
        None => range
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.split('-').next())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0),
    }
    .min(total);
    let remaining = match piece {
        Some((_, length)) => length.min(total - start),
        None => total - start,
    };

    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not open").into_response();
    };
    if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "seek failed").into_response();
    }

    // The sending side enters its own transfer list. It is the one that
    // transmits the bytes, so it already knows the progress: there is no report back
    // to get lost, and the number stays right with the other side's screen closed.
    // A download in pieces asks many times for the same job: only the first
    // piece opens it, and a job already closed here (cancelled on this side)
    // is not reopened by the next piece.
    let known = match (piece, q.job.as_deref()) {
        (Some(_), Some(job_id)) => state
            .transfers
            .jobs
            .read()
            .await
            .get(job_id)
            .map(|job| is_finished(&job.state)),
        _ => None,
    };
    if known == Some(true) {
        return (StatusCode::GONE, "transfer closed on this side").into_response();
    }
    if let Some(job_id) = q.job.as_deref().filter(|_| known.is_none()) {
        let peer_host = peer_label(&state, q.peer.as_deref()).await;
        // The `.pom` is next to the file: it is where the "qwen3:8b · L0-15"
        // the screen shows in place of the hash comes from.
        let slice = slice_label_for_path(&path);
        state
            .transfers
            .start_job(
                job_id,
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("slice.gguf"),
                TRANSFER_SENDING,
                &peer_host,
                &path.to_string_lossy(),
                total,
                slice.as_ref(),
            )
            .await;
    }

    use futures::StreamExt;
    let reader = tokio_util::io::ReaderStream::with_capacity(
        tokio::io::AsyncReadExt::take(file, remaining),
        CHUNK,
    )
    .boxed();
    let body = match q.job.clone() {
        Some(job_id) => {
            Body::from_stream(served_stream(state.clone(), job_id, reader, start, total))
        }
        None => Body::from_stream(reader),
    };

    let partial = start > 0 || piece.is_some();
    let status = if partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, remaining.to_string())
        .header(header::ACCEPT_RANGES, "bytes");
    if partial {
        resp = resp.header(
            header::CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                start,
                (start + remaining).saturating_sub(1),
                total
            ),
        );
    }
    resp.body(body).unwrap().into_response()
}

/// Readable name of the peer on the other side, from the `node_id` it
/// sent. The translation comes from the local table: the request never carries the ready-made name.
async fn peer_label(state: &GatewayState, node_id: Option<&str>) -> String {
    let Some(node_id) = node_id else {
        return "unknown".to_string();
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

/// Wraps the file stream to count what has gone out and to stop early
/// when the job is cancelled.
///
/// **This is where the sending side finds out it is done.** Without this close the sender's
/// job stayed `running` forever, with `bytes_done` frozen at the
/// last 500 ms report - on a fast LAN, zero - and the bell of the machine that
/// sent the file showed an eternal transfer that had already finished
/// on the other side.
///
/// The close is the **last byte**, not the end of the stream: when the body satisfies
/// the promised `Content-Length`, hyper stops pulling and drops the stream without
/// ever asking for the `None` that would mark the end. Counting the bytes is the only signal
/// that always arrives. Cancellation does not go through here: `cancel` already closed the job
/// with the name of whoever asked, and `finish` respects what is terminal.
fn served_stream(
    state: GatewayState,
    job_id: String,
    inner: futures::stream::BoxStream<'static, std::io::Result<axum::body::Bytes>>,
    already: u64,
    total: u64,
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
        move |(mut inner, state, job_id, mut done, already, started, mut last)| async move {
            if state.transfers.is_cancelled(&job_id).await {
                // Ending the body here cuts the transfer: the receiver sees the
                // stream end before the promised Content-Length.
                return None;
            }
            let chunk = inner.next().await?;
            if let Ok(ref b) = chunk {
                done += b.len() as u64;
                let finished = done >= total;
                if finished || last.elapsed() >= Duration::from_millis(500) {
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
                if finished {
                    state
                        .transfers
                        .finish(&job_id, TRANSFER_DONE, "sent in full")
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
    /// Optional: an updated sender does not compute a digest to offer, and an
    /// old one still sends its own. Neither is read - see
    /// [`TransferOffer::md5`].
    #[serde(default)]
    pub md5: String,
    /// The slicing manifest, when what is being offered is a
    /// part. Absent in an offer from an old peer or for a standalone file.
    #[serde(default)]
    pub pom: Option<crate::pom::PomManifest>,
    #[serde(default)]
    pub start_layer: Option<u32>,
    #[serde(default)]
    pub end_layer: Option<u32>,
}

/// Where an accepted offer ends up, and what has to exist beforehand.
///
/// With a manifest, the offer is a **part**: it lands in
/// `<shard_dir>/<model>/<sha>.gguf`, and the `.pom` is written next to it before
/// the first byte arrives. This was the defect: without the manifest, the file
/// fell loose at the root of the slice folder and the catalog here showed it as a
/// whole model with an illegible name.
///
/// Writing the manifest **first** is deliberate. It is the only thing that
/// explains the file, and a download interrupted midway leaves a `.part` that
/// nobody claims; an orphan manifest, on the other hand, is harmless - it describes
/// parts that the index simply does not find.
///
/// Without a manifest, the old path: a standalone file in `recebidos/`.
async fn offer_target(state: &GatewayState, offer: &TransferOffer) -> Result<OfferTarget, String> {
    let shard_dir = state.shard_dir().await;
    if let Some(m) = &offer.pom {
        let sha = std::path::Path::new(&offer.name)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| crate::pom::is_sha256_hex(s))
            .map(|s| s.to_string())
            // The name may come from an old peer; the manifest still says which
            // part it is, as long as it describes exactly one.
            .or_else(|| {
                m.parts
                    .iter()
                    .chain(m.tp_parts.iter())
                    .next()
                    .map(|p| p.sha256.clone())
            });

        if let Some(sha) = sha {
            if m.parts
                .iter()
                .chain(m.tp_parts.iter())
                .any(|p| p.sha256 == sha)
            {
                // The received manifest is only remote knowledge. Merge it
                // into the local manifest so as not to erase parts this peer already
                // had of the same original model.
                let folder = match crate::pom::merge_manifest(&shard_dir, m) {
                    Ok((folder, _)) => folder,
                    Err(e) => {
                        return Err(format!(
                            "could not merge the manifest of {}: {e:#}",
                            m.model.label
                        ));
                    }
                };
                info!(
                    "manifest of {} stored; the part goes to {}",
                    m.model.label,
                    folder.display()
                );
                return Ok(OfferTarget::Part {
                    sha256: sha,
                    model_dir: Some(folder),
                    model_hint: m.model.name.clone(),
                });
            }
        }
        return Err(format!(
            "the offer brought the manifest of {} but the file is not a part of it",
            m.model.label
        ));
    }

    // Without a manifest, the file is only acceptable if it is **not** a part. A
    // hash name with no `.pom` is an orphan slice, and accepting it would mean keeping a
    // file nobody knows how to name - the rule is the same on both sides.
    if std::path::Path::new(&offer.name)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(crate::pom::is_sha256_hex)
    {
        return Err(format!(
            "{} offered a slice without the `.pom` that describes it",
            offer.from_host
        ));
    }

    Ok(OfferTarget::Standalone {
        remote_path: offer.remote_path.clone(),
        grant: offer.grant.clone(),
        name: offer.name.clone(),
    })
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
        pom: req.pom,
        start_layer: req.start_layer,
        end_layer: req.end_layer,
    };
    info!(
        "📥 slice offer received: {} ({} bytes)",
        offer.name, offer.size_bytes
    );
    let id = offer.id.clone();

    // Development gate, off by default and read from *this* machine's
    // environment: the sender has no way to ask for it. See `dev_auto_accept_env`.
    if dev_auto_accept() {
        warn!(
            "⚙️  {} on — accepting {} without confirmation",
            dev_auto_accept_env(),
            offer.name
        );
        let target = match offer_target(&state, &offer).await {
            Ok(a) => a,
            Err(e) => {
                warn!("automatic accept of {} refused: {e}", offer.name);
                state.transfers.offers.write().await.push(offer);
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
            }
        };
        return match spawn_download(
            state.clone(),
            offer.from_node_id.clone(),
            offer.from_gateway.clone(),
            target,
            offer.from_host.clone(),
        )
        .await
        {
            Ok(job_id) => {
                notify_offerer(&state, &offer, true, Some(job_id.clone()));
                Json(json!({
                    "accepted_for_review": false,
                    "auto_accepted": true,
                    "offer_id": id,
                    "job_id": job_id,
                }))
                .into_response()
            }
            // The offer still goes on the shelf: an auto-accept that could not
            // start is a transfer the operator can still take by hand, and
            // swallowing it would lose the file with only a log line to say so.
            Err(e) => {
                warn!("automatic accept of {} failed: {e}", offer.name);
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

async fn list_offers(
    _auth: ConnectionAuth,
    headers: HeaderMap,
    State(state): State<GatewayState>,
) -> Response {
    let slice_offers = state.transfers.offers.read().await.clone();
    let mut offers = slice_offers
        .into_iter()
        .map(|offer| serde_json::to_value(offer).expect("slice offer is serializable"))
        .collect::<Vec<_>>();
    if let Some(offer) = state.admin_trust.pending_transfer() {
        if offer.successor == state.authority_principal {
            let peers = state.network.alive_peers().await;
            let from_host = peers
                .iter()
                .find(|peer| peer.node_id == offer.issuer.node_id)
                .map(|peer| peer.hostname.clone())
                .filter(|hostname| !hostname.is_empty())
                .unwrap_or_else(|| offer.issuer.node_id.clone());
            let target_host = state
                .auth
                .read()
                .await
                .node_name
                .clone()
                .unwrap_or_else(|| state.network.node_id.clone());
            offers.push(json!({
                "kind": "principal_transfer",
                "id": offer.offer_id,
                "from_node_id": offer.issuer.node_id,
                "from_host": from_host,
                "target_node_id": offer.successor.node_id,
                "target_host": target_host,
                "status": "pending",
                "issued_at": offer.command_id,
            }));
        }
    }
    crate::webui::conditional_json(&headers, &json!({ "offers": offers }))
}

async fn sysmon_disk_free_mb() -> u64 {
    crate::sysmon::sample().await.disk.free_mb
}

/// The verdict from the other side on an offer that left here.
///
/// Knocking on the door and never knowing whether they opened was half of the defect: the operator
/// sent a slice and their screen said nothing more - neither "accepted" nor
/// "refused". The receiver now answers, and the sender records the outcome
/// as a job, which is the vocabulary the bell already reads.
#[derive(Debug, Deserialize)]
pub struct AnswerRequest {
    /// The grant that traveled with the offer. It is the only id that both ends
    /// share before a job exists.
    pub grant: String,
    /// Who is answering. Checked against the registered destination: a verdict
    /// that does not come from whoever received the offer describes another conversation.
    #[serde(default)]
    pub from_node_id: String,
    pub accepted: bool,
    /// The id the receiver minted for the download, when it accepted. Registering the
    /// same id on both sides is what makes the sender's job the
    /// same transfer, and not a second line in the list.
    #[serde(default)]
    pub job_id: Option<String>,
    /// Why it refused, when there is a reason beyond the will of whoever decided.
    #[serde(default)]
    pub reason: Option<String>,
}

async fn receive_answer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<AnswerRequest>,
) -> Response {
    // The offer leaves the registry on any verdict: it has been answered.
    let offer = {
        let mut record = state.transfers.offered.write().await;
        // A `grant` that this machine did not offer describes nothing it
        // can show. It is not an error on the other side - it may be a restart here.
        let Some(o) = record.get(&req.grant) else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "unknown offer" })),
            )
                .into_response();
        };
        if !req.from_node_id.is_empty() && req.from_node_id != o.to_node_id {
            warn!(
                "verdict about {} came from {} and not from {}",
                o.name, req.from_node_id, o.to_node_id
            );
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "this offer was not made to this node" })),
            )
                .into_response();
        }
        record.remove(&req.grant).expect("was just read")
    };

    if req.accepted {
        // The accept arrives **before** the first byte, and the distance between
        // the two is the time for the receiver to ask for the digest - minutes on a
        // slow volume. Registering the job already here is what takes the sender's screen out of
        // silence in that interval; when the bytes start, `download`
        // rewrites this same id with the real size.
        let job_id = req
            .job_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        info!("👍 {} accepted {}", offer.to_host, offer.name);
        state
            .transfers
            .start_job(
                &job_id,
                &offer.name,
                TRANSFER_SENDING,
                &offer.to_host,
                &offer.path,
                offer.size_bytes,
                offer.slice.as_ref(),
            )
            .await;
        set_detail(&state, &job_id, "accepted; waiting for the send to start").await;
        return Json(json!({ "noted": true, "job_id": job_id })).into_response();
    }

    // A refusal is not a failure: it was someone's decision, and the screen has to say
    // whose. `cancelled` is the state that already carries that meaning.
    let job_id = req
        .job_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    info!("👎 {} refused {}", offer.to_host, offer.name);
    state
        .transfers
        .start_job(
            &job_id,
            &offer.name,
            TRANSFER_SENDING,
            &offer.to_host,
            &offer.path,
            offer.size_bytes,
            offer.slice.as_ref(),
        )
        .await;
    let reason = match req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(r) => format!("refused by {}: {r}", offer.to_host),
        None => format!("refused by {}", offer.to_host),
    };
    state
        .transfers
        .finish(&job_id, TRANSFER_CANCELLED, reason)
        .await;
    Json(json!({ "noted": true, "job_id": job_id })).into_response()
}

/// Tells whoever offered what was decided here.
///
/// Best-effort and in the background: the click of whoever decided cannot wait for the
/// other machine to answer, and a sender that went offline is no reason for the
/// refusal to fail on the screen of whoever refused.
fn notify_offerer(
    state: &GatewayState,
    offer: &TransferOffer,
    accepted: bool,
    job_id: Option<String>,
) {
    let Some(grant) = offer.grant.clone() else {
        // An offer from a peer older than `grant`: there is no shared id to
        // name the verdict, and inventing one would only produce noise on the other side.
        return;
    };
    let state = state.clone();
    let from_node_id = offer.from_node_id.clone();
    let base_url = normalize_scheme(&offer.from_gateway);
    let from_host = offer.from_host.clone();
    tokio::spawn(async move {
        let client = match state.peer_http.for_peer(&from_node_id) {
            Ok(c) => c,
            Err(e) => {
                warn!("no client to tell {from_host} the verdict: {e}");
                return;
            }
        };
        let body = json!({
            "grant": grant,
            "from_node_id": state.network.node_id,
            "accepted": accepted,
            "job_id": job_id,
        });
        let request = state
            .control_json_request(
                client
                    .post(format!("{base_url}/api/transfer/offers/answer"))
                    .timeout(Duration::from_secs(30)),
                "/api/transfer/offers/answer",
                &body,
            )
            .await;
        match request {
            Ok(request) => match request.send().await {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => warn!("{from_host} answered {} to the verdict", r.status()),
                Err(e) => warn!("could not tell {from_host} the verdict: {e}"),
            },
            Err(e) => warn!("could not sign the verdict for {from_host}: {e}"),
        }
    });
}

async fn reject_offer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if state
        .admin_trust
        .pending_transfer()
        .is_some_and(|offer| offer.offer_id == id && offer.successor == state.authority_principal)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "the principal transfer only ends with a signed acceptance or with a new epoch"
            })),
        )
            .into_response();
    }
    let refused = {
        let mut offers = state.transfers.offers.write().await;
        offers
            .iter()
            .position(|o| o.id == id)
            .map(|i| offers.remove(i))
    };
    if let Some(offer) = &refused {
        notify_offerer(&state, offer, false, None);
    }
    Json(json!({ "rejected": true })).into_response()
}

async fn accept_offer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if state
        .admin_trust
        .pending_transfer()
        .is_some_and(|offer| offer.offer_id == id && offer.successor == state.authority_principal)
    {
        return crate::admin::accept_authority_transfer(&state, &id).await;
    }
    let offer = {
        let mut offers = state.transfers.offers.write().await;
        match offers.iter().position(|o| o.id == id) {
            Some(i) => offers.remove(i),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "offer does not exist" })),
                )
                    .into_response()
            }
        }
    };

    let target = match offer_target(&state, &offer).await {
        Ok(a) => a,
        Err(e) => {
            // The offer could not even start: whoever offered needs to know, or
            // is left waiting for a download that will never happen.
            notify_offerer(&state, &offer, false, None);
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    };
    match spawn_download(
        state.clone(),
        offer.from_node_id.clone(),
        offer.from_gateway.clone(),
        target,
        offer.from_host.clone(),
    )
    .await
    {
        Ok(job_id) => {
            notify_offerer(&state, &offer, true, Some(job_id.clone()));
            Json(json!({ "started": true, "job_id": job_id })).into_response()
        }
        Err(e) => {
            notify_offerer(&state, &offer, false, None);
            (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response()
        }
    }
}

// ── Pull direction ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PullRequest {
    /// Hash of the part wanted. When present, it is what locates the file on the
    /// other side, and `remote_path` stops mattering.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Model name, to know which folder the part will land in here. Only read
    /// together with `sha256`.
    #[serde(default)]
    pub model: Option<String>,
    /// Peer holding the file.
    pub node_id: String,
    /// Only for the request by path; a request by `sha256` has nothing to
    /// say here, because it does not know (nor need to know) how the other side
    /// organized its disk.
    #[serde(default)]
    pub remote_path: String,
    #[serde(default)]
    pub name: String,
}

async fn start_pull(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<PullRequest>,
) -> Response {
    // The name validation applies to the request **by path**, in which `name` is
    // what becomes a file here. A request by `sha256` sends no name: its
    // name is derived from the hash (`<sha256>.gguf`) and checked inside
    // `spawn_download`, along with the rest. Requiring `name` here refused every
    // content-addressed copy - including the ones deployment makes.
    if req.sha256.is_none() {
        if let Err(e) = valid_transfer_filename(&req.name) {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
        }
    }
    let peers = state.all_peers().await;
    let Some(peer) = peers.iter().find(|p| p.node_id == req.node_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown peer" })),
        )
            .into_response();
    };
    // The address that already answered first, not the first announced: the
    // download is direct HTTP and has no other path.
    let Some(base) = state.gateway_base(peer) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer does not announce a gateway port" })),
        )
            .into_response();
    };
    let host = if peer.hostname.is_empty() {
        peer.node_id[..12].to_string()
    } else {
        peer.hostname.clone()
    };

    // Two ways to ask, and the difference is who guarantees what. With `sha256` the
    // machine asks for a **content**: the peer looks for whoever has that hash, the
    // file goes straight into the model folder and only gets its name after being
    // checked. Without it, it is the old path - a standalone file, through the path
    // the other side exposes, with the digest it declares.
    let target = match req.sha256.as_deref() {
        Some(sha) if crate::pom::is_sha256_hex(sha) => {
            let model = req.model.as_deref().unwrap_or("parts");
            OfferTarget::Part {
                sha256: sha.to_string(),
                // The folder depends on the round id, which comes in the manifest the
                // download will fetch from the origin.
                model_dir: None,
                model_hint: model.to_string(),
            }
        }
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid sha256" })),
            )
                .into_response();
        }
        None => OfferTarget::Standalone {
            remote_path: req.remote_path,
            grant: None,
            name: req.name,
        },
    };

    match spawn_download(state.clone(), peer.node_id.clone(), base, target, host).await {
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
    /// Machine that requested the cancellation, so the other side can say who it was.
    #[serde(default)]
    pub by: Option<String>,
    /// `true` when the call came from the peer. Avoids the echo: whoever was notified does not
    /// notify back.
    #[serde(default)]
    pub from_peer: bool,
}

/// Cancels a transfer.
///
/// The same route serves the local screen and the notice the other side sends -
/// one verb, with the same semantics on both ends.
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
    let by = req.by.unwrap_or_else(|| state.network.hostname());

    let peer_host = state
        .transfers
        .jobs
        .read()
        .await
        .get(&id)
        .map(|j| j.peer_host.clone());

    state.transfers.cancel(&id, &by).await;
    info!("🛑 transfer {id} cancelled by {by}");

    // Notifying is what separates "cancelled" from "network dropped" on the other side's screen.
    // If the notice does not arrive, the local cancellation still holds and the other
    // side falls onto the dead-connection path, which reports the real reason.
    if !req.from_peer {
        if let Some(host) = peer_host {
            notify_peer_cancel(&state, &id, &by, &host).await;
        }
    }

    Json(json!({ "cancelled": true })).into_response()
}

/// Passes the cancellation on to the peer on the other side of this transfer.
async fn notify_peer_cancel(state: &GatewayState, id: &str, by: &str, peer_host: &str) {
    let peers = state.all_peers().await;
    let Some(peer) = peers
        .iter()
        .find(|p| p.hostname == peer_host || p.node_id.starts_with(peer_host))
    else {
        warn!("could not tell {peer_host} about the cancellation of {id}: unknown peer");
        return;
    };
    let body = json!({ "by": by, "from_peer": true });
    let sent = state
        .control_post_json(
            peer,
            &format!("/api/transfer/jobs/{id}/cancel"),
            &body,
            Duration::from_secs(10),
        )
        .await;
    if let Err(e) = sent {
        warn!("cancellation notice did not reach {peer_host}: {e}");
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
            .map_err(|e| format!("failed to query the local catalog: {e}"))?;
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
        [] => Err(format!("file not found in the local catalog: {filename}")),
        [path] => local_send_path(path.to_string_lossy().as_ref()),
        _ => Err(format!(
            "there is more than one file named {filename} in the local catalog; rename one of them before transferring"
        )),
    }
}

/// Validate a path already resolved by the local catalog before granting it.
fn local_send_path(requested: &str) -> Result<PathBuf, String> {
    let full = Path::new(requested)
        .canonicalize()
        .map_err(|_| format!("file not found: {requested}"))?;
    if !full.is_file() {
        return Err(format!("not a file: {requested}"));
    }
    ensure_gguf(&full)?;
    Ok(full)
}

/// The manifest of the slicing this file belongs to, if it is a part.
///
/// Looks up by path: a part lives in `<shard_dir>/<model>/<sha>.gguf`, and
/// the `.pom` files that describe it are in the same folder. A file outside the slice
/// folder, or inside it but not listed in any manifest, is not a part of
/// anything and goes alone, as it always did.
async fn manifest_for(state: &GatewayState, path: &Path) -> Option<crate::pom::PomManifest> {
    let sha = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| crate::pom::is_sha256_hex(s))?;
    let folder = path.parent()?;
    // Only a manifest that is in the file's own folder counts: a `.pom`
    // from elsewhere could describe a homonymous part of another round.
    let _ = state;
    crate::pom::load_dir(folder).into_iter().find(|m| {
        m.parts
            .iter()
            .chain(m.tp_parts.iter())
            .any(|p| p.sha256 == sha)
    })
}

/// Offer one of our slices to a peer.
///
/// This only knocks on the door: the peer stores the offer and its owner
/// decides. We never write to another machine's disk uninvited.
///
/// **Knocking on the door cannot cost the price of reading the file.** This path used to
/// compute the slice's MD5 before posting the offer, and since the digest is
/// a read of the whole file, the bell on the other side only lit up when it
/// finished: 3 min 23 s measured for a 4.2 GB slice on the SD card
/// (~20 MB/s). The cache by (path, mtime, size) masked the delay on the
/// second send of the same file, and that is where the "sometimes" in the report came from.
/// The number was not even used: whoever checks the download is the receiver, with the MD5
/// it asks for at `/api/transfer/manifest` after accepting. Here the
/// `stat` is enough - size and name, which is what the notification shows.
async fn send_to_peer(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SendRequest>,
) -> Response {
    let path = match catalog_send_path(&state, &req.filename).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    };

    if let Ok(Some(count)) = crate::multipart::declared_split_count(&path) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "the chosen file belongs to a multipart GGUF with {count} parts; keep the set together or slice it with POM before transferring"
                )
            })),
        )
            .into_response();
    }

    let size = match tokio::fs::metadata(&path).await {
        Ok(m) => m.len(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("stat failed: {e}") })),
            )
                .into_response()
        }
    };

    let peers = state.all_peers().await;
    let Some(peer) = peers.iter().find(|p| p.node_id == req.to_node_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown peer" })),
        )
            .into_response();
    };
    // The address that already answered this node comes first (`gateway_bases`), and
    // it is what the offer's return address comes from (`from_gateway`, via
    // `local_ip_for`). The first announced may be an unreachable IPv6: the
    // offer died there and the return address pointed to the wrong
    // interface.
    let Some(peer_base) = state.gateway_base(peer) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "peer does not announce a gateway port" })),
        )
            .into_response();
    };

    // The name comes from the request: `main` now lets the sender name it.
    let name = req.filename.trim().to_string();

    // The manifest goes along with the offer. It is what tells the other side which
    // model this part is from and which layers it carries - without it the file arrives
    // with a hash name and no owner, and the catalog there shows it as just any whole
    // model. What identifies the part is the **path** of the file
    // being sent, not the name the request chose for it.
    let pom = manifest_for(&state, &path).await;
    let sha = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    // **A part does not leave here without its `.pom`.** A file called
    // `<sha256>.gguf` is, by construction, part of a slicing; if the manifest
    // that describes it is not next to it, this machine has an orphan on disk
    // and sending it on would only spread the problem. Refusing here says where
    // to fix it; sending anyway delivers to the neighbor a file it
    // will not know how to name.
    if pom.is_none() && crate::pom::is_sha256_hex(sha) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "part {} has no manifest on this machine; a slice does not travel without the `.pom` that describes it",
                    &sha[..12]
                )
            })),
        )
            .into_response();
    }

    // The layer range used to be read here from the file name. Now it comes from the
    // manifest, which is what knows; with no manifest, nothing is invented.
    let slice = pom.as_ref().and_then(|m| slice_label(m, sha));
    let shard: Option<(u32, u32)> = slice.as_ref().map(|f| (f.start_layer, f.end_layer));

    // Where the peer should come and fetch it from — our own gateway, on the
    // address the peer already reaches us at.
    let my_host = state.network.hostname();

    // The peer fetches by grant id, never by path: that is what lets this file
    // sit outside `models/` without opening the disk to whoever holds the key.
    let grant = uuid::Uuid::new_v4().simple().to_string();
    state
        .transfers
        .grants
        .write()
        .await
        .insert(grant.clone(), path.clone());

    // Stored before the offer goes out: the verdict may come back before this
    // function returns, and without the record the sender would receive an "accepted" without
    // knowing which file. The destination's name comes from the local peer table,
    // which is what knows how to translate `node_id` into a machine name.
    state.transfers.offered.write().await.insert(
        grant.clone(),
        OfferedFile {
            to_node_id: req.to_node_id.clone(),
            to_host: if peer.hostname.is_empty() {
                peer.node_id[..12.min(peer.node_id.len())].to_string()
            } else {
                peer.hostname.clone()
            },
            name: name.clone(),
            path: path.to_string_lossy().into_owned(),
            size_bytes: size,
            slice: slice.clone(),
        },
    );

    let body = json!({
        "from_node_id": state.network.node_id,
        "from_host": my_host,
        // A pinned address beats one read off the routing table: with privacy
        // extensions the outbound source is a temporary IPv6 that rotates, and a
        // multi-gigabyte download outlives it.
        // The SCHEME in force, never a fixed "http". With the cluster on TLS, whoever
        // accepted dialed http and the gateway answered 403 in plain text
        // ("this cluster requires TLS between nodes; redo the call over https"); the
        // `.json()` on the other side choked on that phrase and the operator saw
        // "invalid manifest: error decoding response body" (18/08).
        "from_gateway": crate::peer_client::peer_base_url(
            &state
                .network
                .advertised_host()
                .unwrap_or_else(|| local_ip_for(&peer_base)),
            state.gateway_port,
            crate::peer_client::tls_enabled(),
        ),
        "grant": grant,
        "remote_path": path.to_string_lossy(),
        "name": name,
        "size_bytes": size,
        // Always empty. It travels only because a peer of a previous version requires the
        // field - see `TransferOffer::md5`.
        "md5": "",
        "pom": pom,
        "start_layer": shard.map(|(a, _)| a),
        "end_layer": shard.map(|(_, b)| b),
    });

    // All of the peer's addresses, the one that answered first, and the tunnel
    // last - the offer is a small control call.
    match state
        .control_post_json(peer, "/api/transfer/offer", &body, Duration::from_secs(30))
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!("📤 slice {name} offered to {}", peer.node_id);
            Json(json!({ "offered": true })).into_response()
        }
        Ok(r) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("peer answered {}", r.status()) })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("could not offer: {e:#}") })),
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

/// Manifest deadline.
///
/// The manifest is where the file's MD5 is finally computed, and a digest is
/// a read of the whole file: on the captain's SD card, 4.2 GB takes
/// 3 min 23 s. The earlier 120 s only passed because the sender had already
/// hashed when offering - which was exactly the defect. With the hash back
/// where it is needed, the deadline has to fit a slow volume; it runs
/// in the background, so waiting here holds nobody up.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Registers the transfer and does the rest - manifest, disk space and
/// download - in the background.
///
/// Querying the other side does **not** happen before returning: the manifest may
/// take minutes (see [`MANIFEST_TIMEOUT`]), and accepting an offer is a click,
/// not a wait. What the operator gains in return is the job already in the list of
/// transfers, saying which stage it is at; a manifest error becomes a `failed`
/// job with the reason, in the same place where they would already look.
/// `peer_id` is the node_id of whoever serves the bytes: it is what pins the certificate of
/// the other side when the cluster is on TLS. It comes from the offer (`from_node_id`) or
/// from the `PeerInfo` the operator chose.
/// The scheme that **this** machine will dial, whatever the offer carries.
///
/// The offer is written by the sender, and a node of a previous version writes a fixed
/// `http://`. Trusting that with the cluster on TLS yields a plain-text 403
/// ("this cluster requires TLS between nodes"), which `.json()` turns into
/// "invalid manifest: error decoding response body" - a message that points
/// nowhere. The local policy is the one that counts for dialing, so it is
/// what decides.
fn normalize_scheme(url: &str) -> String {
    let scheme = crate::peer_client::peer_scheme();
    match url.split_once("://") {
        Some((current, rest)) if current != scheme => format!("{scheme}://{rest}"),
        _ => url.to_string(),
    }
}

/// What is being fetched and where it will end up.
///
/// Two forms: a standalone file, located by path or grant and
/// dropped in `recebidos/`; or a **part**, located by hash and written
/// straight into the model folder with the hash as its name. The second skips
/// quarantine: the name is only given after the check, so a file with
/// a part name is, by construction, a checked part.
enum OfferTarget {
    Standalone {
        remote_path: String,
        grant: Option<String>,
        name: String,
    },
    Part {
        sha256: String,
        /// The folder, when whoever asked already had the manifest in hand (the case of
        /// an offer, which brings the `.pom` along).
        ///
        /// `None` when only the hash is known: then the folder comes from the manifest that the
        /// download fetches at the origin, because its name carries the round id
        /// and without the manifest there is no id.
        model_dir: Option<PathBuf>,
        /// Model name for the message, when there is no manifest.
        model_hint: String,
    },
}

async fn spawn_download(
    state: GatewayState,
    peer_id: String,
    base_url: String,
    target: OfferTarget,
    from_host: String,
) -> Result<String, String> {
    // The offer carries the scheme the SENDER wrote; the one dialing is this
    // machine, so the local policy is the one that counts. See `normalize_scheme`.
    let base_url = normalize_scheme(&base_url);
    let key = state.auth.read().await.connection_key.clone();

    // The id is born here and travels in the request: the other side registers the same id, and
    // that is what makes "cancel" apply to the whole transfer, on both
    // sides, instead of just the local half.
    let job_id = uuid::Uuid::new_v4().simple().to_string();

    // A grant, when we have one, is the only locator the serving side needs —
    // and the only one that reaches a file outside its `models/`.
    let (mut locator, dir, name, expected_sha): (
        Vec<(&str, String)>,
        PathBuf,
        String,
        Option<String>,
    ) = match &target {
        OfferTarget::Standalone {
            remote_path,
            grant,
            name,
        } => {
            let loc = match grant {
                Some(g) => vec![("grant", g.clone())],
                None => vec![("path", remote_path.clone())],
            };
            (
                loc,
                state.shard_dir().await.join("received"),
                name.clone(),
                None,
            )
        }
        OfferTarget::Part {
            sha256,
            model_dir,
            model_hint,
        } => {
            // **The manifest is mandatory.** A part's folder carries the round
            // id, and the id comes from the `.pom`; without it there is nowhere for the part to fit.
            //
            // Before here there was a plan B - dropping the part in `recebidos/` -
            // and it was wrong: a loose part, with nothing saying which
            // model it is from or which layers it carries, is a file nobody knows how to
            // use and which the screen shows as if it were a whole model. It is
            // better for the transfer to fail saying what was missing than to
            // finish well and leave junk that looks like a slice.
            //
            // Whoever arrived by offer already brings the manifest; whoever pulls by hash
            // asks the origin here, before any weight byte - a `.pom`
            // is a few kB, and finding out now costs far less than finding out
            // after moving gigabytes.
            let folder = match model_dir {
                Some(p) => p.clone(),
                None => {
                    let m = fetch_pom(&state, &peer_id, &base_url, &key, sha256)
                        .await
                        .ok_or_else(|| {
                            format!(
                                "{from_host} does not have the manifest of part {} ({model_hint}); a part does not travel without the `.pom` that describes it",
                                &sha256[..12]
                            )
                        })?;
                    if !m
                        .parts
                        .iter()
                        .chain(m.tp_parts.iter())
                        .any(|p| &p.sha256 == sha256)
                    {
                        return Err(format!(
                            "the manifest {from_host} sent does not describe part {}",
                            &sha256[..12]
                        ));
                    }
                    // Writing the manifest is part of the transfer, not an
                    // extra. The merge preserves parts that already existed on this
                    // peer and adds only the remote knowledge that is missing.
                    let p = crate::pom::merge_manifest(&state.shard_dir().await, &m)
                        .map(|(path, _)| path)
                        .map_err(|e| {
                            format!("could not merge the manifest of {}: {e:#}", m.model.label)
                        })?;
                    p
                }
            };
            (
                vec![("sha", sha256.clone())],
                folder,
                format!("{sha256}.gguf"),
                Some(sha256.clone()),
            )
        }
    };
    // The name comes from the other side and becomes a file here: the check has to
    // happen with it in hand. It used to be at the top of the function, when `name` was a
    // parameter; now it is computed above, according to the target. A part is
    // called `<sha256>.gguf`, which passes the same ruler as any other name.
    valid_transfer_filename(&name)?;

    // Optional on purpose: a peer that does not have this version yet ignores the
    // two and the transfer works as before, just without visibility on the sending side.
    locator.push(("job", job_id.clone()));
    locator.push(("peer", state.network.node_id.clone()));

    // The directory is the only thing that has to succeed right now: without it there is no
    // transfer at all to put in the list.
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    // **The `.pom` is already in place, and it is where the screen's name comes from.** Whoever
    // arrived by offer brought the manifest along and it was just merged
    // into the folder; whoever pulls by hash asked the origin for theirs just above - in both
    // cases before any weight byte. Reading from here is what swaps the
    // notification's `<sha256>.gguf` for "qwen3:8b · L0-15".
    let destination = dir.join(&name);
    let slice = slice_label_for_path(&destination);

    // `bytes_total` 0 until the manifest answers - the bar stays indeterminate, which
    // is the truth: the size is not known yet.
    state
        .transfers
        .start_job(
            &job_id,
            &name,
            TRANSFER_RECEIVING,
            &from_host,
            &destination.to_string_lossy(),
            0,
            slice.as_ref(),
        )
        .await;
    set_detail(&state, &job_id, "checking the file at the source").await;

    let task_state = state.clone();
    let task_job_id = job_id.clone();
    tokio::spawn(async move {
        let job_id = task_job_id;
        let dest = dir.join(&name);
        let part = dir.join(format!("{name}.part"));
        let expected_sha = expected_sha;
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
                expected_sha.as_deref(),
            )
            .await
        }
        .await;

        if let Err(e) = outcome {
            warn!("transfer of {name} failed: {e}");
            // A partial file must never look like a usable slice.
            let _ = tokio::fs::remove_file(&part).await;
            // `finish` respects the terminal state that is already there: `cancel`
            // labeled this job with who asked, and overwriting would swap someone's
            // decision for an anonymous "failed".
            task_state
                .transfers
                .finish(&job_id, TRANSFER_FAILED, e)
                .await;
        }
    });

    Ok(job_id)
}

/// Asks the other side for the manifest of the slicing a part belongs to.
async fn fetch_pom(
    state: &GatewayState,
    peer_id: &str,
    base_url: &str,
    _key: &str,
    sha256: &str,
) -> Option<crate::pom::PomManifest> {
    let client = state.peer_http.for_peer(peer_id).ok()?;
    let request = state
        .control_get_request(
            client
                .get(format!("{base_url}/api/transfer/pom?sha={sha256}"))
                .timeout(MANIFEST_TIMEOUT),
            "/api/transfer/pom",
        )
        .await
        .ok()?;
    let resp = match request.send().await {
        Ok(resp) => resp,
        Err(error) if error.is_connect() => {
            let query = [("sha", sha256.to_string())];
            tunnel_get(
                state,
                peer_id,
                "/api/transfer/pom",
                &query,
                None,
                MANIFEST_TIMEOUT,
            )
            .await
            .ok()?
        }
        Err(_) => return None,
    };
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

/// The node a file comes from, as the p2p tunnel addresses it.
async fn tunnel_peer(
    state: &GatewayState,
    peer_id: &str,
) -> Result<(crate::peer_client::TunnelPeer, String), String> {
    let peers = state.network.alive_peers().await;
    let peer = peers
        .iter()
        .find(|peer| peer.node_id == peer_id)
        .ok_or_else(|| "the source is not on the network".to_string())?;
    let tunnel = crate::peer_client::TunnelPeer::from_peer(peer)
        .ok_or_else(|| "the source does not announce the p2p tunnel".to_string())?;
    Ok((tunnel, peer.endpoint_id.clone().unwrap_or_default()))
}

/// Signed GET to the gateway of the node a file comes from, through the
/// p2p tunnel: the path left when no direct address connects.
async fn tunnel_get(
    state: &GatewayState,
    peer_id: &str,
    path: &str,
    query: &[(&str, String)],
    range: Option<String>,
    timeout: Duration,
) -> Result<reqwest::Response, String> {
    let (tunnel, _) = tunnel_peer(state, peer_id).await?;
    let client = state
        .peer_http
        .for_peer(peer_id)
        .map_err(|e| e.to_string())?;
    let mut request = state
        .control_get_request(
            client
                .get(format!("http://tunnel.invalid{path}"))
                .query(query)
                .timeout(timeout),
            path,
        )
        .await
        .map_err(|e| e.to_string())?;
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    crate::peer_client::send_request_via_tunnel(&tunnel, request)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// How long a download waits for a connection that may carry it, after the
/// tunnel reached the peer: the hole punch runs over the relayed connection
/// and takes a few seconds to close.
const TRANSFER_LINK_WAIT: Duration = Duration::from_secs(if cfg!(test) { 0 } else { 12 });

/// Brings a file through the p2p tunnel, one bounded piece at a time,
/// appending to `file` from `already`. Returns the bytes on disk.
///
/// **Never through a public relay.** A piece only travels when the
/// connection's selected path is direct (dialed or hole punched) or goes
/// through a member relay that allows transfers; the serving node checks the
/// path of the connection each piece arrives on and answers
/// `TRANSFER_ROUTE_REFUSED_STATUS` otherwise. A connection moves from a relay
/// to a direct path when the punch closes, so a refusal is retried while a
/// connection that may carry the file exists.
#[allow(clippy::too_many_arguments)]
async fn download_over_tunnel(
    state: &GatewayState,
    job_id: &str,
    peer_id: &str,
    locator: &[(&str, String)],
    part: &Path,
    file: &mut tokio::fs::File,
    already: u64,
    total: u64,
) -> Result<u64, String> {
    use crate::infer_tunnel::{TRANSFER_CHUNK_BYTES, TRANSFER_ROUTE_REFUSED_STATUS};

    let (_, endpoint_peer) = tunnel_peer(state, peer_id).await?;
    let no_route = "no direct route to the source: the only path goes through a public relay, which does not carry files";
    let waited = Instant::now();
    loop {
        let (allowed, _) = state.network.transfer_links(&endpoint_peer).await;
        if allowed > 0 {
            break;
        }
        if waited.elapsed() >= TRANSFER_LINK_WAIT {
            return Err(no_route.to_string());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    info!(
        "{}: no direct address, bringing it through the p2p tunnel",
        part.display()
    );

    let mut done = already;
    let started = Instant::now();
    let mut last_report = Instant::now();
    while done < total {
        if state.transfers.is_cancelled(job_id).await {
            let _ = tokio::fs::remove_file(part).await;
            return Err("cancelled".to_string());
        }
        let last = (done + TRANSFER_CHUNK_BYTES).min(total) - 1;
        let mut refusals = 0usize;
        let piece = loop {
            let resp = tunnel_get(
                state,
                peer_id,
                "/api/transfer/download",
                locator,
                Some(format!("bytes={done}-{last}")),
                Duration::from_secs(120),
            )
            .await?;
            let status = resp.status();
            if status.as_u16() == TRANSFER_ROUTE_REFUSED_STATUS {
                // This request went over a connection that carries no files.
                let (allowed, links) = state.network.transfer_links(&endpoint_peer).await;
                refusals += 1;
                if allowed == 0 || refusals > 4 * links + 4 {
                    return Err(no_route.to_string());
                }
                continue;
            }
            let body = resp.bytes().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                let text = String::from_utf8_lossy(&body);
                return Err(format!("peer answered {status}: {}", text.trim()));
            }
            break body;
        };
        if piece.len() as u64 != last - done + 1 {
            return Err("the source interrupted the transfer".to_string());
        }
        file.write_all(&piece).await.map_err(|e| e.to_string())?;
        done += piece.len() as u64;
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
    Ok(done)
}

/// Current stage of a job, so the screen has something to say while waiting.
async fn set_detail(state: &GatewayState, job_id: &str, detail: &str) {
    let mut jobs = state.transfers.jobs.write().await;
    if let Some(j) = jobs.get_mut(job_id) {
        j.detail = detail.to_string();
    }
}

/// Size and digest of the file on the other side - what decides whether it is worth starting.
async fn fetch_manifest(
    state: &GatewayState,
    peer_id: &str,
    base_url: &str,
    _key: &str,
    locator: &[(&str, String)],
    from_host: &str,
    name: &str,
) -> Result<(u64, String), String> {
    let client = state
        .peer_http
        .for_peer(peer_id)
        .map_err(|e| e.to_string())?;
    let request = state
        .control_get_request(
            client
                .get(format!("{base_url}/api/transfer/manifest"))
                .query(locator)
                .timeout(MANIFEST_TIMEOUT),
            "/api/transfer/manifest",
        )
        .await
        .map_err(|e| format!("could not sign the query on {from_host}: {e}"))?;
    let resp = match request.send().await {
        Ok(resp) => resp,
        // No direct address connects: the question is small and goes through
        // the tunnel, like the offer does.
        Err(error) if error.is_connect() => tunnel_get(
            state,
            peer_id,
            "/api/transfer/manifest",
            locator,
            None,
            MANIFEST_TIMEOUT,
        )
        .await
        .map_err(|tunnel| format!("could not reach {from_host}: {error}; {tunnel}"))?,
        Err(e) => return Err(format!("could not reach {from_host}: {e}")),
    };

    // Check the status BEFORE trying to read JSON. A gateway error usually
    // comes as plain text - the TLS gate's 403 is literally the phrase "this
    // cluster requires TLS between nodes; redo the call over https" -, and `.json()`
    // on top of it produced "invalid manifest: error decoding response body",
    // which points nowhere. The explanation from the other side is the best
    // there is: repeat it.
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let body = body.trim();
        return Err(if body.is_empty() {
            format!("{from_host} refused the manifest: HTTP {status}")
        } else {
            format!("{from_host} refused the manifest (HTTP {status}): {body}")
        });
    }

    let manifest: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("invalid manifest from {from_host}: {e}"))?;

    let total = manifest["size_bytes"].as_u64().unwrap_or(0);
    let md5 = manifest["md5"].as_str().unwrap_or_default().to_string();
    if total == 0 || md5.is_empty() {
        return Err(format!("{from_host} could not describe {name}"));
    }

    // Refusing here beats filling the disk and failing halfway.
    let free_mb = sysmon_disk_free_mb().await;
    let needed_mb = total / 1_048_576 + DISK_HEADROOM_MB;
    if free_mb > 0 && free_mb < needed_mb {
        return Err(format!(
            "not enough space: {name} needs ~{needed_mb} MB (with slack) and only {free_mb} MB are free"
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
    _key: &str,
    locator: &[(&str, String)],
    part: &Path,
    dest: &Path,
    total: u64,
    expected_md5: &str,
    // `expected_sha256`, when present, is what rules: the receiver checks the
    // hash it **asked for**, not a digest the sender declared. It is the
    // difference between "the other side said it is right" and "I checked".
    expected_sha256: Option<&str>,
) -> Result<(), String> {
    use futures::StreamExt;

    // Resume whatever a previous attempt already wrote.
    let already = tokio::fs::metadata(part)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let already = already.min(total);

    let client = state
        .peer_http
        .for_peer(peer_id)
        .map_err(|e| e.to_string())?;
    let mut req = state
        .control_get_request(
            client
                .get(format!("{base_url}/api/transfer/download"))
                .query(locator)
                .timeout(Duration::from_secs(60 * 60 * 6)),
            "/api/transfer/download",
        )
        .await
        .map_err(|e| e.to_string())?;
    if already > 0 {
        info!("resuming {} from {} bytes", dest.display(), already);
        req = req.header(header::RANGE, format!("bytes={already}-"));
    }

    // Direct HTTP first. Only when no address connects does the file come
    // through the p2p tunnel, and never across a public relay.
    let direct = match req.send().await {
        Ok(resp) => Some(resp),
        Err(error) if error.is_connect() => {
            info!("{}: no direct address connected ({error})", dest.display());
            None
        }
        Err(e) => return Err(e.to_string()),
    };
    if let Some(resp) = &direct {
        if !resp.status().is_success() {
            // Along with the status goes the phrase from the other side: see the twin note in
            // `fetch_manifest`.
            let status = resp.status();
            let body = direct
                .expect("checked above")
                .text()
                .await
                .unwrap_or_default();
            let body = body.trim();
            return Err(if body.is_empty() {
                format!("peer answered {status}")
            } else {
                format!("peer answered {status}: {body}")
            });
        }
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
    let Some(resp) = direct else {
        done = download_over_tunnel(
            state, job_id, peer_id, locator, part, &mut file, already, total,
        )
        .await?;
        return finish_download(
            state,
            job_id,
            part,
            dest,
            file,
            done,
            expected_md5,
            expected_sha256,
        )
        .await;
    };
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        if state.transfers.is_cancelled(job_id).await {
            drop(file);
            // Cancelling is giving up: the partial leaves the disk. Keeping it would take up
            // gigabytes the screen does not show.
            let _ = tokio::fs::remove_file(part).await;
            return Err("cancelled".to_string());
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
    finish_download(
        state,
        job_id,
        part,
        dest,
        file,
        done,
        expected_md5,
        expected_sha256,
    )
    .await
}

/// Closes a download whichever way the bytes came: flush, verify, rename.
#[allow(clippy::too_many_arguments)]
async fn finish_download(
    state: &GatewayState,
    job_id: &str,
    part: &Path,
    dest: &Path,
    mut file: tokio::fs::File,
    done: u64,
    expected_md5: &str,
    expected_sha256: Option<&str>,
) -> Result<(), String> {
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

    if let Some(expected) = expected_sha256 {
        let p = part.to_path_buf();
        let expected = expected.to_string();
        let got = tokio::task::spawn_blocking(move || crate::pom::hash_file(&p))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?
            .0;
        if got != expected {
            // It is not "the download failed": it is content that is not what was
            // asked for. The partial file goes away in `spawn_download`'s error handling,
            // and nothing with this name reaches the disk as good.
            return Err(format!(
                "sha256 does not match (requested {}, got {})",
                &expected[..12],
                &got[..12]
            ));
        }
    } else {
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
                "MD5 does not match (expected {expected_md5}, got {got})"
            ));
        }
    }

    // On Windows rename does not replace: if the destination exists, it fails with
    // "access denied" instead of swapping the file. Getting here with the destination
    // occupied means the file was demoted while this download
    // was running, and what just passed the MD5 is the good one.
    #[cfg(windows)]
    if dest.exists() {
        let _ = tokio::fs::remove_file(dest).await;
    }

    tokio::fs::rename(part, dest)
        .await
        .map_err(|e| e.to_string())?;
    // What just arrived has to show up in this machine's catalog now, not
    // after the cache's next pass: a deployment reads it right away.
    crate::webui::invalidate_catalog(state.database.clone()).await;

    {
        let mut jobs = state.transfers.jobs.write().await;
        if let Some(j) = jobs.get_mut(job_id) {
            j.bytes_done = done;
            j.eta_secs = Some(0);
        }
    }
    state
        .transfers
        .finish(
            job_id,
            TRANSFER_DONE,
            format!("MD5 verified · {}", dest.display()),
        )
        .await;
    info!("✅ slice received and verified: {}", dest.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Automatic acceptance writes gigabytes to the receiver's disk without
    /// asking. It only turns on when someone wrote a real value in this
    /// machine's variable - an absent, empty or disabled variable still
    /// requires the click, which is the production default.
    #[test]
    fn auto_accept_stays_shut_unless_explicitly_opened() {
        assert!(
            !auto_accept_set(None),
            "without the variable, nobody accepts anything"
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

    /// The sender needs to appear in its own list, with direction and the same id
    /// the receiver minted - it is the shared id that makes symmetric
    /// cancellation possible.
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
                Some(&SliceLabel {
                    model_label: "qwen3:8b".into(),
                    start_layer: 0,
                    end_layer: 15,
                }),
            )
            .await;

        let jobs = transfers.jobs.read().await;
        let job = jobs.get("job-1").expect("job registered");
        assert_eq!(job.direction, TRANSFER_SENDING);
        assert_eq!(job.peer_host, "ksia");
        assert_eq!(job.bytes_total, 4_096);
        assert_eq!(job.state, TRANSFER_RUNNING);
        assert_eq!(job.path, "/models/fatia_0_15.gguf");
        assert!(job.finished_at.is_none(), "not finished yet");
        // And the screen has something to show beyond the file name.
        assert_eq!(job.model_label.as_deref(), Some("qwen3:8b"));
        assert_eq!((job.start_layer, job.end_layer), (Some(0), Some(15)));
    }

    /// A part's descriptor comes from the `.pom`, and only from there.
    ///
    /// The file name is the sha256 and claims nothing about any layer;
    /// deducing the range from it was a real defect. A hash the manifest does not
    /// describe does not get an invented label.
    #[test]
    fn a_slice_describes_itself_through_its_manifest() {
        use crate::pom::{PomKind, PomManifest, PomModel, PomPart};

        let sha = "a".repeat(64);
        let other = "b".repeat(64);
        let m = PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 32,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "no-1",
            "a".repeat(64),
            vec![PomPart {
                sha256: sha.clone(),
                bytes: 10,
                start_layer: 16,
                end_layer: 31,
                is_first: false,
                is_last: true,
                tp: None,
            }],
        );

        let f = slice_label(&m, &sha).expect("the manifest describes this part");
        assert_eq!(f.model_label, "qwen3:8b");
        assert_eq!((f.start_layer, f.end_layer), (16, 31));

        assert!(
            slice_label(&m, &other).is_none(),
            "a part the manifest does not describe gets no invented label"
        );
    }

    /// A job that finished carries **when** it finished.
    ///
    /// It is what lets the bell show the outcome: without the mark it only knows how to list
    /// what is in progress, and "completed"/"refused" vanished from the screen the
    /// instant it happened - the operator only found the result again on the
    /// Transfers screen, which is exactly what was reported.
    #[tokio::test]
    async fn a_finished_job_records_when_it_finished() {
        let transfers = TransferState::default();
        transfers
            .start_job(
                "job-1",
                "f.gguf",
                TRANSFER_RECEIVING,
                "ksia",
                "/m/f.gguf",
                10,
                None,
            )
            .await;

        transfers
            .finish("job-1", TRANSFER_DONE, "MD5 verified")
            .await;

        let jobs = transfers.jobs.read().await;
        let job = jobs.get("job-1").unwrap();
        assert_eq!(job.state, TRANSFER_DONE);
        assert_eq!(job.detail, "MD5 verified");
        assert!(
            job.finished_at.is_some(),
            "without the time the bell does not know this just happened"
        );
    }

    /// A terminal state is not rewritten by another.
    ///
    /// "Cancelled by ksia" was someone's decision; the loop that later notices the dead
    /// stream cannot swap that for an anonymous "failed".
    #[tokio::test]
    async fn a_finished_job_is_never_relabelled() {
        let transfers = TransferState::default();
        transfers
            .start_job(
                "job-1",
                "f.gguf",
                TRANSFER_SENDING,
                "ksia",
                "/m/f.gguf",
                10,
                None,
            )
            .await;

        transfers.cancel("job-1", "atena").await;
        let when = {
            let jobs = transfers.jobs.read().await;
            jobs.get("job-1").unwrap().finished_at.clone()
        };
        assert!(when.is_some(), "cancelling also stamps the time");

        transfers
            .finish("job-1", TRANSFER_FAILED, "connection died")
            .await;
        transfers.finish("job-1", TRANSFER_DONE, "enviado").await;

        let jobs = transfers.jobs.read().await;
        let job = jobs.get("job-1").unwrap();
        assert_eq!(job.state, TRANSFER_CANCELLED, "someone's decision won");
        assert!(job.detail.contains("atena"), "detail: {}", job.detail);
        assert_eq!(
            job.finished_at, when,
            "and the time is that of the cancellation"
        );
    }

    /// Cancelling is idempotent and does not depend on the job existing: the notice from the other
    /// side may arrive before the transfer has been registered here.
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
                None,
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

        // Twice is the same as once.
        transfers.cancel("job-1", "macbook").await;
        assert!(transfers.is_cancelled("job-1").await);

        // An id that has not arrived here yet is marked anyway, so that the loop
        // that starts later is born already cancelled.
        transfers.cancel("job-desconhecido", "ksia").await;
        assert!(transfers.is_cancelled("job-desconhecido").await);
    }

    /// The rule is a conjunction: signature in the header AND a GGUF name (extension
    /// `.gguf`, or none).
    #[test]
    fn only_real_gguf_files_pass_the_gate() {
        let dir = std::env::temp_dir().join(format!("pom-gguf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let ok = dir.join("fatia.gguf");
        std::fs::write(&ok, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&ok).is_ok());

        // An Ollama blob is not an allowed transfer.
        let blob = dir.join("sha256-abc123");
        std::fs::write(&blob, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&blob).is_err());

        // No extension, as Hugging Face publishes: the signature decides.
        let bare = dir.join("MTP");
        std::fs::write(&bare, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&bare).is_ok());
        let bare_fake = dir.join("LICENSE");
        std::fs::write(&bare_fake, b"NOTG____").unwrap();
        assert!(ensure_gguf(&bare_fake).is_err());

        // The name lies about the content.
        let fake = dir.join("mentira.gguf");
        std::fs::write(&fake, b"NOTG____").unwrap();
        assert!(ensure_gguf(&fake).is_err());

        // Right content, wrong extension: still a no.
        let wrong_ext = dir.join("modelo.txt");
        std::fs::write(&wrong_ext, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&wrong_ext).is_err());

        // Uppercase does not matter.
        let upper = dir.join("FATIA.GGUF");
        std::fs::write(&upper, b"GGUF\x03\x00\x00\x00").unwrap();
        assert!(ensure_gguf(&upper).is_ok());

        // Smaller than the signature itself.
        let tiny = dir.join("curto.gguf");
        std::fs::write(&tiny, b"GG").unwrap();
        assert!(ensure_gguf(&tiny).is_err());

        assert!(ensure_gguf(&dir).is_err(), "a directory is not a file");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// What the operator chooses on their own screen still has to be a GGUF.
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
        assert_eq!(valid_transfer_filename("MTP").unwrap(), "MTP");
        assert!(valid_transfer_filename("sha256-abc123").is_err());

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

/// How long the offer takes to leave this machine.
///
/// The defect these tests guard: the offer was posted **after** the MD5 of the
/// whole file, so the bell on the other side only lit up when the read
/// finished. Measured on the captain's SD card, `md5` of a 4.2 GB slice
/// costs 3 min 23 s (~20 MB/s) - exactly the "sometimes 1 or 2 minutes" of the
/// report, with the digest cache by (path, mtime, size) masking the
/// delay on the second send of the same file.
#[cfg(test)]
mod offer_latency_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A real GGUF, large enough that reading it whole shows up on the
    /// clock if someone goes back to reading it.
    fn write_gguf(path: &Path, mib: usize) {
        let mut body = Vec::with_capacity(mib * 1024 * 1024);
        body.extend_from_slice(GGUF_MAGIC);
        body.resize(mib * 1024 * 1024, 0x5a);
        std::fs::write(path, body).unwrap();
    }

    /// The peer that receives the offer, reduced to what matters: it notes the body
    /// that arrived and answers. It is a real HTTP server, on loopback.
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

    /// A complete sender node, with a peer already in the table pointing to
    /// `peer_port`. No UDP leaves here: the table is seeded by hand.
    async fn sender_state(peer_port: u16) -> GatewayState {
        crate::gateway::testkit::state_with(
            "eu",
            vec![crate::gateway::testkit::peer("peer-b", "peer-b", peer_port)],
        )
        .await
    }

    /// The download through the p2p tunnel asks for one closed piece at a
    /// time. The sender answers exactly that piece, keeps one job for the
    /// whole file, and does not reopen a job it already closed.
    #[tokio::test]
    async fn a_closed_range_serves_one_piece_and_keeps_one_job() {
        let state = sender_state(1).await;
        let dir = std::env::temp_dir().join(format!("pom-piece-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        state.auth.write().await.shard_dir = Some(dir.to_string_lossy().into());
        // Only a GGUF is served; the magic is all the check reads here.
        let mut content = b"GGUF".to_vec();
        content.extend(4..100u8);
        std::fs::write(dir.join("avulso.gguf"), &content).unwrap();

        let ask = |range: &'static str| {
            let state = state.clone();
            async move {
                let mut headers = HeaderMap::new();
                headers.insert(header::RANGE, range.parse().unwrap());
                let query = PathQuery {
                    path: Some("avulso.gguf".into()),
                    grant: None,
                    sha: None,
                    job: Some("job-pieces".into()),
                    peer: None,
                };
                download(ConnectionAuth, State(state), headers, Query(query)).await
            }
        };

        let first = ask("bytes=0-39").await;
        assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            first.headers().get(header::CONTENT_RANGE).unwrap(),
            "bytes 0-39/100"
        );
        let body = axum::body::to_bytes(first.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], &content[..40]);
        assert_eq!(
            state.transfers.jobs.read().await["job-pieces"].state,
            TRANSFER_RUNNING
        );

        // The last piece is shorter than asked and closes the job.
        let last = ask("bytes=40-999").await;
        let body = axum::body::to_bytes(last.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], &content[40..]);
        assert_eq!(
            state.transfers.jobs.read().await["job-pieces"].state,
            TRANSFER_DONE
        );

        // A job closed on this side is not reopened by one more piece.
        assert_eq!(ask("bytes=0-39").await.status(), StatusCode::GONE);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The offer is an invitation, not the transfer: it only needs to say what
    /// the file is and where to fetch it. The MD5 is checked by the receiver, after
    /// Whoever dials decides the scheme, not the offer.
    ///
    /// The offer is written by the sender and a node of a previous version writes a fixed
    /// `http://`. With the cluster on TLS this yielded a plain-text 403, and
    /// `.json()` on top of that phrase became "invalid manifest: error decoding
    /// response body" - an error that points to nothing (18/08).
    #[test]
    fn the_dialled_scheme_comes_from_local_policy_not_from_the_offer() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();

        crate::peer_client::set_tls_enabled(true);
        assert_eq!(
            normalize_scheme("http://ksia:8080"),
            "https://ksia:8080",
            "an old offer over http must be promoted"
        );
        assert_eq!(normalize_scheme("https://ksia:8080"), "https://ksia:8080");
        assert_eq!(
            normalize_scheme("http://[2804:d55::1]:8080"),
            "https://[2804:d55::1]:8080",
            "bracketed IPv6 survives"
        );

        crate::peer_client::set_tls_enabled(false);
        assert_eq!(
            normalize_scheme("https://ksia:8080"),
            "http://ksia:8080",
            "a plain cluster downgrades the offer"
        );

        crate::peer_client::set_tls_enabled(false);
    }

    /// accepting, with the number it asks for itself at `/api/transfer/manifest` -
    /// hashing here was work nobody consumed and that held back the bell on the
    /// other side for the time it took to read the whole file.
    #[tokio::test]
    async fn an_offer_leaves_without_reading_the_whole_file() {
        // Serializes with the tests that touch the TLS policy, and pins "in the
        // clear": this test dials a fake peer over plain HTTP. Without this, a
        // parallel test that turns the policy on made the client try TLS against
        // the fake server, the offer was not delivered, the `let _ =` swallowed the
        // error and the assertion blew up on `offers[0]` with the list empty - the
        // intermittency that chased the suite the whole of 18/08.
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        crate::peer_client::set_tls_enabled(false);

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
        assert_eq!(resp.status(), StatusCode::OK, "the offer must be accepted");

        let offers = seen.read().await;
        assert_eq!(offers.len(), 1, "the peer must have received the offer");
        let offer = &offers[0];
        assert_eq!(offer["name"], "fatia_0_15.gguf");
        assert_eq!(offer["size_bytes"], 24 * 1024 * 1024);
        // The range used to come from the file name. It no longer does: `fatia_0_15.gguf`
        // stopped being a claim about any layer, and guessing from
        // it was precisely the defect the `.pom` closes.
        assert!(offer["start_layer"].is_null());
        assert!(offer["end_layer"].is_null());

        // The proof that nothing was read: this machine's digest cache is still
        // empty. It is populated by `file_digest` and nothing else, so an
        // entry here means someone read the whole file before
        // knocking on the neighbor's door.
        assert!(
            state.transfers.digests.read().await.is_empty(),
            "the offer cannot hash the file: that is what made the bell on the other side slow"
        );

        // Belt and suspenders: with the hash out of the way, the invitation goes out in
        // milliseconds even here, where the file is small.
        assert!(
            elapsed < Duration::from_secs(2),
            "offer took {elapsed:?}; it cannot depend on the file size"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The announced size has to be the real one: it is what the other side shows
    /// in the notification before deciding to accept gigabytes.
    #[tokio::test]
    async fn the_offer_carries_the_real_size_without_a_digest() {
        // Serializes with the tests that touch the TLS policy, and pins "in the
        // clear": this test dials a fake peer over plain HTTP. Without this, a
        // parallel test that turns the policy on made the client try TLS against
        // the fake server, the offer was not delivered, the `let _ =` swallowed the
        // error and the assertion blew up on `offers[0]` with the list empty - the
        // intermittency that chased the suite the whole of 18/08.
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        crate::peer_client::set_tls_enabled(false);

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
        // Compatibility: an old peer requires the field, so it keeps
        // traveling - empty, which is how one says "I don't know yet".
        assert_eq!(offers[0]["md5"], "");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Accepting cannot be stuck waiting on the other side. The manifest is where the MD5
    /// is finally computed, and on a slow volume that is minutes: if the click
    /// waited for it, the operator would have swapped one wait for another.
    #[tokio::test]
    async fn accepting_returns_before_the_other_side_answers() {
        // Serializes with the tests that touch the TLS policy, and pins "in the
        // clear": this test dials a fake peer over plain HTTP. Without this, a
        // parallel test that turns the policy on made the client try TLS against
        // the fake server, the offer was not delivered, the `let _ =` swallowed the
        // error and the assertion blew up on `offers[0]` with the list empty - the
        // intermittency that chased the suite the whole of 18/08.
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        crate::peer_client::set_tls_enabled(false);

        let slow_hits = Arc::new(AtomicU64::new(0));
        let hits = slow_hits.clone();
        let app = Router::new().route(
            "/api/transfer/manifest",
            get(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    // The other side is reading a file of gigabytes.
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
            OfferTarget::Standalone {
                remote_path: "/qualquer/fatia.gguf".into(),
                grant: None,
                name: "fatia.gguf".into(),
            },
            "peer-b".into(),
        )
        .await
        .expect("accepting registers the transfer right away");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "accepting waited for the manifest: {:?}",
            started.elapsed()
        );

        // And the work really exists: the job is in the list, with the other side
        // already being queried in the background.
        let jobs = state.transfers.jobs.read().await;
        assert!(
            jobs.contains_key(&job_id),
            "the job must show up on the screen"
        );
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
            "the manifest must have been requested in the background"
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
            "expected bracketed, got {out}"
        );
        assert!(
            format!("http://{out}:8090").parse::<reqwest::Url>().is_ok(),
            "the address must form a valid URL: {out}"
        );
    }
}

/// Fetching parts by content: a node asks for a **hash**, not a path.
///
/// It is what closes the torrent model on the network side. Whoever asks does not know (nor
/// need to know) where the other side stored the file, and what arrives either has the
/// requested hash or does not enter the disk with a part name.
#[cfg(test)]
mod part_fetch_tests {
    use super::*;
    use crate::pom;

    /// A real node serving the transfer router on a port.
    async fn serving_node(shard_dir: &Path) -> (GatewayState, u16) {
        let state = crate::gateway::testkit::state_with("servidor", vec![]).await;
        state.auth.write().await.shard_dir = Some(shard_dir.to_string_lossy().into());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = routes().with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (state, port)
    }

    /// **A part does not travel without its `.pom`.**
    ///
    /// Here the origin has the bytes but not the manifest that describes them. The
    /// request is refused at once, before moving anything: a loose part
    /// at the destination is a file nobody knows which model it is from or which layers it
    /// carries, and the screen would show it as a whole model. Failing and saying what is
    /// missing is better than finishing well and leaving junk that looks like a slice.
    #[tokio::test]
    async fn a_part_without_its_manifest_is_refused() {
        let root =
            std::env::temp_dir().join(format!("pom-parte-{}", uuid::Uuid::new_v4().simple()));
        let server_disk = root.join("servidor");
        let client_disk = root.join("cliente");

        // The bytes exist at the origin, the manifest does not.
        let content = crate::splitter::test_gguf_bytes(8);
        let (sha, _) = {
            let tmp = root.join("tmp.gguf");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(&tmp, &content).unwrap();
            pom::hash_file(&tmp).unwrap()
        };
        let server_folder = server_disk.join("qwen3-8b-orfa");
        std::fs::create_dir_all(&server_folder).unwrap();
        std::fs::write(pom::part_path(&server_folder, &sha).unwrap(), &content).unwrap();

        let (_server, port) = serving_node(&server_disk).await;
        let client = crate::gateway::testkit::state_with(
            "cliente",
            vec![crate::gateway::testkit::peer("servidor", "servidor", port)],
        )
        .await;
        client.auth.write().await.shard_dir = Some(client_disk.to_string_lossy().into());

        let resp = start_pull(
            ConnectionAuth,
            State(client.clone()),
            Json(PullRequest {
                sha256: Some(sha.clone()),
                model: Some("qwen3-8b".into()),
                node_id: "servidor".into(),
                remote_path: String::new(),
                name: String::new(),
            }),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "without a manifest, the pull is refused"
        );

        // And nothing was written: no invented round folder, no quarantine.
        assert!(!client_disk.join("recebidos").exists());
        assert!(!client_disk.join("qwen3-8b").exists());

        std::fs::remove_dir_all(&root).ok();
    }

    /// Offering an orphan part is also refused, and on the sender's side: the
    /// machine that has the file without the `.pom` is the one that needs to fix it.
    #[tokio::test]
    async fn offering_an_orphan_part_is_refused_at_the_source() {
        let root = std::env::temp_dir().join(format!("pom-orf-{}", uuid::Uuid::new_v4().simple()));
        let folder = root.join("qwen3-8b-orfa");
        std::fs::create_dir_all(&folder).unwrap();

        let content = crate::splitter::test_gguf_bytes(8);
        let tmp = root.join("tmp.gguf");
        std::fs::write(&tmp, &content).unwrap();
        let (sha, _) = pom::hash_file(&tmp).unwrap();
        let path = pom::part_path(&folder, &sha).unwrap();
        std::fs::rename(&tmp, &path).unwrap();

        let state = crate::gateway::testkit::state_with(
            "eu",
            vec![crate::gateway::testkit::peer("outro", "outro", 8080)],
        )
        .await;
        state.auth.write().await.shard_dir = Some(root.to_string_lossy().into());

        let resp = send_to_peer(
            ConnectionAuth,
            State(state),
            // `main` now accepts only the name, resolved against this machine's
            // catalog - the path no longer comes from the browser.
            Json(SendRequest {
                to_node_id: "outro".into(),
                filename: path.file_name().unwrap().to_string_lossy().into_owned(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        std::fs::remove_dir_all(&root).ok();
    }

    /// Asking for a hash nobody has is refused at once, leaving no file.
    #[tokio::test]
    async fn a_part_nobody_has_fails_without_leaving_a_file() {
        let root =
            std::env::temp_dir().join(format!("pom-sem-parte-{}", uuid::Uuid::new_v4().simple()));
        let server_disk = root.join("servidor");
        let client_disk = root.join("cliente");
        std::fs::create_dir_all(&server_disk).unwrap();

        let (_server, port) = serving_node(&server_disk).await;
        let client = crate::gateway::testkit::state_with(
            "cliente",
            vec![crate::gateway::testkit::peer("servidor", "servidor", port)],
        )
        .await;
        client.auth.write().await.shard_dir = Some(client_disk.to_string_lossy().into());

        let absent = "c".repeat(64);
        let resp = start_pull(
            ConnectionAuth,
            State(client.clone()),
            Json(PullRequest {
                sha256: Some(absent.clone()),
                model: Some("qwen3-8b".into()),
                node_id: "servidor".into(),
                remote_path: String::new(),
                name: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!pom::part_path(&client_disk.join("recebidos"), &absent)
            .unwrap()
            .exists());

        std::fs::remove_dir_all(&root).ok();
    }

    /// The rule holds in both directions: accepting an offer of a slice without a `.pom`
    /// is also refused, even if the other side insists on sending it (a peer
    /// of a previous version, for example).
    #[tokio::test]
    async fn accepting_an_offer_of_a_part_without_a_manifest_is_refused() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let sha = "d".repeat(64);

        let offer = TransferOffer {
            id: "of-1".into(),
            from_node_id: "outro".into(),
            from_host: "outro".into(),
            from_gateway: "http://127.0.0.1:1".into(),
            remote_path: format!("/shards/qwen/{sha}.gguf"),
            grant: Some("g".into()),
            name: format!("{sha}.gguf"),
            size_bytes: 4096,
            md5: String::new(),
            // What is missing.
            pom: None,
            start_layer: None,
            end_layer: None,
        };
        state.transfers.offers.write().await.push(offer);

        let resp = accept_offer(
            ConnectionAuth,
            State(state.clone()),
            AxumPath("of-1".to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // A file that is **not** a part still passes: the requirement is about
        // slices, not about everything that arrives.
        let whole = TransferOffer {
            id: "of-2".into(),
            from_node_id: "outro".into(),
            from_host: "outro".into(),
            from_gateway: "http://127.0.0.1:1".into(),
            remote_path: "/models/qwen3.5-9b.gguf".into(),
            grant: Some("g".into()),
            name: "qwen3.5-9b.gguf".into(),
            size_bytes: 4096,
            md5: String::new(),
            pom: None,
            start_layer: None,
            end_layer: None,
        };
        assert!(offer_target(&state, &whole).await.is_ok());
    }

    /// A `sha` from the network cannot become a path: `find_part` only accepts a hash.
    #[test]
    fn a_sha_from_the_wire_cannot_escape_the_shard_dir() {
        let dir = std::env::temp_dir();
        assert!(find_part(&dir, "../../etc/passwd").is_none());
        assert!(find_part(&dir, "").is_none());
        assert!(find_part(&dir, &"z".repeat(64)).is_none());
    }
}

/// The slice does not travel alone: the manifest goes along, and it lands in the model
/// folder - not loose at the root, looking like a whole model.
#[cfg(test)]
mod pom_travels_tests {
    use super::*;
    use crate::pom::{self, PomKind, PomManifest, PomModel, PomPart};

    /// A node with a part and its manifest on disk, serving the router.
    async fn node_with_part(root: &Path) -> (GatewayState, u16, PomManifest, String) {
        let folder = pom::scratch_dir(root);
        std::fs::create_dir_all(&folder).unwrap();

        let bytes = crate::splitter::test_gguf_bytes(8);
        let tmp = folder.join("tmp");
        std::fs::write(&tmp, &bytes).unwrap();
        let (sha, n) = pom::hash_file(&tmp).unwrap();
        std::fs::rename(&tmp, pom::part_path(&folder, &sha).unwrap()).unwrap();

        let m = PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 8,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            "origem",
            "a".repeat(64),
            vec![PomPart {
                sha256: sha.clone(),
                bytes: n,
                start_layer: 0,
                end_layer: 7,
                is_first: true,
                is_last: true,
                tp: None,
            }],
        );
        let folder = pom::finalize_run(root, &folder, &m).unwrap();
        let _ = &folder;

        let state = crate::gateway::testkit::state_with("origem", vec![]).await;
        state.auth.write().await.shard_dir = Some(root.to_string_lossy().into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = routes().with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (state, port, m, sha)
    }

    /// Pulling by hash has to bring the `.pom` along and write everything in the model
    /// folder. This is what was missing: the slice arrived alone, at the root of the slice
    /// folder, and the destination's catalog showed it as a whole model with an
    /// illegible name.
    #[tokio::test]
    async fn pulling_a_part_brings_its_manifest_along() {
        let root = std::env::temp_dir().join(format!("pom-tr-{}", uuid::Uuid::new_v4().simple()));
        let source_disk = root.join("origem");
        let dest_disk = root.join("destino");
        let (source, port, manifesto, sha) = node_with_part(&source_disk).await;
        source.network.peers.write().await.insert(
            "destino".into(),
            crate::gateway::testkit::peer("destino", "destino", 0),
        );

        let client = crate::gateway::testkit::state_with(
            "destino",
            vec![crate::gateway::testkit::peer("origem", "origem", port)],
        )
        .await;
        client.auth.write().await.shard_dir = Some(dest_disk.to_string_lossy().into());

        let resp = start_pull(
            ConnectionAuth,
            State(client.clone()),
            Json(PullRequest {
                sha256: Some(sha.clone()),
                model: Some("qwen3-8b".into()),
                node_id: "origem".into(),
                remote_path: String::new(),
                name: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let folder = pom::dir_for(&dest_disk, &manifesto);
        let destination = pom::part_path(&folder, &sha).unwrap();
        for _ in 0..100 {
            if destination.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(destination.is_file(), "the part lands in the model folder");
        // And **nothing** at the root of the slice folder, which is where it used to end up.
        assert!(!dest_disk.join("recebidos").exists());
        assert!(!dest_disk.join(format!("{sha}.gguf")).exists());

        // The manifest arrived along, and it is what gives the slice its name in the catalog.
        let manifestos = pom::load_dir(&folder);
        assert_eq!(manifestos.len(), 1, "the `.pom` came with the part");
        assert_eq!(manifestos[0].pom_id, manifesto.pom_id);

        let cat = crate::catalog::discover(&[], &dest_disk);
        assert_eq!(cat.models.len(), 1);
        assert!(
            cat.models[0].name.contains("qwen3:8b") && cat.models[0].name.contains("L0-7"),
            "the slice presents itself by the model and the layers: {}",
            cat.models[0].name
        );
        assert!(
            cat.models[0].is_shard(),
            "and as a slice, not a whole model"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// Why `recebidos/` still exists.
    ///
    /// Not every file that arrives is a **part** of a slicing: sending a whole
    /// model from one machine to another is a legitimate path of the "Send a file"
    /// screen, and a whole GGUF belongs to no `.pom`. It has
    /// no hash to name it nor a model folder to live in, so it lands in
    /// `recebidos/` - and the catalog shows it by its name, as a whole
    /// model. The old defect was the **slice** landing here; the slice now goes
    /// to the model folder (see `pulling_a_part_brings_its_manifest_along`).
    #[tokio::test]
    async fn a_whole_model_has_no_manifest_and_lands_in_recebidos() {
        let root = std::env::temp_dir().join(format!("pom-av-{}", uuid::Uuid::new_v4().simple()));
        let received = root.join("recebidos");
        std::fs::create_dir_all(&received).unwrap();
        // A whole model that arrived by this path.
        std::fs::write(
            received.join("qwen3.5-9b.gguf"),
            crate::splitter::test_gguf_bytes(8),
        )
        .unwrap();

        let cat = crate::catalog::discover(&[], &root);
        assert_eq!(cat.models.len(), 1);
        let m = &cat.models[0];
        assert_eq!(m.name, "qwen3.5-9b.gguf", "it appears by name, not by hash");
        assert!(!m.is_shard(), "and as a whole model, not as a slice");
        assert!(m.sha256.is_none());
        assert!(cat.shard_sets.is_empty(), "it does not form a slice set");

        std::fs::remove_dir_all(&root).ok();
    }

    /// Whoever offers sends the manifest in the offer itself, so the other side
    /// knows what it is accepting before downloading anything.
    #[tokio::test]
    async fn an_offer_carries_the_manifest() {
        let root = std::env::temp_dir().join(format!("pom-of-{}", uuid::Uuid::new_v4().simple()));
        let disk = root.join("origem");
        let (state, _port, manifesto, sha) = node_with_part(&disk).await;

        let folder = pom::dir_for(&disk, &manifesto);
        let path = pom::part_path(&folder, &sha).unwrap();

        let found = manifest_for(&state, &path)
            .await
            .expect("the manifest of the part is next to it");
        assert_eq!(found.pom_id, manifesto.pom_id);

        // A file that is part of no manifest goes alone.
        let loose = root.join("solto.gguf");
        std::fs::write(&loose, crate::splitter::test_gguf_bytes(4)).unwrap();
        assert!(manifest_for(&state, &loose).await.is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn accepting_a_part_merges_with_the_local_manifest() {
        let root = std::env::temp_dir().join(format!(
            "pom-transfer-merge-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let source_sha256 = "a".repeat(64);
        let local_sha256 = "b".repeat(64);
        let remote_sha256 = "c".repeat(64);
        let model = PomModel {
            name: "original".into(),
            label: "original".into(),
            total_layers: 32,
            quant: None,
        };
        let local_part = PomPart {
            sha256: local_sha256.clone(),
            bytes: 5,
            start_layer: 0,
            end_layer: 14,
            is_first: true,
            is_last: false,
            tp: None,
        };
        let remote_part = PomPart {
            sha256: remote_sha256.clone(),
            bytes: 6,
            start_layer: 15,
            end_layer: 31,
            is_first: false,
            is_last: true,
            tp: None,
        };
        let local_manifest = PomManifest::new(
            model.clone(),
            PomKind::Layers,
            "local",
            source_sha256.clone(),
            vec![local_part],
        );
        let scratch = pom::scratch_dir(&root);
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(pom::part_path(&scratch, &local_sha256).unwrap(), b"local").unwrap();
        let model_dir = pom::finalize_run(&root, &scratch, &local_manifest).unwrap();

        let state = crate::gateway::testkit::state_with("destino", vec![]).await;
        state.auth.write().await.shard_dir = Some(root.to_string_lossy().into());
        let incoming = PomManifest::new(
            model,
            PomKind::Layers,
            "remote",
            source_sha256.clone(),
            vec![remote_part],
        );
        let offer = TransferOffer {
            id: "offer-merge".into(),
            from_node_id: "remoto".into(),
            from_host: "remoto".into(),
            from_gateway: "http://127.0.0.1:1".into(),
            remote_path: format!("/slices/{remote_sha256}.gguf"),
            grant: Some("grant".into()),
            name: format!("{remote_sha256}.gguf"),
            size_bytes: 6,
            md5: String::new(),
            pom: Some(incoming),
            start_layer: Some(15),
            end_layer: Some(31),
        };

        let target = offer_target(&state, &offer).await.unwrap();
        let destination = match target {
            OfferTarget::Part { model_dir, .. } => model_dir.unwrap(),
            OfferTarget::Standalone { .. } => panic!("the offer with a manifest is a part"),
        };
        assert_eq!(destination, model_dir);
        let manifestos = pom::load_dir(&destination);
        assert_eq!(manifestos.len(), 1);
        assert_eq!(manifestos[0].pom_id, source_sha256);
        assert_eq!(manifestos[0].parts.len(), 2);
        assert!(manifestos[0].parts.iter().any(|p| p.sha256 == local_sha256));
        assert!(manifestos[0]
            .parts
            .iter()
            .any(|p| p.sha256 == remote_sha256));

        std::fs::remove_dir_all(&root).ok();
    }
}

/// **The verdict goes back to whoever offered, and the send knows when it ends.**
///
/// Two halves of the same silence reported in the field. Whoever sent a slice
/// got no notice at all: neither that the other side accepted, nor that it refused, and
/// the send job stayed `running` forever because nothing closed it when the
/// body finished going out. In practice the bell of the machine that sent showed an
/// eternal transfer at 0 B, and the refusal produced no signal at all.
#[cfg(test)]
mod verdict_tests {
    use super::*;
    use crate::pom::{self, PomKind, PomManifest, PomModel, PomPart};

    /// A node serving the transfer router on an ephemeral port.
    ///
    /// `gateway_port` is adjusted to the real port: it is what goes into the
    /// offer's `from_gateway`, and it is by that address that the verdict comes back.
    /// With the testkit's 8080 both sides would dial a port where there is
    /// nobody.
    async fn serving_node(state: GatewayState, listener: tokio::net::TcpListener) -> GatewayState {
        let app = routes().with_state(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        state
    }

    /// A node with a part on disk, serving the transfer router.
    async fn node_with_part(
        node_id: &str,
        root: &Path,
    ) -> (GatewayState, u16, PomManifest, String) {
        let folder = pom::scratch_dir(root);
        std::fs::create_dir_all(&folder).unwrap();

        let bytes = crate::splitter::test_gguf_bytes(8);
        let tmp = folder.join("tmp");
        std::fs::write(&tmp, &bytes).unwrap();
        let (sha, n) = pom::hash_file(&tmp).unwrap();
        std::fs::rename(&tmp, pom::part_path(&folder, &sha).unwrap()).unwrap();

        let m = PomManifest::new(
            PomModel {
                name: "qwen3-8b".into(),
                label: "qwen3:8b".into(),
                total_layers: 8,
                quant: Some("Q4_K_M".into()),
            },
            PomKind::Layers,
            node_id,
            "a".repeat(64),
            vec![PomPart {
                sha256: sha.clone(),
                bytes: n,
                start_layer: 0,
                end_layer: 7,
                is_first: true,
                is_last: true,
                tp: None,
            }],
        );
        pom::finalize_run(root, &folder, &m).unwrap();

        let mut state = crate::gateway::testkit::state_with(node_id, vec![]).await;
        state.auth.write().await.shard_dir = Some(root.to_string_lossy().into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        state.gateway_port = port;
        let state = serving_node(state, listener).await;
        (state, port, m, sha)
    }

    /// The other side: an empty node that already knows `peer`, serving the router.
    async fn empty_node(
        node_id: &str,
        root: &Path,
        peer: crate::network::PeerInfo,
    ) -> (GatewayState, u16) {
        let mut state = crate::gateway::testkit::state_with(node_id, vec![peer]).await;
        state.auth.write().await.shard_dir = Some(root.to_string_lossy().into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        state.gateway_port = port;
        (serving_node(state, listener).await, port)
    }

    /// Waits for the job to reach a terminal state, or gives up.
    async fn until_done(state: &GatewayState, job_id: &str) -> TransferJob {
        for _ in 0..200 {
            if let Some(j) = state.transfers.jobs.read().await.get(job_id) {
                if is_finished(&j.state) {
                    return j.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job {job_id} never finished");
    }

    /// Accepting notifies whoever offered, and the send closes by itself when the last
    /// byte goes out.
    #[tokio::test]
    async fn accepting_tells_the_sender_and_the_send_completes() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        crate::peer_client::set_tls_enabled(false);

        let root =
            std::env::temp_dir().join(format!("pom-vd-ok-{}", uuid::Uuid::new_v4().simple()));
        let disk_a = root.join("atena");
        let disk_b = root.join("ksia");
        let (atena, port_a, _m, sha) = node_with_part("atena", &disk_a).await;

        // The receiver needs to reach the sender through the real router: it is
        // by it that the verdict comes back.
        let (ksia, port_b) = empty_node(
            "ksia",
            &disk_b,
            crate::gateway::testkit::peer("atena", "atena", port_a),
        )
        .await;
        // And the sender needs to know the destination, to name it on the screen.
        atena.network.peers.write().await.insert(
            "ksia".into(),
            crate::gateway::testkit::peer("ksia", "ksia", port_b),
        );

        let resp = send_to_peer(
            ConnectionAuth,
            State(atena.clone()),
            Json(SendRequest {
                to_node_id: "ksia".into(),
                filename: format!("{sha}.gguf"),
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "the offer must go out");

        // The sender kept the offer to know how to name the verdict.
        assert_eq!(atena.transfers.offered.read().await.len(), 1);

        let offer_id = {
            let offers = ksia.transfers.offers.read().await;
            assert_eq!(offers.len(), 1, "the offer arrived at the destination");
            offers[0].id.clone()
        };

        let resp = accept_offer(ConnectionAuth, State(ksia.clone()), AxumPath(offer_id)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // The download finishes, and the **send** too: it was this second close that
        // did not exist, and that is why the bell of whoever sent never emptied.
        let received = {
            let id = ksia
                .transfers
                .jobs
                .read()
                .await
                .keys()
                .next()
                .cloned()
                .expect("the receiver registered the job");
            until_done(&ksia, &id).await
        };
        assert_eq!(received.state, TRANSFER_DONE);
        assert_eq!(received.direction, TRANSFER_RECEIVING);

        let sent = until_done(&atena, &received.id).await;
        assert_eq!(
            sent.direction, TRANSFER_SENDING,
            "the same id exists on both sides, with opposite directions"
        );
        assert_eq!(
            sent.state, TRANSFER_DONE,
            "the send stayed `running` forever: nothing closed it"
        );
        assert_eq!(sent.peer_host, "ksia", "and the screen says who it went to");
        assert!(
            sent.finished_at.is_some(),
            "without the time the bell does not show the outcome"
        );
        assert_eq!(
            sent.bytes_done, sent.bytes_total,
            "and the final number is the whole file, not the last 500 ms report"
        );

        // Once answered, the offer leaves the sender's registry.
        assert!(atena.transfers.offered.read().await.is_empty());

        // **And both sides can say what it was, without talking about a hash.** The file
        // name is the sha256; what says which model it is from and which layers it carries is
        // the `.pom`, and it travels with the offer and is written before the first byte.
        for (who, job) in [("recebido", &received), ("enviado", &sent)] {
            assert_eq!(
                job.model_label.as_deref(),
                Some("qwen3:8b"),
                "{who}: the screen needs the model, not the hash"
            );
            assert_eq!(
                (job.start_layer, job.end_layer),
                (Some(0), Some(7)),
                "{who}: and the layer range"
            );
            // The hash is still available, as secondary identification: it is what
            // names the file on disk.
            assert_eq!(job.name, format!("{sha}.gguf"), "{who}");
        }

        std::fs::remove_dir_all(&root).ok();
    }

    /// Refusing also notifies - and it was the case that produced no signal at all.
    #[tokio::test]
    async fn rejecting_tells_the_sender_who_refused() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        crate::peer_client::set_tls_enabled(false);

        let root =
            std::env::temp_dir().join(format!("pom-vd-nao-{}", uuid::Uuid::new_v4().simple()));
        let disk_a = root.join("atena");
        let disk_b = root.join("ksia");
        let (atena, port_a, _m, sha) = node_with_part("atena", &disk_a).await;

        // The receiver needs to reach the sender through the real router: it is
        // by it that the verdict comes back.
        let (ksia, port_b) = empty_node(
            "ksia",
            &disk_b,
            crate::gateway::testkit::peer("atena", "atena", port_a),
        )
        .await;
        atena.network.peers.write().await.insert(
            "ksia".into(),
            crate::gateway::testkit::peer("ksia", "ksia", port_b),
        );

        let _ = send_to_peer(
            ConnectionAuth,
            State(atena.clone()),
            Json(SendRequest {
                to_node_id: "ksia".into(),
                filename: format!("{sha}.gguf"),
            }),
        )
        .await;

        let offer_id = ksia.transfers.offers.read().await[0].id.clone();
        let resp = reject_offer(ConnectionAuth, State(ksia.clone()), AxumPath(offer_id)).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // The sender gets to know. Before this the refusal was silence: no
        // job, no log line, nothing on the screen of whoever had offered.
        let mut refusal = None;
        for _ in 0..200 {
            let jobs = atena.transfers.jobs.read().await;
            if let Some(j) = jobs.values().next() {
                refusal = Some(j.clone());
                break;
            }
            drop(jobs);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let refusal = refusal.expect("the refusal must become a notice at the sender");
        assert_eq!(refusal.direction, TRANSFER_SENDING);
        assert_eq!(
            refusal.state, TRANSFER_CANCELLED,
            "a refusal is someone's decision, not a failure"
        );
        assert!(
            refusal.detail.contains("ksia"),
            "the screen must say who refused: {}",
            refusal.detail
        );
        assert!(refusal.finished_at.is_some());
        // The refusal notice also talks about the model, not the hash.
        assert_eq!(refusal.model_label.as_deref(), Some("qwen3:8b"));
        assert_eq!((refusal.start_layer, refusal.end_layer), (Some(0), Some(7)));

        std::fs::remove_dir_all(&root).ok();
    }

    /// A verdict about a grant that this machine did not offer does not
    /// describe anything it can show.
    #[tokio::test]
    async fn an_answer_for_an_unknown_grant_is_refused() {
        let state = crate::gateway::testkit::state_with("atena", vec![]).await;

        let resp = receive_answer(
            ConnectionAuth,
            State(state.clone()),
            Json(AnswerRequest {
                grant: "nao-existe".into(),
                from_node_id: "ksia".into(),
                accepted: true,
                job_id: None,
                reason: None,
            }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(state.transfers.jobs.read().await.is_empty());
    }

    /// And a verdict that does not come from whoever received the offer is another conversation.
    #[tokio::test]
    async fn an_answer_from_the_wrong_node_is_refused() {
        let state = crate::gateway::testkit::state_with("atena", vec![]).await;
        state.transfers.offered.write().await.insert(
            "g-1".into(),
            OfferedFile {
                to_node_id: "ksia".into(),
                to_host: "ksia".into(),
                name: "f.gguf".into(),
                path: "/m/f.gguf".into(),
                size_bytes: 10,
                slice: None,
            },
        );

        let resp = receive_answer(
            ConnectionAuth,
            State(state.clone()),
            Json(AnswerRequest {
                grant: "g-1".into(),
                from_node_id: "intruso".into(),
                accepted: false,
                job_id: None,
                reason: None,
            }),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(state.transfers.jobs.read().await.is_empty());
        assert_eq!(
            state.transfers.offered.read().await.len(),
            1,
            "and the offer still stands, waiting for the right party"
        );
    }
}

#[cfg(test)]
mod offer_etag_tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};
    use axum::body::to_bytes;
    use axum::http::header::{ETAG, IF_NONE_MATCH};

    fn offer(id: &str) -> TransferOffer {
        TransferOffer {
            id: id.into(),
            from_node_id: "ksia-id".into(),
            from_host: "ksia".into(),
            from_gateway: "http://ksia:8080".into(),
            remote_path: "/models/f.gguf".into(),
            grant: Some("grant".into()),
            name: "f.gguf".into(),
            size_bytes: 10,
            md5: String::new(),
            pom: None,
            start_layer: Some(0),
            end_layer: Some(1),
        }
    }

    #[tokio::test]
    async fn unchanged_offers_return_304_and_a_new_offer_changes_the_etag() {
        let state = state_with("eu", vec![]).await;
        state.transfers.offers.write().await.push(offer("offer-1"));

        let first = list_offers(ConnectionAuth, HeaderMap::new(), State(state.clone())).await;
        assert_eq!(first.status(), StatusCode::OK);
        let etag = first.headers().get(ETAG).cloned().expect("offer ETag");
        assert!(body_of(first).await.get("disk_free_mb").is_none());

        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, etag.clone());
        let second = list_offers(ConnectionAuth, headers, State(state.clone())).await;
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
        assert!(to_bytes(second.into_body(), usize::MAX)
            .await
            .unwrap()
            .is_empty());

        state.transfers.offers.write().await.push(offer("offer-2"));
        let third = list_offers(ConnectionAuth, HeaderMap::new(), State(state)).await;
        assert_eq!(third.status(), StatusCode::OK);
        assert_ne!(third.headers().get(ETAG), Some(&etag));
    }
}
