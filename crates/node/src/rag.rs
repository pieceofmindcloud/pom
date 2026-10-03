//! Transparent retrieval: the gateway asks the network's RAG server for
//! passages related to the user's question and adds them to the prompt.
//!
//! The RAG server is whatever plugin, on whatever machine, publishes a ready
//! service of kind `rag` (`plugin_services.rs`); the gateway knows no plugin
//! by name. It calls that plugin's `peer.search` operation - locally when the
//! plugin runs on this node, through the signed peer route otherwise - and the
//! plugin embeds the question with the model that embedded the documents.
//!
//! Retrieval is an optimization of the answer, never a condition of the
//! request: no server, a slow server or a refusal all mean the prompt goes out
//! as it came.
//!
//! **Where the passages go matters for the prefix cache.** They are appended
//! after the whole conversation, and the head is told to retain the KV only up
//! to where the conversation ends (`retain_prefix_bytes`). The next turn's
//! prompt starts with that same conversation, so its prefix is reused; the
//! passages, which change with every question, are never part of what is kept.

use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SERVICE_KIND: &str = "rag";
/// How long the choice of server is trusted before the network is asked again.
const DIRECTORY_TTL: Duration = Duration::from_secs(15);
const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(800);
/// A search slower than this is dropped and the request goes on without it.
/// Retrieval sits in front of every chat request, so the default is a few
/// hundred milliseconds; `POM_RAG_TIMEOUT_MS` changes it.
const DEFAULT_SEARCH_TIMEOUT_MS: u64 = 300;
const SEARCH_TIMEOUT_RANGE_MS: std::ops::RangeInclusive<u64> = 20..=5000;
const MAX_QUESTION_CHARS: usize = 2000;
const MAX_PASSAGE_CHARS: usize = 4000;
const DEFAULT_PASSAGES: usize = 4;
/// Cosine similarity of the multilingual E5 models sits high even for
/// unrelated text; below this a passage is noise more often than help.
const DEFAULT_MIN_SCORE: f64 = 0.8;

/// A machine whose plugin answers `peer.search`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Provider {
    pub node_id: String,
    pub plugin_code: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Passage {
    pub path: String,
    pub text: String,
    pub score: f64,
}

/// What a retrieval added to a request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Retrieved {
    pub provider: Provider,
    pub passages: Vec<Passage>,
}

impl Retrieved {
    /// For the request audit and the log: where the context came from, never
    /// the text.
    pub(crate) fn summary(&self) -> Value {
        json!({
            "node_id": self.provider.node_id,
            "plugin": self.provider.plugin_code,
            "passages": self.passages.iter().map(|passage| json!({
                "path": passage.path,
                "score": (passage.score * 1000.0).round() / 1000.0,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Settings {
    pub enabled: bool,
    pub passages: usize,
    pub min_score: f64,
    pub timeout: Duration,
}

/// `POM_RAG=0` turns retrieval off on this gateway; `POM_RAG_PASSAGES`,
/// `POM_RAG_MIN_SCORE` and `POM_RAG_TIMEOUT_MS` tune it.
pub(crate) fn settings() -> Settings {
    let read = |name: &str| crate::brand::env(name).ok();
    settings_from(
        read("RAG").as_deref(),
        read("RAG_PASSAGES").as_deref(),
        read("RAG_MIN_SCORE").as_deref(),
        read("RAG_TIMEOUT_MS").as_deref(),
    )
}

fn settings_from(
    enabled: Option<&str>,
    passages: Option<&str>,
    min_score: Option<&str>,
    timeout_ms: Option<&str>,
) -> Settings {
    Settings {
        enabled: !matches!(
            enabled
                .map(|value| value.trim().to_ascii_lowercase())
                .as_deref(),
            Some("0" | "false" | "no" | "off")
        ),
        passages: passages
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|count| (1..=20).contains(count))
            .unwrap_or(DEFAULT_PASSAGES),
        min_score: min_score
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|score| (0.0..=1.0).contains(score))
            .unwrap_or(DEFAULT_MIN_SCORE),
        timeout: Duration::from_millis(
            timeout_ms
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|millis| SEARCH_TIMEOUT_RANGE_MS.contains(millis))
                .unwrap_or(DEFAULT_SEARCH_TIMEOUT_MS),
        ),
    }
}

/// The first ready `rag` service of a node's service list.
pub(crate) fn rag_plugin(services: &[crate::plugin_services::PluginService]) -> Option<&str> {
    services
        .iter()
        .find(|service| service.kind == SERVICE_KIND && service.status == "ready")
        .map(|service| service.plugin_code.as_str())
}

/// Reads the passages of a `peer.search` answer, dropping what is below
/// `min_score` or is not a passage. The order is the plugin's: it ranks with
/// more than the similarity it reports (words as well as meaning), so the
/// first passage is its best one even when another has a higher score.
pub(crate) fn parse_passages(answer: &Value, settings: Settings) -> Vec<Passage> {
    let mut passages: Vec<Passage> = answer["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|hit| {
            let text = hit["text"].as_str()?.trim();
            let score = hit["score"].as_f64()?;
            (!text.is_empty() && score >= settings.min_score).then(|| Passage {
                path: hit["path"].as_str().unwrap_or_default().to_owned(),
                text: text.chars().take(MAX_PASSAGE_CHARS).collect(),
                score,
            })
        })
        .collect();
    passages.truncate(settings.passages);
    passages
}

/// The text appended to the prompt. It opens with a blank line so it reads as
/// its own block after the last turn.
pub(crate) fn context_block(passages: &[Passage]) -> String {
    let mut block = String::from(
        "\n\nRetrieved context: passages from the knowledge base that may help answer the last \
         user message. Use them when they are relevant, ignore them when they are not, and do \
         not mention this block.",
    );
    for (index, passage) in passages.iter().enumerate() {
        block.push_str(&format!(
            "\n\n[{}] {}\n{}",
            index + 1,
            passage.path,
            passage.text
        ));
    }
    block
}

/// The question to search for: the trimmed text, bounded.
pub(crate) fn search_text(question: &str) -> Option<String> {
    let question = question.trim();
    (!question.is_empty()).then(|| question.chars().take(MAX_QUESTION_CHARS).collect())
}

struct Directory {
    provider: Option<Provider>,
    refreshed: Option<Instant>,
}

fn directory() -> &'static Mutex<Directory> {
    static DIRECTORY: OnceLock<Mutex<Directory>> = OnceLock::new();
    DIRECTORY.get_or_init(|| {
        Mutex::new(Directory {
            provider: None,
            refreshed: None,
        })
    })
}

fn lock_directory() -> std::sync::MutexGuard<'static, Directory> {
    directory()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Asks this node and then its peers who serves retrieval.
async fn discover(state: &crate::gateway::GatewayState) -> Option<Provider> {
    let local = crate::plugin_services::node_services(&state.plugins).await;
    if let Some(code) = rag_plugin(&local) {
        return Some(Provider {
            node_id: state.network.node_id.clone(),
            plugin_code: code.to_owned(),
        });
    }
    let mut peers: Vec<_> = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .filter(|peer| peer.node_id != state.network.node_id && peer.gateway_port.is_some())
        .collect();
    peers.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    let answers = futures::future::join_all(peers.iter().map(|peer| async move {
        let response = state
            .control_get(peer, "/api/internal/plugins/services", DISCOVERY_TIMEOUT)
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body: Value = response.json().await.ok()?;
        let code = body["services"].as_array()?.iter().find_map(|service| {
            (service["kind"] == SERVICE_KIND && service["status"] == "ready")
                .then(|| service["plugin_code"].as_str())
                .flatten()
        })?;
        Some(Provider {
            node_id: peer.node_id.clone(),
            plugin_code: code.to_owned(),
        })
    }))
    .await;
    answers.into_iter().flatten().next()
}

/// The network's RAG server, from a short-lived cache. A request never waits
/// for the network to find it: a stale or empty cache answers with what it
/// has and is refreshed behind the request. Only this node's own plugins are
/// asked in line, which costs nothing, so a gateway that is the RAG server
/// retrieves from its first request.
async fn provider(state: &crate::gateway::GatewayState) -> Option<Provider> {
    let (known, fresh) = {
        let directory = lock_directory();
        (
            directory.provider.clone(),
            directory
                .refreshed
                .is_some_and(|at| at.elapsed() < DIRECTORY_TTL),
        )
    };
    if fresh {
        return known;
    }
    // Claimed before the refresh starts, so one request refreshes.
    lock_directory().refreshed = Some(Instant::now());
    let background = state.clone();
    tokio::spawn(async move {
        let found = discover(&background).await;
        lock_directory().provider = found;
    });
    if known.is_some() {
        return known;
    }
    let local = crate::plugin_services::node_services(&state.plugins).await;
    rag_plugin(&local).map(|code| Provider {
        node_id: state.network.node_id.clone(),
        plugin_code: code.to_owned(),
    })
}

async fn search(
    state: &crate::gateway::GatewayState,
    provider: &Provider,
    question: &str,
    settings: Settings,
) -> Option<Value> {
    // More than needed is asked for: the score filter drops some.
    let params = json!({ "query": question, "limit": settings.passages * 2 });
    if provider.node_id == state.network.node_id {
        // The plugin is on this machine: the same operation, without the hop.
        let caller = json!({ "node_id": state.network.node_id, "head": true, "local": true });
        return state
            .plugins
            .query_audit(
                &provider.plugin_code,
                "peer.search",
                json!({ "params": params, "caller": caller }),
            )
            .await;
    }
    let peer = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == provider.node_id)?;
    let path = format!("/api/internal/plugins/{}/peer/search", provider.plugin_code);
    // The measured route, not every announced address: a peer behind NAT is
    // reached by its tunnel at once instead of after its dead addresses.
    let response = state
        .control_post_json_routed(&peer, &path, &params, settings.timeout)
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// The passages for `question`, or `None` when retrieval is off, there is no
/// RAG server, it did not answer in time, or nothing relevant was found.
pub(crate) async fn retrieve(
    state: &crate::gateway::GatewayState,
    question: &str,
) -> Option<Retrieved> {
    let settings = settings();
    if !settings.enabled {
        return None;
    }
    let question = search_text(question)?;
    let provider = provider(state).await?;
    let answer = match tokio::time::timeout(
        settings.timeout,
        search(state, &provider, &question, settings),
    )
    .await
    {
        Ok(Some(answer)) => answer,
        Ok(None) | Err(_) => {
            // The server may have moved or stopped: ask again next time.
            lock_directory().refreshed = None;
            tracing::debug!(node_id = %provider.node_id, "o servidor de RAG não respondeu à busca");
            return None;
        }
    };
    let passages = parse_passages(&answer, settings);
    (!passages.is_empty()).then_some(Retrieved { provider, passages })
}

/// Whether a chat request sent now would be given retrieved context: retrieval
/// is on and the network has a RAG server. The Chat offers its RAG switch only
/// then.
pub(crate) async fn available(state: &crate::gateway::GatewayState) -> bool {
    settings().enabled && provider(state).await.is_some()
}

/// `GET /api/internal/plugins/services`: what this node's plugins serve, for
/// a peer choosing where to send a search.
pub(crate) async fn list_services(
    _auth: crate::auth::InternalNodeAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::Json<Value> {
    let services = crate::plugin_services::node_services(&state.plugins).await;
    axum::Json(json!({ "services": services }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Settings {
        settings_from(None, None, None, None)
    }

    #[test]
    fn retrieval_is_on_by_default_and_tuned_by_the_environment() {
        assert_eq!(
            defaults(),
            Settings {
                enabled: true,
                passages: DEFAULT_PASSAGES,
                min_score: DEFAULT_MIN_SCORE,
                timeout: Duration::from_millis(DEFAULT_SEARCH_TIMEOUT_MS),
            }
        );
        for off in ["0", "false", " OFF ", "no"] {
            assert!(!settings_from(Some(off), None, None, None).enabled, "{off}");
        }
        assert!(settings_from(Some("1"), None, None, None).enabled);
        let tuned = settings_from(None, Some("8"), Some("0.9"), Some("120"));
        assert_eq!((tuned.passages, tuned.min_score), (8, 0.9));
        assert_eq!(tuned.timeout, Duration::from_millis(120));
        // Out of range or unreadable values fall back instead of breaking.
        let bad = settings_from(None, Some("0"), Some("7"), Some("60000"));
        assert_eq!(
            (bad.passages, bad.min_score),
            (DEFAULT_PASSAGES, DEFAULT_MIN_SCORE)
        );
        assert_eq!(
            bad.timeout,
            Duration::from_millis(DEFAULT_SEARCH_TIMEOUT_MS)
        );
    }

    #[test]
    fn only_relevant_passages_are_kept_in_the_plugins_order() {
        let answer = json!({"hits": [
            {"path": "b.md", "chunk": 0, "text": "second", "score": 0.84},
            {"path": "a.md", "chunk": 1, "text": " first ", "score": 0.91},
            {"path": "c.md", "chunk": 0, "text": "noise", "score": 0.61},
            {"path": "d.md", "chunk": 0, "text": "  ", "score": 0.99},
            {"path": "e.md", "chunk": 0, "score": 0.99},
        ]});
        let passages = parse_passages(&answer, defaults());
        let paths: Vec<&str> = passages
            .iter()
            .map(|passage| passage.path.as_str())
            .collect();
        // "b.md" stays first: the plugin ranked it there.
        assert_eq!(paths, ["b.md", "a.md"]);
        assert_eq!(passages[1].text, "first");

        let one = parse_passages(
            &answer,
            Settings {
                passages: 1,
                ..defaults()
            },
        );
        assert_eq!(one.len(), 1);
        assert!(parse_passages(&json!({"error": "refused"}), defaults()).is_empty());
    }

    #[test]
    fn the_context_block_is_its_own_paragraph_and_names_each_source() {
        let block = context_block(&[
            Passage {
                path: "manual.pdf".into(),
                text: "Warranty is two years.".into(),
                score: 0.9,
            },
            Passage {
                path: "faq.md".into(),
                text: "Returns take ten days.".into(),
                score: 0.85,
            },
        ]);
        assert!(block.starts_with("\n\nRetrieved context"));
        assert!(block.contains("\n\n[1] manual.pdf\nWarranty is two years."));
        assert!(block.contains("\n\n[2] faq.md\nReturns take ten days."));
    }

    #[test]
    fn the_summary_names_sources_and_never_carries_the_text() {
        let retrieved = Retrieved {
            provider: Provider {
                node_id: "node-a".into(),
                plugin_code: "qdrant".into(),
            },
            passages: vec![Passage {
                path: "manual.pdf".into(),
                text: "secret text".into(),
                score: 0.87654,
            }],
        };
        let summary = retrieved.summary();
        assert_eq!(summary["node_id"], "node-a");
        assert_eq!(
            summary["passages"][0],
            json!({"path": "manual.pdf", "score": 0.877})
        );
        assert!(!summary.to_string().contains("secret text"));
    }

    #[test]
    fn the_question_is_trimmed_and_bounded() {
        assert_eq!(
            search_text("  what is it?  ").as_deref(),
            Some("what is it?")
        );
        assert_eq!(search_text("   "), None);
        let long = "é".repeat(MAX_QUESTION_CHARS + 50);
        assert_eq!(
            search_text(&long).unwrap().chars().count(),
            MAX_QUESTION_CHARS
        );
    }

    #[test]
    fn only_a_ready_rag_service_is_a_server() {
        let service = |kind: &str, status: &str| crate::plugin_services::PluginService {
            plugin_code: "qdrant".into(),
            id: "rag".into(),
            kind: kind.into(),
            name: "Qdrant".into(),
            status: status.into(),
            version: None,
            detail: None,
        };
        assert_eq!(rag_plugin(&[service("rag", "ready")]), Some("qdrant"));
        assert_eq!(rag_plugin(&[service("rag", "starting")]), None);
        assert_eq!(rag_plugin(&[service("search", "ready")]), None);
        assert_eq!(rag_plugin(&[]), None);
    }
}
