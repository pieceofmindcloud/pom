//! What the network was asked to generate, and on whose credential.
//!
//! The Logs screen answers two questions that nothing else in this process
//! could: which token paid for a request, and which machines actually ran it.
//! Both are known at the moment a generation finishes — the credential from the
//! `Authorization` header, the hops from the pipeline plan — so they are
//! recorded there and nowhere else.
//!
//! The log is **in memory and bounded**. It is an operator's rear-view mirror,
//! not an audit trail: a node that restarts starts with an empty one, and only
//! the most recent [`CAPACITY`] requests are kept. Writing it to disk would mean
//! deciding on retention, rotation and privacy, none of which anybody has asked
//! for yet.

use axum::{extract::Query, response::IntoResponse, Json};
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use tokio::sync::RwLock;

/// Requests kept before the oldest is dropped.
const CAPACITY: usize = 500;

/// Who asked. Built by the auth extractor, which is the only place that sees
/// the credential.
#[derive(Debug, Clone)]
pub struct Caller {
    /// Label of the API key, or "chave de conexão" for the operator credential.
    pub token_label: String,
    /// Last characters of the token. The token itself never enters the log.
    pub token_hint: String,
    /// Node the key is assigned to, when it is assigned to one.
    pub token_node_id: Option<String>,
}

impl Caller {
    /// The cluster's own operator credential, which also opens the `/v1` routes.
    pub fn operator() -> Self {
        Self {
            token_label: "chave de conexão".to_string(),
            token_hint: String::new(),
            token_node_id: None,
        }
    }

    /// Requests that arrived on a route with no credential at all
    /// (`POST /api/generate`, which has never been behind auth).
    pub fn anonymous() -> Self {
        Self {
            token_label: "sem token".to_string(),
            token_hint: String::new(),
            token_node_id: None,
        }
    }

    /// Never show more than the tail of a secret.
    pub fn hint_of(token: &str) -> String {
        let tail: String = token
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    pub at: String,
    /// The HTTP route the request came in on.
    pub route: String,
    pub model: String,
    pub token_label: String,
    pub token_hint: String,
    pub token_node_id: Option<String>,
    /// Node that received the request and planned the pipeline.
    pub entry_node_id: String,
    /// Node ids the generation actually went through, in order.
    pub hops: Vec<String>,
    /// The same string the API returns as `x_rdma_p2p.source`.
    pub source: String,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub duration_ms: Option<u64>,
    /// "ok" | "error"
    pub status: &'static str,
    pub error: Option<String>,
}

fn log() -> &'static RwLock<VecDeque<Entry>> {
    static LOG: std::sync::OnceLock<RwLock<VecDeque<Entry>>> = std::sync::OnceLock::new();
    LOG.get_or_init(|| RwLock::new(VecDeque::with_capacity(CAPACITY)))
}

/// Record one finished generation. Never fails: a full log drops its oldest.
pub async fn record(entry: Entry) {
    let mut q = log().write().await;
    if q.len() >= CAPACITY {
        q.pop_front();
    }
    q.push_back(entry);
}

#[derive(Debug, serde::Deserialize)]
pub struct LogQuery {
    /// Only requests that touched this node.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Only requests paid for by a token whose hint or label matches.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /api/ui/requests` — newest first, with a per-token and per-node tally
/// so the screen does not have to add up a truncated list.
pub async fn list_requests(Query(q): Query<LogQuery>) -> impl IntoResponse {
    let all = log().read().await;

    let matches = |e: &Entry| {
        q.node_id
            .as_ref()
            .is_none_or(|n| e.entry_node_id == *n || e.hops.iter().any(|h| h == n))
            && q.token.as_ref().is_none_or(|t| {
                e.token_label.contains(t.as_str()) || e.token_hint.contains(t.as_str())
            })
    };

    let filtered: Vec<&Entry> = all.iter().rev().filter(|e| matches(e)).collect();

    // Totals are computed over everything that matched, not over the page, so
    // "12 requisições neste token" stays true as the list grows.
    let mut by_token: HashMap<&str, u64> = HashMap::new();
    let mut by_node: HashMap<&str, u64> = HashMap::new();
    let mut errors = 0u64;
    for e in &filtered {
        *by_token.entry(e.token_label.as_str()).or_default() += 1;
        for hop in &e.hops {
            *by_node.entry(hop.as_str()).or_default() += 1;
        }
        if e.status != "ok" {
            errors += 1;
        }
    }

    let limit = q.limit.unwrap_or(200).min(CAPACITY);
    let entries: Vec<&Entry> = filtered.iter().take(limit).copied().collect();

    Json(json!({
        "entries": entries,
        "total": filtered.len(),
        "errors": errors,
        "by_token": by_token,
        "by_node": by_node,
        "capacity": CAPACITY,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_shows_the_tail_and_nothing_else() {
        let hint = Caller::hint_of("sk-4f2c9a70-1b3d-4e58-9c07-2a6f8b1d4e30");
        assert_eq!(hint, "…4e30");
        assert!(!hint.contains("sk-"));
    }

    #[test]
    fn a_short_token_does_not_panic_or_leak_more_than_it_has() {
        assert_eq!(Caller::hint_of("ab"), "…ab");
        assert_eq!(Caller::hint_of(""), "…");
    }

    #[tokio::test]
    async fn the_log_forgets_the_oldest_instead_of_growing() {
        for i in 0..(CAPACITY + 10) {
            record(Entry {
                id: i.to_string(),
                at: chrono::Utc::now().to_rfc3339(),
                route: "/v1/chat/completions".into(),
                model: "m".into(),
                token_label: "t".into(),
                token_hint: "…abcd".into(),
                token_node_id: None,
                entry_node_id: "n".into(),
                hops: vec!["n".into()],
                source: "pipeline:n".into(),
                prompt_tokens: None,
                completion_tokens: None,
                duration_ms: None,
                status: "ok",
                error: None,
            })
            .await;
        }
        assert_eq!(log().read().await.len(), CAPACITY);
    }
}
