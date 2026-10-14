//! The conversation sessions of a plugin engine across the machines that serve a whole model.
//!
//! A turn names its conversation (`session` in the chat request). The machine that ran the turn saves the
//! conversation in its own engine (the plugin's bridge does, before it writes the turn's `done` line) and reports the
//! save there. That machine then sends the turn to every other machine that serves the whole model: the turn's delta
//! and its tokens, which the peer applies to the whole session it holds for the conversation. A peer that no longer
//! holds the delta's base gets the whole session instead. Each machine restores the longest saved start of a prompt
//! itself, so the gateway's choice of machine does not change.
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
use std::time::Duration;
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

/// The conversation a turn saved, from the `done` object of its answer (the bridge writes `session` only when it saved).
pub(crate) fn saved_session(done: &Value) -> Option<String> {
    let session = done.get("session")?;
    if session["saved"].is_null() {
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
    fn only_a_saved_turn_names_its_session() {
        let saved = json!({"session": {"key": "c1", "saved": {"ids": [1, 2], "delta": true}}});
        assert_eq!(saved_session(&saved), Some("c1".into()));
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
