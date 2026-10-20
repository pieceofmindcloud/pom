//! The conversation sessions of a plugin engine across the machines that serve a whole model.
//!
//! A turn names its conversation (`session` in the chat request). The machine that ran the turn saves the
//! conversation in its own engine: the plugin's bridge either saves it before the turn's `done` line (`saved`) or
//! defers the save to an idle engine (`deferred`). Either way that machine then sends the turn, in a background task
//! of its own (`spawn_sync`, which waits for a deferred save to be written), to every other machine that serves the
//! whole model: the turn's delta and its tokens, which the peer applies to the whole session it holds for the
//! conversation. A peer that no longer holds the delta's base gets the whole session instead. Each machine restores
//! the longest saved start of a prompt itself, so the gateway's choice of machine does not change.
//!
//! A turn of a prefill/decode split names the prefill machine that read its prompt (`session_source`): the head
//! fetches the session from that machine before the turn, whatever the transfer costs (`pull_from`).
//!
//! Routes (signed peer calls, the same checks as the generation route):
//! - `POST /api/internal/plugins/:code/engine/session/chunk?key=..&kind=..&offset=..&total=..`, one piece of a file
//! - `POST /api/internal/plugins/:code/engine/session/apply`, `{"key": ..}`: apply a received delta here

use crate::auth::InternalNodeAuth;
use crate::gateway::GatewayState;
use crate::plugin_engine::{check_peer_call, local_upstream, loopback_client, services_of};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

/// Subfolder of the node's KV snapshot directory where the conversation sessions are kept. The KV cache clear empties
/// it with the snapshots (`backend/webui/kv_cache.rs`), so both are cleared by the same action.
pub(crate) const SESSIONS_SUBDIR: &str = "strata-sessions";

/// The largest session file a peer accepts: a whole session of a long conversation is a few hundred MB.
pub(crate) const MAX_SESSION_FILE: usize = 1 << 30;
/// A piece of a session file on the wire (the signed control channel takes bodies up to CONTROL_BODY_LIMIT).
pub(crate) const SESSION_PIECE: usize = 64 << 20;
/// The largest signed control body: a piece and its envelope. Every signed control call is read up to this size.
pub(crate) const CONTROL_BODY_LIMIT: usize = 128 << 20;
const SESSION_TIMEOUT: Duration = Duration::from_secs(900);
/// How often the sync asks its bridge whether a deferred save is still pending.
const SAVE_POLL: Duration = Duration::from_millis(250);
const KINDS: [&str; 4] = ["full", "delta", "full_ids", "delta_meta"];

/// Whether `key` can name a conversation: letters, digits, '-', '_' and '.', not starting with '.', at most 96 bytes.
pub(crate) fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 96
        && !key.starts_with('.')
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// The conversation a chat request belongs to. A request that names one (`session`) is that conversation. One that
/// names none - an agent that sends no session id - is named by a hash of its first user message: every turn of a
/// conversation carries that message unchanged, so its turns share the name. Two conversations that open with the
/// same message share it too, and the later one's save replaces the earlier's; a restore still takes only a session
/// that starts the prompt, so the replacement costs cache, never correctness.
pub(crate) fn session_of(request: &Value) -> Option<String> {
    if let Some(key) = request["session"].as_str().filter(|key| valid_key(key)) {
        return Some(key.to_owned());
    }
    let first = first_user_text(request)?;
    let digest = sha2::Sha256::digest(first.as_bytes());
    Some(format!("auto-{}", &hex::encode(digest)[..24]))
}

/// The text of the first user message of a chat request (its text parts, when the content is a list).
fn first_user_text(request: &Value) -> Option<String> {
    let message = request["messages"]
        .as_array()?
        .iter()
        .find(|message| message["role"] == "user")?;
    let text = match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    (!text.trim().is_empty()).then_some(text)
}

/// The conversation a turn left for the other machines, from the `done` object of its answer: a turn whose save ran
/// (`saved`), or whose save the bridge defers to an idle engine (`deferred`, see `spawn_sync`, which waits for it).
pub(crate) fn saved_session(done: &Value) -> Option<String> {
    let session = done.get("session")?;
    if session["saved"].is_null() && session["deferred"].as_bool() != Some(true) {
        return None;
    }
    session["key"]
        .as_str()
        .filter(|key| valid_key(key))
        .map(str::to_owned)
}

/// Whether a machine's services offer the inference service `code` and are not in an error or waiting state. The
/// service reports `stages: true` on every machine that runs the plugin (a stage can run anywhere), so it does not say
/// whether the machine holds the whole model: a peer that holds a layer range refuses the session (its layers differ),
/// and the refusal is logged.
pub(crate) fn offers_model(services: &[Value], code: &str) -> bool {
    services.iter().any(|entry| {
        entry["plugin_code"] == code
            && entry["kind"] == "inference"
            && entry["status"] != "error"
            && entry["status"] != "waiting"
    })
}

/// Syncs a saved turn to the other whole-model machines in the background. The bridge saved the files before it wrote
/// the turn's `done` line, so they are complete when this runs.
pub(crate) fn spawn_sync(state: GatewayState, code: String, key: String) {
    // one sync of a conversation at a time, and only the newest turn is sent: a sync that a newer turn has overtaken
    // is dropped (the newer one carries the older turns), so two syncs never write the same file at one peer
    let (lock, generation) = {
        let slot = key_sync(&code, &key);
        let generation = slot.latest.fetch_add(1, Ordering::SeqCst) + 1;
        (slot, generation)
    };
    tokio::spawn(async move {
        let _turn = lock.turns.lock().await;
        if lock.latest.load(Ordering::SeqCst) != generation {
            info!(plugin = %code, session = %key, "session sync: a newer turn of the conversation takes its place");
            return;
        }
        // the bridge may still be writing the turn's save (it writes when its engine is idle): this task waits for
        // it, so the turn's answer never does
        match wait_saved(&state, &code, &key).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                warn!("plugin {code}: session {key} not synced to every machine: {error}");
                return;
            }
        }
        if lock.latest.load(Ordering::SeqCst) != generation {
            info!(plugin = %code, session = %key, "session sync: a newer turn of the conversation takes its place");
            return;
        }
        if let Err(error) = sync(&state, &code, &key).await {
            warn!("plugin {code}: session {key} not synced to every machine: {error}");
        }
    });
}

/// The sync state of one conversation: the turns' lock and the newest turn requested.
struct KeySync {
    turns: AsyncMutex<()>,
    latest: AtomicU64,
}

fn key_sync(code: &str, key: &str) -> Arc<KeySync> {
    static SYNCS: OnceLock<StdMutex<HashMap<String, Arc<KeySync>>>> = OnceLock::new();
    let map = SYNCS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(format!("{code}/{key}"))
        .or_insert_with(|| {
            Arc::new(KeySync {
                turns: AsyncMutex::new(()),
                latest: AtomicU64::new(0),
            })
        })
        .clone()
}

/// Waits until this machine's bridge has written the conversation's deferred save (`GET /pom/session/pending`).
/// `Ok(false)`: the sessions are off on this deployment, so there is nothing to sync.
async fn wait_saved(state: &GatewayState, code: &str, key: &str) -> Result<bool, String> {
    let (port, token) = local_upstream(&state.plugins, code).await?;
    let started = std::time::Instant::now();
    loop {
        let response = loopback_client()
            .get(format!(
                "http://127.0.0.1:{port}/pom/session/pending?key={key}"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|error| format!("the plugin did not answer: {error}"))?;
        if response.status() == StatusCode::NOT_IMPLEMENTED {
            return Ok(false);
        }
        if !response.status().is_success() {
            return Err(format!(
                "the plugin refused the pending check: {}",
                response.status()
            ));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|error| format!("the plugin's pending answer broke: {error}"))?;
        if body["pending"].as_bool() != Some(true) {
            return Ok(true);
        }
        if started.elapsed() > SESSION_TIMEOUT {
            return Err(format!(
                "its save was still pending after {} s",
                SESSION_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(SAVE_POLL).await;
    }
}

async fn sync(state: &GatewayState, code: &str, key: &str) -> Result<(), String> {
    let (port, token) = local_upstream(&state.plugins, code).await?;
    let delta = match local_file(port, &token, key, "delta_meta").await? {
        Some(meta) => local_file(port, &token, key, "delta")
            .await?
            .map(|bytes| (meta, bytes)),
        None => None,
    };
    let mut whole = None;
    let mut failures = Vec::new();
    let peers = state.all_peers().await;
    info!(plugin = %code, session = %key, peers = peers.len(), delta = delta.is_some(), "session sync starts");
    for peer in peers {
        if peer.node_id == state.network.node_id {
            continue;
        }
        let Some(services) = services_of(state, &peer.node_id).await else {
            info!(plugin = %code, session = %key, peer = %peer.node_id, "session sync: the peer did not answer its services");
            continue;
        };
        if !offers_model(&services, code) {
            info!(plugin = %code, session = %key, peer = %peer.node_id, "session sync: the peer does not offer this inference service");
            continue;
        }
        if let Some((meta, bytes)) = &delta {
            match push_delta(state, code, key, &peer, meta.clone(), bytes.clone()).await {
                Ok(()) => continue,
                Err(error) => failures.push(format!("{}: {error}", peer.node_id)),
            }
        }
        // no delta, or the peer does not hold its base: the whole session goes instead
        if whole.is_none() {
            whole = Some(local_whole(port, &token, key).await?);
        }
        if let Some((full, full_ids)) = &whole {
            if let Err(error) =
                push_whole(state, code, key, &peer, full.clone(), full_ids.clone()).await
            {
                failures.push(format!("{}: {error}", peer.node_id));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// One file of this machine's bridge for `key`; None when it does not exist.
async fn local_file(
    port: u16,
    token: &str,
    key: &str,
    kind: &str,
) -> Result<Option<Vec<u8>>, String> {
    let response = loopback_client()
        .get(format!(
            "http://127.0.0.1:{port}/pom/session/file?key={key}&kind={kind}"
        ))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|error| format!("the plugin did not answer: {error}"))?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("the plugin refused {kind}: {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|bytes| Some(bytes.to_vec()))
        .map_err(|error| format!("the plugin's {kind} broke: {error}"))
}

async fn local_whole(port: u16, token: &str, key: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let full = local_file(port, token, key, "full")
        .await?
        .ok_or_else(|| format!("no whole session saved for {key}"))?;
    let full_ids = local_file(port, token, key, "full_ids")
        .await?
        .ok_or_else(|| format!("no tokens saved for {key}"))?;
    Ok((full, full_ids))
}

async fn push_delta(
    state: &GatewayState,
    code: &str,
    key: &str,
    peer: &crate::network::PeerInfo,
    meta: Vec<u8>,
    bytes: Vec<u8>,
) -> Result<(), String> {
    post_file(state, code, key, peer, "delta", bytes).await?;
    post_file(state, code, key, peer, "delta_meta", meta).await?;
    let response = state
        .control_post_json_routed(
            peer,
            &format!("/api/internal/plugins/{code}/engine/session/apply"),
            &json!({ "key": key }),
            SESSION_TIMEOUT,
        )
        .await
        .map_err(|error| error.to_string())?;
    answer(response).await
}

async fn push_whole(
    state: &GatewayState,
    code: &str,
    key: &str,
    peer: &crate::network::PeerInfo,
    full: Vec<u8>,
    full_ids: Vec<u8>,
) -> Result<(), String> {
    post_file(state, code, key, peer, "full", full).await?;
    post_file(state, code, key, peer, "full_ids", full_ids).await
}

/// One file of the session, in pieces of SESSION_PIECE: the peer publishes it when the last piece is in.
async fn post_file(
    state: &GatewayState,
    code: &str,
    key: &str,
    peer: &crate::network::PeerInfo,
    kind: &str,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let total = bytes.len();
    let mut offset = 0;
    loop {
        let end = (offset + SESSION_PIECE).min(total);
        let response = state
            .control_post_bytes_routed(
                peer,
                &format!(
                    "/api/internal/plugins/{code}/engine/session/chunk?key={key}&kind={kind}&offset={offset}&total={total}"
                ),
                bytes[offset..end].to_vec(),
                SESSION_TIMEOUT,
            )
            .await
            .map_err(|error| error.to_string())?;
        answer(response).await?;
        offset = end;
        if offset >= total {
            return Ok(());
        }
    }
}

async fn answer(response: reqwest::Response) -> Result<(), String> {
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value["error"].as_str().map(str::to_owned))
        .unwrap_or(text);
    Err(format!("{status}: {message}"))
}

#[derive(Deserialize)]
pub(crate) struct SessionChunkQuery {
    key: String,
    kind: String,
    offset: u64,
    total: u64,
}

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, axum::Json(json!({ "error": error }))).into_response()
}

async fn relay(response: reqwest::Response) -> Response {
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    match response.text().await {
        Ok(text) => (status, [("content-type", "application/json")], text).into_response(),
        Err(error) => refuse(StatusCode::BAD_GATEWAY, &error.to_string()),
    }
}

/// `POST /api/internal/plugins/:code/engine/session/chunk`: one piece of a peer's session file, written by this
/// machine's bridge. Only a signed call from a live peer that serves this model gets through.
pub(crate) async fn engine_session_chunk_route(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Path(code): Path<String>,
    Query(query): Query<SessionChunkQuery>,
    body: Bytes,
) -> Response {
    if let Err(response) = check_peer_call(&auth, &state, &code).await {
        return response;
    }
    if !valid_key(&query.key) || !KINDS.contains(&query.kind.as_str()) {
        return refuse(StatusCode::BAD_REQUEST, "invalid session file");
    }
    if query.total > MAX_SESSION_FILE as u64 || query.offset + body.len() as u64 > query.total {
        return refuse(StatusCode::BAD_REQUEST, "the piece does not fit the file");
    }
    let (port, token) = match local_upstream(&state.plugins, &code).await {
        Ok(upstream) => upstream,
        Err(error) => return refuse(StatusCode::SERVICE_UNAVAILABLE, &error),
    };
    match loopback_client()
        .put(format!(
            "http://127.0.0.1:{port}/pom/session/chunk?key={}&kind={}&offset={}&total={}",
            query.key, query.kind, query.offset, query.total
        ))
        .bearer_auth(token)
        .body(body)
        .send()
        .await
    {
        Ok(response) => relay(response).await,
        Err(error) => refuse(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    }
}

/// `POST /api/internal/plugins/:code/engine/session/apply`: applies the delta a peer sent to the whole session here.
pub(crate) async fn engine_session_apply_route(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Path(code): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(response) = check_peer_call(&auth, &state, &code).await {
        return response;
    }
    let Some(key) = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value["key"].as_str().map(str::to_owned))
        .filter(|key| valid_key(key))
    else {
        return refuse(StatusCode::BAD_REQUEST, "a session key is required");
    };
    let (port, token) = match local_upstream(&state.plugins, &code).await {
        Ok(upstream) => upstream,
        Err(error) => return refuse(StatusCode::SERVICE_UNAVAILABLE, &error),
    };
    match loopback_client()
        .post(format!("http://127.0.0.1:{port}/pom/session"))
        .bearer_auth(token)
        .json(&json!({ "key": key, "action": "apply" }))
        .send()
        .await
    {
        Ok(response) => relay(response).await,
        Err(error) => refuse(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    }
}

// ---- pulling a conversation's session from a peer ---------------------------------------------------------------
//
// The head that runs a turn fetches the conversation's saved session from a peer when it lacks it: before the turn
// starts, `prepare_for_body` asks this machine's bridge and every peer that serves the model what each holds for the
// conversation (`engine/session/offer`). A peer is only used when it has the same layer split (the engine refuses a
// session of another split), its save is written, and it holds more tokens than this machine. The fetch happens when
// the transfer is faster than reading those tokens again (`fetch_pays`), with the rates measured here. The turn then
// restores the longest saved start of its prompt as it always does.

/// How long a head is remembered as holding a conversation: its slot and its file stay there until the conversation
/// moves, and a head that answers nothing for this long is not a source of affinity.
const HOLDER_TTL: Duration = Duration::from_secs(6 * 3600);

/// The head that last ran each conversation (`model/session` -> node id, when). The conversation's KV session lives in
/// that head's batch slot, so its next turn goes back there (`gateway::generation::auto_head`).
static HOLDERS: StdMutex<Option<HashMap<String, (String, Instant)>>> = StdMutex::new(None);

/// Records that `node_id` ran a turn of the conversation `key` of `model`.
pub(crate) fn note_holder(model: &str, key: &str, node_id: &str) {
    let mut holders = HOLDERS.lock().unwrap_or_else(|e| e.into_inner());
    holders
        .get_or_insert_with(HashMap::new)
        .insert(format!("{model}/{key}"), (node_id.to_owned(), Instant::now()));
}

/// The head that holds the conversation `key` of `model`, when it was recorded recently. (The plugin's code is only
/// known once the head is chosen, so the holder is keyed by the model.)
pub(crate) fn holder_of(model: &str, key: &str) -> Option<String> {
    let holders = HOLDERS.lock().unwrap_or_else(|e| e.into_inner());
    let (node, since) = holders.as_ref()?.get(&format!("{model}/{key}"))?;
    (since.elapsed() < HOLDER_TTL).then(|| node.clone())
}

/// Whether a turn is still pushed to the peers (`spawn_sync`). Off: the peers pull the session when they need it
/// (`prepare_for_body`), so nothing is sent unless no one asks. Kept, not deleted, until the pull is proven on the cluster.
pub(crate) const PUSH_SYNC: bool = false;

/// Rates until a measurement exists: a 1 GbE-class link and a prefill speed of a mid-range card.
const DEFAULT_LINK_BYTES_PER_S: f64 = 100.0 * 1024.0 * 1024.0;
const DEFAULT_PREFILL_TOKENS_PER_S: f64 = 1000.0;
/// Round trips and the restore's fixed cost, added to every fetch.
const FETCH_OVERHEAD_S: f64 = 0.5;
/// The weight of a new measurement in its running average.
const RATE_WEIGHT: f64 = 0.3;
const FETCH_TIMEOUT: Duration = Duration::from_secs(900);

static LINK_RATE: StdMutex<Option<f64>> = StdMutex::new(None);
static PREFILL_RATE: StdMutex<Option<f64>> = StdMutex::new(None);

fn fold_rate(cell: &StdMutex<Option<f64>>, sample: f64) {
    if !sample.is_finite() || sample <= 0.0 {
        return;
    }
    let mut rate = cell.lock().unwrap_or_else(|e| e.into_inner());
    *rate = Some(match *rate {
        Some(old) => old + RATE_WEIGHT * (sample - old),
        None => sample,
    });
}

fn measured(cell: &StdMutex<Option<f64>>, default: f64) -> f64 {
    cell.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or(default)
}

/// A turn's prefill speed on this machine (its timings), which the pull weighs against a fetch.
pub(crate) fn note_prefill_rate(tokens_per_s: f64) {
    fold_rate(&PREFILL_RATE, tokens_per_s);
}

/// What a machine can give for a conversation: the tokens and bytes of its file on disk, the tokens its slot holds in
/// RAM (not fetchable), the layer split it runs, the engine settings its sessions are bound to (`config`, None from an
/// older bridge), and whether its save is still being written.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Offer {
    pub tokens: u64,
    pub bytes: u64,
    pub ram_tokens: u64,
    pub layers: String,
    pub config: Option<String>,
    pub pending: bool,
}

impl Offer {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            tokens: value["tokens"].as_u64()?,
            bytes: value["bytes"].as_u64()?,
            ram_tokens: value["ram_tokens"].as_u64().unwrap_or(0),
            layers: value["layers"].as_str()?.to_owned(),
            config: value["config"].as_str().map(str::to_owned),
            pending: value["pending"].as_bool().unwrap_or(false),
        })
    }

    /// The tokens this machine already has for the conversation, on disk or in a slot.
    fn have(&self) -> u64 {
        self.tokens.max(self.ram_tokens)
    }
}

/// Whether a session saved with the layers `saved` can be restored on a machine running the layers `local`: the same
/// split, or a machine that runs the whole model (`whole`) and a session saved on a split that covers every layer, from
/// 0 to the end, as one contiguous chain of ranges (the engine joins the stages into one image on that machine).
/// Whether a session saved with the engine settings `saved` restores on a machine running `local` (Strata's config
/// fingerprint). Unknown on either side (an older bridge): the engine decides.
pub(crate) fn configs_compatible(local: Option<&str>, saved: Option<&str>) -> bool {
    match (local, saved) {
        (Some(local), Some(saved)) => local == saved,
        _ => true,
    }
}

pub(crate) fn layers_compatible(local: &str, saved: &str) -> bool {
    if local == saved {
        return true;
    }
    if local != "whole" {
        return false;
    }
    let mut next = 0u64;
    for range in saved.split('|') {
        let Some((start, end)) = range.split_once('-') else {
            return false;
        };
        let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>()) else {
            return false;
        };
        if start != next || end < start {
            return false;
        }
        next = end + 1;
    }
    next > 0
}

/// The peer whose session gains this machine the most tokens: a session it can restore (`layers_compatible`), the save
/// written, more tokens than this machine has. Ties keep the first peer.
pub(crate) fn best_source<'a>(local: &Offer, peers: &'a [(String, Offer)]) -> Option<&'a (String, Offer)> {
    peers
        .iter()
        .filter(|(_, offer)| {
            layers_compatible(&local.layers, &offer.layers)
                && !offer.pending
                && offer.bytes > 0
                && offer.tokens > local.have()
        })
        .fold(None, |best: Option<&(String, Offer)>, candidate| match best {
            Some(best) if best.1.tokens >= candidate.1.tokens => Some(best),
            _ => Some(candidate),
        })
}

/// Whether fetching `bytes` to gain `gained` tokens is faster than reading those tokens again on this machine.
pub(crate) fn fetch_pays(bytes: u64, gained: u64, link_bytes_per_s: f64, prefill_tokens_per_s: f64) -> bool {
    if gained == 0 || link_bytes_per_s <= 0.0 || prefill_tokens_per_s <= 0.0 {
        return false;
    }
    let fetch = bytes as f64 / link_bytes_per_s + FETCH_OVERHEAD_S;
    let reread = gained as f64 / prefill_tokens_per_s;
    fetch < reread
}

/// A session this machine fetched from a peer before a turn: the head's part of the trace (`session_transfer`).
/// It reaches the gateway in the turn's done line (`session_fetch`) when the head is another machine.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionFetch {
    pub source: String,
    pub tokens: u64,
    pub bytes: u64,
    pub transfer_ms: u64,
    pub save_wait_ms: u64,
}

impl SessionFetch {
    pub(crate) fn to_json(&self) -> Value {
        json!({"source": self.source, "tokens": self.tokens, "bytes": self.bytes,
               "transfer_ms": self.transfer_ms, "save_wait_ms": self.save_wait_ms})
    }

    pub(crate) fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            source: value["source"].as_str()?.to_owned(),
            tokens: value["tokens"].as_u64().unwrap_or(0),
            bytes: value["bytes"].as_u64()?,
            transfer_ms: value["transfer_ms"].as_u64().unwrap_or(0),
            save_wait_ms: value["save_wait_ms"].as_u64().unwrap_or(0),
        })
    }
}

/// Before a turn runs on this machine: fetches the conversation's session from a peer when that is faster than
/// reading the prompt again. Never fails the turn: any problem leaves the turn to read its prompt. Returns what it
/// fetched, if anything.
pub(crate) async fn prepare_for_body(state: &GatewayState, code: &str, body: &Value) -> Option<SessionFetch> {
    let key = body["session"].as_str().filter(|key| valid_key(key))?;
    // a prefill/decode split: the prefill machine read this prompt just now, and its session is the one to take
    if let Some(source) = body["session_source"]
        .as_str()
        .filter(|source| *source != state.network.node_id)
    {
        return pull_from(state, code, key, source).await.unwrap_or_else(|error| {
            warn!(plugin = %code, session = %key, source = %source, "prefill split: session not fetched from the prefill machine: {error}");
            None
        });
    }
    pull(state, code, key).await.unwrap_or_else(|error| {
        info!(plugin = %code, session = %key, "session not fetched from a peer: {error}");
        None
    })
}

async fn pull(state: &GatewayState, code: &str, key: &str) -> Result<Option<SessionFetch>, String> {
    let (port, token) = local_upstream(&state.plugins, code).await?;
    let local = offer_here(port, &token, key).await?;
    let mut offers = Vec::new();
    for peer in state.all_peers().await {
        if peer.node_id == state.network.node_id {
            continue;
        }
        let Some(services) = services_of(state, &peer.node_id).await else {
            continue;
        };
        if !offers_model(&services, code) {
            continue;
        }
        match offer_of_peer(state, code, key, &peer).await {
            Ok(offer) => offers.push((peer.node_id.clone(), offer)),
            Err(error) => info!(plugin = %code, session = %key, peer = %peer.node_id, "no offer: {error}"),
        }
    }
    info!(plugin = %code, session = %key, local_tokens = local.tokens, local_ram_tokens = local.ram_tokens,
          layers = %local.layers, peers = offers.len(), "session pull: offers collected");
    for (node, offer) in &offers {
        info!(plugin = %code, session = %key, peer = %node, tokens = offer.tokens, bytes = offer.bytes,
              layers = %offer.layers, pending = offer.pending, "session pull: peer offer");
    }
    let Some((node, offer)) = best_source(&local, &offers).cloned() else {
        info!(plugin = %code, session = %key, "session pull: no peer holds more of it on this split; the prompt is read here");
        return Ok(None);
    };
    let gained = offer.tokens - local.have();
    let link = measured(&LINK_RATE, DEFAULT_LINK_BYTES_PER_S);
    let prefill = measured(&PREFILL_RATE, DEFAULT_PREFILL_TOKENS_PER_S);
    // the integration test forces the fetch to exercise the transfer (POM_SESSION_FORCE_FETCH=1)
    let forced = std::env::var("POM_SESSION_FORCE_FETCH").is_ok_and(|value| value == "1");
    if !forced && !fetch_pays(offer.bytes, gained, link, prefill) {
        info!(
            plugin = %code, session = %key, peer = %node, gained, bytes = offer.bytes,
            "session not fetched: reading the prompt again is faster ({:.0} MiB/s link, {:.0} tok/s prefill)",
            link / (1024.0 * 1024.0), prefill
        );
        return Ok(None);
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == node)
        .ok_or_else(|| format!("{node} left the network"))?;
    let started = Instant::now();
    fetch_session(state, code, key, &peer, port, &token).await?;
    let seconds = started.elapsed().as_secs_f64().max(1e-3);
    fold_rate(&LINK_RATE, offer.bytes as f64 / seconds);
    info!(plugin = %code, session = %key, peer = %node, gained, bytes = offer.bytes, seconds,
          forced, "session pull: fetched from a peer; the turn restores it from this machine's disk");
    Ok(Some(SessionFetch {
        source: node,
        tokens: offer.tokens,
        bytes: offer.bytes,
        transfer_ms: (seconds * 1000.0).round() as u64,
        save_wait_ms: 0,
    }))
}

/// How long the head waits for the prefill machine's save of a split turn to be written.
const SOURCE_SAVE_WAIT: Duration = Duration::from_secs(300);

/// A prefill/decode split: fetches the session the prefill machine `source` saved for this turn. Its save may still be
/// being written (the engine writes it once the prefill request has ended), so the offer is asked again until it is not
/// pending. No cost check: the gateway sent the prompt there to be read instead of here.
async fn pull_from(state: &GatewayState, code: &str, key: &str, source: &str) -> Result<Option<SessionFetch>, String> {
    let (port, token) = local_upstream(&state.plugins, code).await?;
    let local = offer_here(port, &token, key).await?;
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == source)
        .ok_or_else(|| format!("{source} left the network"))?;
    let waited = Instant::now();
    let offer = loop {
        let offer = offer_of_peer(state, code, key, &peer).await?;
        if !offer.pending {
            break offer;
        }
        if waited.elapsed() > SOURCE_SAVE_WAIT {
            return Err(format!("its save was still pending after {} s", SOURCE_SAVE_WAIT.as_secs()));
        }
        tokio::time::sleep(SAVE_POLL).await;
    };
    if !layers_compatible(&local.layers, &offer.layers) {
        return Err(format!("its layers ({}) are not this machine's ({})", offer.layers, local.layers));
    }
    if !configs_compatible(local.config.as_deref(), offer.config.as_deref()) {
        return Err(format!(
            "its engine settings ({}) are not this machine's ({}): the engine would refuse the session",
            offer.config.as_deref().unwrap_or("?"),
            local.config.as_deref().unwrap_or("?")
        ));
    }
    if offer.bytes == 0 || offer.tokens <= local.have() {
        info!(plugin = %code, session = %key, source = %source, tokens = offer.tokens, local = local.have(),
              "prefill split: this machine already holds as much of the conversation");
        return Ok(None);
    }
    let started = Instant::now();
    fetch_session(state, code, key, &peer, port, &token).await?;
    let seconds = started.elapsed().as_secs_f64().max(1e-3);
    fold_rate(&LINK_RATE, offer.bytes as f64 / seconds);
    info!(plugin = %code, session = %key, source = %source, tokens = offer.tokens, bytes = offer.bytes, seconds,
          waited_s = waited.elapsed().as_secs_f64() - seconds,
          "prefill split: session fetched from the prefill machine; the turn restores it");
    Ok(Some(SessionFetch {
        source: source.to_owned(),
        tokens: offer.tokens,
        bytes: offer.bytes,
        transfer_ms: (seconds * 1000.0).round() as u64,
        save_wait_ms: ((waited.elapsed().as_secs_f64() - seconds).max(0.0) * 1000.0).round() as u64,
    }))
}

/// What the machine `node_id` (this one or a peer) holds for the conversation `key`.
pub(crate) async fn offer_of(state: &GatewayState, code: &str, key: &str, node_id: &str) -> Result<Offer, String> {
    if node_id == state.network.node_id {
        let (port, token) = local_upstream(&state.plugins, code).await?;
        return offer_here(port, &token, key).await;
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == node_id)
        .ok_or_else(|| format!("{node_id} left the network"))?;
    offer_of_peer(state, code, key, &peer).await
}

async fn offer_here(port: u16, token: &str, key: &str) -> Result<Offer, String> {
    let response = loopback_client()
        .get(format!("http://127.0.0.1:{port}/pom/session/offer?key={key}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|error| format!("the plugin did not answer: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("the plugin refused the offer: {}", response.status()));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|error| format!("the plugin's offer broke: {error}"))?;
    Offer::from_json(&value).ok_or_else(|| "the plugin's offer is not readable".to_owned())
}

async fn offer_of_peer(
    state: &GatewayState,
    code: &str,
    key: &str,
    peer: &crate::network::PeerInfo,
) -> Result<Offer, String> {
    let response = state
        .control_get(
            peer,
            &format!("/api/internal/plugins/{code}/engine/session/offer?key={key}"),
            Duration::from_secs(5),
        )
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("status {}", response.status()));
    }
    let value: Value = response.json().await.map_err(|error| error.to_string())?;
    Offer::from_json(&value).ok_or_else(|| "its offer is not readable".to_owned())
}

/// Copies the peer's session files (the token list, then the K/V) into this machine's session folder, a piece at a
/// time, through the bridge's chunk route: the bridge publishes each file when its last piece is in.
async fn fetch_session(
    state: &GatewayState,
    code: &str,
    key: &str,
    peer: &crate::network::PeerInfo,
    port: u16,
    token: &str,
) -> Result<(), String> {
    for kind in ["full_ids", "full"] {
        let mut offset = 0u64;
        let mut total: Option<u64> = None;
        loop {
            if total.is_some_and(|total| offset >= total) {
                break;
            }
            let response = state
                .control_get_routed(
                    peer,
                    &format!(
                        "/api/internal/plugins/{code}/engine/session/file?key={key}&kind={kind}&offset={offset}&len={SESSION_PIECE}"
                    ),
                    FETCH_TIMEOUT,
                )
                .await
                .map_err(|error| format!("{kind}: {error}"))?;
            if !response.status().is_success() {
                return Err(format!("{kind}: the peer refused a piece: {}", response.status()));
            }
            let size = response
                .headers()
                .get("x-file-size")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| format!("{kind}: the peer did not say the file's size"))?;
            if size > MAX_SESSION_FILE as u64 {
                return Err(format!("{kind}: the file is larger than a session may be"));
            }
            match total {
                Some(known) if known != size => return Err(format!("{kind}: the file changed while it was fetched")),
                _ => total = Some(size),
            }
            let piece = response.bytes().await.map_err(|error| format!("{kind}: {error}"))?;
            if piece.is_empty() {
                return Err(format!("{kind}: the peer sent an empty piece before the end"));
            }
            let put = loopback_client()
                .put(format!(
                    "http://127.0.0.1:{port}/pom/session/chunk?key={key}&kind={kind}&offset={offset}&total={size}"
                ))
                .bearer_auth(token)
                .body(piece.to_vec())
                .timeout(FETCH_TIMEOUT)
                .send()
                .await
                .map_err(|error| format!("{kind}: this machine did not take the piece: {error}"))?;
            if !put.status().is_success() {
                return Err(format!("{kind}: this machine refused the piece: {}", put.status()));
            }
            offset += piece.len() as u64;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct SessionOfferQuery {
    key: String,
}

/// `GET /api/internal/plugins/:code/engine/session/offer?key=`: what this machine offers a peer for the conversation.
pub(crate) async fn engine_session_offer_route(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Path(code): Path<String>,
    Query(query): Query<SessionOfferQuery>,
) -> Response {
    if let Err(response) = check_peer_call(&auth, &state, &code).await {
        return response;
    }
    if !valid_key(&query.key) {
        return refuse(StatusCode::BAD_REQUEST, "invalid session key");
    }
    let (port, token) = match local_upstream(&state.plugins, &code).await {
        Ok(upstream) => upstream,
        Err(error) => return refuse(StatusCode::SERVICE_UNAVAILABLE, &error),
    };
    match loopback_client()
        .get(format!("http://127.0.0.1:{port}/pom/session/offer?key={}", query.key))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(response) => relay(response).await,
        Err(error) => refuse(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    }
}

#[derive(Deserialize)]
pub(crate) struct SessionFileQuery {
    key: String,
    kind: String,
    offset: u64,
    len: u64,
}

/// `GET /api/internal/plugins/:code/engine/session/file?key=&kind=&offset=&len=`: one piece of a session file here, with
/// the whole file's size in `x-file-size`.
pub(crate) async fn engine_session_file_route(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Path(code): Path<String>,
    Query(query): Query<SessionFileQuery>,
) -> Response {
    if let Err(response) = check_peer_call(&auth, &state, &code).await {
        return response;
    }
    if !valid_key(&query.key) || !KINDS.contains(&query.kind.as_str()) {
        return refuse(StatusCode::BAD_REQUEST, "invalid session file");
    }
    if query.len == 0 || query.len > SESSION_PIECE as u64 {
        return refuse(StatusCode::BAD_REQUEST, "a piece is 1 byte to 64 MiB");
    }
    let (port, token) = match local_upstream(&state.plugins, &code).await {
        Ok(upstream) => upstream,
        Err(error) => return refuse(StatusCode::SERVICE_UNAVAILABLE, &error),
    };
    let response = match loopback_client()
        .get(format!(
            "http://127.0.0.1:{port}/pom/session/file?key={}&kind={}&offset={}&len={}",
            query.key, query.kind, query.offset, query.len
        ))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return refuse(StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    };
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let size = response.headers().get("x-file-size").cloned();
    let Ok(bytes) = response.bytes().await else {
        return refuse(StatusCode::BAD_GATEWAY, "the piece broke");
    };
    let mut answer = (status, bytes).into_response();
    answer
        .headers_mut()
        .insert("content-type", "application/octet-stream".parse().expect("a static header value"));
    if let Some(size) = size {
        answer.headers_mut().insert("x-file-size", size);
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_file_name_stems() {
        assert!(valid_key("conv-1"));
        assert!(valid_key("a.b_c"));
        assert!(!valid_key(""));
        assert!(!valid_key(".hidden"));
        assert!(!valid_key("a/b"));
        assert!(!valid_key("a b"));
        assert!(!valid_key(&"a".repeat(97)));
    }

    #[test]
    fn a_request_names_its_session_or_takes_its_first_message_as_the_name() {
        let named = json!({"session": "c1", "messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(session_of(&named), Some("c1".into()));
        let turn_one = json!({"messages": [{"role": "user", "content": "fix the bug"}]});
        let turn_two = json!({"messages": [
            {"role": "user", "content": "fix the bug"},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "now test it"},
        ]});
        let other = json!({"messages": [{"role": "user", "content": "write a poem"}]});
        let name = session_of(&turn_one).expect("a named first message");
        assert!(valid_key(&name));
        assert_eq!(session_of(&turn_two), Some(name));
        assert_ne!(session_of(&other), session_of(&turn_one));
        // an invalid name is not a name, and a request with no user text has none
        assert!(session_of(&json!({"session": "../x", "messages": []})).is_none());
        assert_eq!(
            session_of(&json!({"messages": [{"role": "system", "content": "s"}]})),
            None
        );
    }

    #[test]
    fn only_a_saved_or_deferred_turn_names_its_session() {
        let saved = json!({"session": {"key": "c1", "saved": {"ids": [1, 2], "delta": true}}});
        assert_eq!(saved_session(&saved), Some("c1".into()));
        let deferred = json!({"session": {"key": "c1", "saved": null, "deferred": true}});
        assert_eq!(saved_session(&deferred), Some("c1".into()));
        assert_eq!(
            saved_session(&json!({"session": {"key": "c1", "saved": null}})),
            None
        );
        assert_eq!(saved_session(&json!({"finish": "stop"})), None);
    }

    #[test]
    fn the_inference_service_is_offered_by_a_ready_machine_only() {
        let whole = json!({"plugin_code": "strata", "kind": "inference", "status": "ready"});
        let stage = json!({"plugin_code": "strata", "kind": "inference", "status": "ready", "stages": true});
        let failing = json!({"plugin_code": "strata", "kind": "inference", "status": "error"});
        assert!(offers_model(&[whole.clone()], "strata"));
        assert!(offers_model(&[stage], "strata"));
        assert!(!offers_model(&[failing], "strata"));
        assert!(!offers_model(&[whole], "other"));
    }
}

#[cfg(test)]
mod pull_tests {
    use super::*;

    fn offer(tokens: u64, bytes: u64, layers: &str) -> Offer {
        Offer {
            tokens,
            bytes,
            ram_tokens: 0,
            layers: layers.to_owned(),
            config: None,
            pending: false,
        }
    }

    #[test]
    fn a_session_with_other_engine_settings_is_not_fetched() {
        let a = "engine=0.1.44.1 backend=cuda kv=int8 max_context=200000 kv_resident=32768 mtp_window=32768 kv_rot=0";
        let b = "engine=0.1.44.1 backend=cuda kv=int8 max_context=200000 kv_resident=32768 mtp_window=-1 kv_rot=0";
        assert!(configs_compatible(Some(a), Some(a)));
        assert!(!configs_compatible(Some(a), Some(b)));
        assert!(configs_compatible(None, Some(b)), "an older bridge: the engine decides");
        let value = json!({"tokens": 5, "bytes": 9, "layers": "whole", "config": a});
        assert_eq!(Offer::from_json(&value).unwrap().config.as_deref(), Some(a));
    }

    const GIB: u64 = 1 << 30;
    const MIB: f64 = (1 << 20) as f64;

    #[test]
    fn a_large_session_on_a_fast_link_is_fetched_instead_of_read_again() {
        // 2 GiB for 124k tokens: ~19 s over 110 MiB/s, against ~99 s of prefill at 1250 tok/s
        assert!(fetch_pays(2 * GIB, 124_000, 110.0 * MIB, 1250.0));
    }

    #[test]
    fn a_session_over_a_slow_link_is_read_again() {
        // the same 2 GiB over 5 MiB/s: ~410 s, far more than the prefill
        assert!(!fetch_pays(2 * GIB, 124_000, 5.0 * MIB, 1250.0));
    }

    #[test]
    fn a_few_tokens_are_never_worth_a_big_file() {
        assert!(!fetch_pays(2 * GIB, 1_000, 110.0 * MIB, 1250.0));
        assert!(!fetch_pays(2 * GIB, 0, 110.0 * MIB, 1250.0));
    }

    #[test]
    fn the_peer_with_the_most_tokens_on_the_same_split_is_the_source() {
        let local = offer(2_000, 0, "0-35|36-47");
        let peers = vec![
            ("a".to_owned(), offer(120_000, GIB, "0-35|36-47")),
            ("b".to_owned(), offer(124_000, GIB, "0-35|36-47")),
            ("c".to_owned(), offer(200_000, GIB, "0-47")),
        ];
        assert_eq!(best_source(&local, &peers).map(|(node, _)| node.as_str()), Some("b"));
    }

    #[test]
    fn a_session_of_another_split_or_still_being_written_is_never_a_source() {
        let local = offer(2_000, 0, "0-35|36-47");
        let mut pending = offer(120_000, GIB, "0-35|36-47");
        pending.pending = true;
        let peers = vec![
            ("other-split".to_owned(), offer(120_000, GIB, "0-47")),
            ("pending".to_owned(), pending),
        ];
        assert!(best_source(&local, &peers).is_none());
    }

    #[test]
    fn a_peer_that_gains_nothing_is_no_source() {
        let mut local = offer(120_000, 0, "whole");
        local.ram_tokens = 124_000;
        assert!(best_source(&local, &[("a".to_owned(), offer(124_000, GIB, "whole"))]).is_none());
    }

    #[test]
    fn an_offer_reads_the_bridge_answer() {
        let value = json!({"tokens": 5, "bytes": 9, "ram_tokens": 4, "layers": "0-35|36-47", "pending": false});
        let expected = Offer {
            ram_tokens: 4,
            ..offer(5, 9, "0-35|36-47")
        };
        assert_eq!(Offer::from_json(&value), Some(expected));
        assert!(Offer::from_json(&json!({"tokens": 5})).is_none());
    }

    #[test]
    fn a_split_session_restores_on_a_machine_with_the_whole_model() {
        assert!(layers_compatible("whole", "0-35|36-47"));
        assert!(layers_compatible("whole", "0-47"));
        assert!(layers_compatible("0-35|36-47", "0-35|36-47"));
    }

    #[test]
    fn a_whole_session_or_a_partial_chain_is_not_restored_on_a_split_machine() {
        assert!(!layers_compatible("0-35|36-47", "whole"));
        assert!(!layers_compatible("0-35|36-47", "0-47"));
        assert!(!layers_compatible("whole", "0-30|36-47"));
        assert!(!layers_compatible("whole", "1-35|36-47"));
        assert!(layers_compatible("whole", "whole"));
    }
}
