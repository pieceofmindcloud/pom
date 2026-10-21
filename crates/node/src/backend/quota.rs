//! Token budgets, checked before a request enters the pipeline.
//!
//! The POM owns no budget: the plans, the windows and the consumption live in
//! the logs plugin of an admin, which already holds the cluster's request log
//! (`peer.quota.*` operations, see the plugin's `src/quota.rs`). The node that
//! receives a request asks for a verdict for the node the request belongs to
//! (the API key's node, else this one): first its own logs plugin, if it runs
//! one, then each live admin through the signed peer call
//! (`/api/internal/plugins/logs/peer/quota.check`). The first answer from a
//! plugin that has budgets configured decides. A machine without the plugin
//! is skipped; one that runs it and does not answer makes a "no budget" answer
//! provisional (not cached, `pending` on the screen).
//!
//! Fail-open by design: with no plugin answering, the request goes on (and a
//! warning is logged), except for a node this one already saw refused, which
//! stays refused until the time that refusal named. A verdict the plugin marks
//! as independent of the request (`cache_ms`) is reused for that long, so a
//! cluster without budgets pays one lookup a minute, not one per request.
//!
//! `GET /api/ui/quota`, `POST /api/ui/quota/reset` and
//! `POST /api/ui/quota/request` are this node's own budget screen: its
//! limits, spending one session-reset credit, and asking the admins for more.
use crate::gateway::GatewayState;
use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PEER_TIMEOUT: Duration = Duration::from_millis(1500);
/// The peer route caps a body at 256 KiB; the check stays well under it.
const MAX_DETAILS_BYTES: usize = 160 * 1024;
const MAX_TEXT_CHARS: usize = 16 * 1024;
const MAX_PROMPT_CHARS: usize = 32 * 1024;
const KEPT_TURNS: usize = 6;

/// A refused request: the message for the client and when to come back.
#[derive(Debug, Clone)]
pub(crate) struct Denial {
    pub message: String,
    pub retry_after_secs: Option<u64>,
}

impl Denial {
    /// `Retry-After` on the refusal, when the plugin knows when.
    pub(crate) fn decorate(&self, mut response: Response) -> Response {
        if let Some(seconds) = self.retry_after_secs {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

#[derive(Clone)]
enum Cached {
    Allowed,
    Denied(Denial),
}

struct Memory {
    verdicts: HashMap<String, (Instant, Cached)>,
    /// Refusals with a known end, kept for the fail-open path.
    blocked_until: HashMap<String, (Instant, Denial)>,
    last_unreachable_warning: Option<Instant>,
}

fn memory() -> &'static Mutex<Memory> {
    static MEMORY: std::sync::OnceLock<Mutex<Memory>> = std::sync::OnceLock::new();
    MEMORY.get_or_init(|| {
        Mutex::new(Memory {
            verdicts: HashMap::new(),
            blocked_until: HashMap::new(),
            last_unreachable_warning: None,
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, Memory> {
    memory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Where a budget answer came from: this node's plugin or an admin's.
enum Source {
    Local,
    Peer(crate::network::PeerInfo),
}

/// What one source said: an answer, that its logs plugin does not run, or
/// nothing (a timeout, a refusal, no route). Telling the last two apart is what
/// keeps a slow admin from reading as "no budget here".
enum Reply {
    Answer(Value),
    Absent,
    Failed,
}

async fn ask_source(state: &GatewayState, source: &Source, op: &str, params: &Value) -> Reply {
    match source {
        Source::Local => {
            let running = state
                .plugins
                .status(crate::plugins::LOGS_PLUGIN_CODE)
                .await
                .is_some_and(|status| status.active);
            if !running {
                return Reply::Absent;
            }
            state
                .plugins
                .query_audit(
                    crate::plugins::LOGS_PLUGIN_CODE,
                    &format!("peer.quota.{op}"),
                    json!({ "params": params, "caller": { "node_id": state.network.node_id } }),
                )
                .await
                .map_or(Reply::Failed, Reply::Answer)
        }
        Source::Peer(peer) => {
            let path = format!(
                "/api/internal/plugins/{}/peer/quota.{op}",
                crate::plugins::LOGS_PLUGIN_CODE
            );
            let Ok(response) = state.control_post_json(peer, &path, params, PEER_TIMEOUT).await else {
                return Reply::Failed;
            };
            if response.status() == StatusCode::NOT_FOUND {
                return Reply::Absent;
            }
            if !response.status().is_success() {
                return Reply::Failed;
            }
            response
                .json::<Value>()
                .await
                .map_or(Reply::Failed, Reply::Answer)
        }
    }
}

/// This node's plugin first, then every live admin.
async fn sources(state: &GatewayState) -> Vec<Source> {
    let mut sources = Vec::new();
    if state
        .plugins
        .runtime_for(crate::plugins::LOGS_PLUGIN_CODE)
        .is_some()
    {
        sources.push(Source::Local);
    }
    let mut admins: Vec<_> = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .filter(|peer| {
            peer.node_id != state.network.node_id
                && peer.profile == crate::profile::NodeProfile::Admin
        })
        .collect();
    admins.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    sources.extend(admins.into_iter().map(Source::Peer));
    sources
}

/// The outcome of asking every source.
struct Lookup {
    /// The first plugin answer with budgets configured; else the last answer
    /// that said there are none; `None` when no plugin answered at all.
    found: Option<(Source, Value)>,
    /// Some source that runs the plugin did not answer: a "none" may then be
    /// wrong, since that source could hold the budgets.
    missed: bool,
}

impl Lookup {
    fn configured(&self) -> bool {
        self.found
            .as_ref()
            .is_some_and(|(_, reply)| reply.get("configured").and_then(Value::as_bool) == Some(true))
    }
}

async fn lookup(state: &GatewayState, op: &str, params: &Value) -> Lookup {
    let mut unconfigured = None;
    let mut missed = false;
    for source in sources(state).await {
        let reply = match ask_source(state, &source, op, params).await {
            Reply::Answer(reply) => reply,
            Reply::Absent => continue,
            Reply::Failed => {
                missed = true;
                continue;
            }
        };
        if reply.get("error").is_some() {
            missed = true;
            continue;
        }
        if reply.get("configured").and_then(Value::as_bool) == Some(true) {
            return Lookup {
                found: Some((source, reply)),
                missed,
            };
        }
        unconfigured = Some((source, reply));
    }
    Lookup {
        found: unconfigured,
        missed,
    }
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => text[..at].to_owned(),
        None => text.to_owned(),
    }
}

fn shrink_content(content: &Value) -> Value {
    match content {
        Value::String(text) => Value::String(truncate(text, MAX_TEXT_CHARS)),
        Value::Array(parts) => Value::Array(
            parts
                .iter()
                .filter_map(|part| {
                    let text = part.get("text").and_then(Value::as_str)?;
                    Some(json!({ "type": "text", "text": truncate(text, MAX_TEXT_CHARS) }))
                })
                .collect(),
        ),
        _ => Value::Null,
    }
}

/// What the plugin needs to tell the request's project (the working
/// directory and repository coding tools put in the system prompt, the
/// newest turns), cut to fit the peer call.
pub(crate) fn digest_details(details: &Value) -> Value {
    if serde_json::to_vec(details).map(|bytes| bytes.len()).unwrap_or(usize::MAX) <= MAX_DETAILS_BYTES {
        return details.clone();
    }
    let mut digest = serde_json::Map::new();
    if let Some(system) = details.get("system") {
        digest.insert("system".into(), shrink_content(system));
    }
    if let Some(messages) = details.get("messages").and_then(Value::as_array) {
        let is_system = |message: &&Value| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        };
        let mut kept: Vec<Value> = messages
            .iter()
            .filter(is_system)
            .map(|message| json!({ "role": message["role"], "content": shrink_content(&message["content"]) }))
            .collect();
        let turns: Vec<&Value> = messages.iter().filter(|message| !is_system(message)).collect();
        for message in turns.iter().skip(turns.len().saturating_sub(KEPT_TURNS)) {
            kept.push(json!({ "role": message["role"], "content": shrink_content(&message["content"]) }));
        }
        digest.insert("messages".into(), Value::Array(kept));
    }
    Value::Object(digest)
}

fn denial_of(reply: &Value) -> Denial {
    Denial {
        message: reply
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Token budget exhausted.")
            .to_owned(),
        retry_after_secs: reply
            .get("retry_after_secs")
            .and_then(Value::as_i64)
            .map(|seconds| seconds.max(0) as u64),
    }
}

/// May a request of `caller` go to the pipeline? `None` lets it through.
pub(crate) async fn admit(
    state: &GatewayState,
    caller: &crate::reqlog::Caller,
    prompt: &str,
    prompt_tokens: u32,
    request_details: &Value,
) -> Option<Denial> {
    let node = caller
        .key_node_id
        .clone()
        .filter(|node| !node.is_empty())
        .unwrap_or_else(|| state.network.node_id.clone());
    {
        let mut memory = lock();
        let now = Instant::now();
        memory.verdicts.retain(|_, (until, _)| *until > now);
        memory.blocked_until.retain(|_, (until, _)| *until > now);
        if let Some((_, cached)) = memory.verdicts.get(&node) {
            return match cached {
                Cached::Allowed => None,
                Cached::Denied(denial) => Some(denial.clone()),
            };
        }
    }
    let params = json!({
        "node_id": node,
        "prompt_tokens": prompt_tokens,
        "prompt": truncate(prompt, MAX_PROMPT_CHARS),
        "key_label": caller.key_label,
        "request_details": digest_details(request_details),
    });
    let found = lookup(state, "check", &params).await;
    // A "no budget" while a source holding the plugin did not answer is not a
    // verdict to keep: that source may be the one with the budgets.
    let keep = found.configured() || !found.missed;
    let Some((_, reply)) = found.found else {
        let mut memory = lock();
        let warn = memory
            .last_unreachable_warning
            .is_none_or(|at| at.elapsed() > Duration::from_secs(60));
        if warn {
            memory.last_unreachable_warning = Some(Instant::now());
            tracing::warn!("token budget: no logs plugin answered, request allowed (fail-open)");
        }
        return memory
            .blocked_until
            .get(&node)
            .map(|(_, denial)| denial.clone());
    };
    let cache = reply
        .get("cache_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .filter(|cache| keep && !cache.is_zero());
    if reply.get("allowed").and_then(Value::as_bool) != Some(false) {
        let mut memory = lock();
        memory.blocked_until.remove(&node);
        if let Some(cache) = cache {
            memory
                .verdicts
                .insert(node, (Instant::now() + cache, Cached::Allowed));
        }
        return None;
    }
    let denial = denial_of(&reply);
    let mut memory = lock();
    if let Some(cache) = cache {
        memory.verdicts.insert(
            node.clone(),
            (Instant::now() + cache, Cached::Denied(denial.clone())),
        );
        if let Some(seconds) = denial.retry_after_secs {
            memory.blocked_until.insert(
                node,
                (Instant::now() + Duration::from_secs(seconds), denial.clone()),
            );
        }
    }
    Some(denial)
}

/// Forget cached verdicts of this node, after it changed its own budget.
fn forget(node: &str) {
    let mut memory = lock();
    memory.verdicts.remove(node);
    memory.blocked_until.remove(node);
}

fn unavailable() -> Response {
    Json(json!({ "available": false, "configured": false })).into_response()
}

/// `GET /api/ui/quota`: this node's budget. `available: false` means no
/// reachable machine runs the logs plugin, so there is no budget screen at all;
/// `pending: true` means a machine that runs it did not answer, so "no budget"
/// is not known yet and the screen asks again.
pub(crate) async fn my_quota(State(state): State<GatewayState>) -> Response {
    let found = lookup(&state, "status", &json!({})).await;
    let pending = found.missed && !found.configured();
    match found.found {
        Some((_, mut reply)) => {
            reply["available"] = json!(true);
            if pending {
                reply["pending"] = json!(true);
            }
            Json(reply).into_response()
        }
        None if pending => {
            Json(json!({ "available": true, "configured": false, "pending": true })).into_response()
        }
        None => unavailable(),
    }
}

/// Sends a self-serve operation to the plugin that holds the budgets.
async fn self_serve(state: &GatewayState, op: &str, params: Value) -> Response {
    let Some((source, status)) = lookup(state, "status", &json!({})).await.found else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "quota_unavailable" })))
            .into_response();
    };
    if status.get("configured").and_then(Value::as_bool) != Some(true) {
        return (StatusCode::CONFLICT, Json(json!({ "error": "quota_not_configured" })))
            .into_response();
    }
    match ask_source(state, &source, op, &params).await {
        Reply::Answer(reply) if reply.get("error").is_some() => {
            (StatusCode::CONFLICT, Json(reply)).into_response()
        }
        Reply::Answer(mut reply) => {
            forget(&state.network.node_id);
            reply["available"] = json!(true);
            Json(reply).into_response()
        }
        Reply::Absent | Reply::Failed => {
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": "quota_unavailable" }))).into_response()
        }
    }
}

/// `POST /api/ui/quota/reset`: spends one session-reset credit.
pub(crate) async fn reset_my_session(State(state): State<GatewayState>) -> Response {
    self_serve(&state, "self_reset", json!({})).await
}

/// `POST /api/ui/quota/request` with `{note}`: asks the admins for more.
pub(crate) async fn request_more(
    State(state): State<GatewayState>,
    body: Option<Json<Value>>,
) -> Response {
    let note = body
        .and_then(|Json(body)| body.get("note").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    self_serve(&state, "request", json!({ "note": truncate(&note, 500) })).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_details_pass_whole_and_large_ones_keep_system_and_newest_turns() {
        let small = json!({"system": "Working directory: /x/pom", "messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(digest_details(&small), small);

        let big_text = "y".repeat(40 * 1024);
        let mut messages = vec![json!({"role": "system", "content": "cwd: /x/pom"})];
        for index in 0..20 {
            messages.push(json!({"role": if index % 2 == 0 { "user" } else { "assistant" }, "content": format!("{index}{big_text}")}));
        }
        let big = json!({"system": [{"type": "text", "text": "Working directory: /x/pom"}], "messages": messages, "tools": ["t"]});
        let digest = digest_details(&big);
        assert!(serde_json::to_vec(&digest).unwrap().len() <= MAX_DETAILS_BYTES);
        assert_eq!(digest["system"][0]["text"], "Working directory: /x/pom");
        let kept = digest["messages"].as_array().unwrap();
        assert_eq!(kept.len(), 1 + KEPT_TURNS);
        assert_eq!(kept[0]["content"], "cwd: /x/pom");
        assert!(kept.last().unwrap()["content"].as_str().unwrap().starts_with("19"));
        assert!(digest.get("tools").is_none());
    }

    #[tokio::test]
    async fn with_no_plugin_answering_requests_pass_except_for_a_node_already_refused() {
        let state = crate::gateway::testkit::state_with("gateway", vec![]).await;
        let caller = |node: &str| crate::reqlog::Caller {
            key_id: "k".into(),
            key_label: "k".into(),
            key_node_id: Some(node.into()),
        };
        assert!(admit(&state, &caller("quota-test-free"), "hi", 1, &json!({})).await.is_none());

        lock().blocked_until.insert(
            "quota-test-held".into(),
            (
                Instant::now() + Duration::from_secs(60),
                Denial { message: "spent".into(), retry_after_secs: Some(60) },
            ),
        );
        let denial = admit(&state, &caller("quota-test-held"), "hi", 1, &json!({})).await;
        assert_eq!(denial.map(|denial| denial.message).as_deref(), Some("spent"));
        forget("quota-test-held");
        assert!(admit(&state, &caller("quota-test-held"), "hi", 1, &json!({})).await.is_none());
    }

    #[test]
    fn a_denial_carries_the_retry_after_header() {
        let denial = denial_of(&json!({"message": "spent", "retry_after_secs": 120}));
        let response = denial.decorate(StatusCode::TOO_MANY_REQUESTS.into_response());
        assert_eq!(response.headers()[header::RETRY_AFTER], "120");
        assert_eq!(denial.message, "spent");
    }
}
