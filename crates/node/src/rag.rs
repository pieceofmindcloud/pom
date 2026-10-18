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
/// A passage is context for one answer, not a document: every character of it
/// is prompt the pipeline has to prefill on every turn.
const MAX_PASSAGE_CHARS: usize = 1500;
/// One passage by default. Text full of numbers costs close to a token per
/// character, and on a pipeline spread over a slow link each token of context
/// is paid in milliseconds; `POM_RAG_PASSAGES` raises it.
const DEFAULT_PASSAGES: usize = 1;
/// A message up to this size is its own search query. A longer one is first
/// reduced to its search terms.
const DEFAULT_REWRITE_MIN_CHARS: usize = 400;
/// How much of a long message the model reads to name the search terms: its
/// start and, mostly, its end, where the actual question usually is.
const REWRITE_HEAD_CHARS: usize = 400;
const REWRITE_TAIL_CHARS: usize = 1100;
const REWRITE_MAX_TOKENS: u32 = 32;
const REWRITE_TIMEOUT: Duration = Duration::from_secs(8);
/// Below this probability of "the documents would help", retrieval is skipped.
/// Low on purpose: the person turned retrieval on, so only a clear "no" stops
/// it.
const DEFAULT_DECISION_MIN: f64 = 0.3;
/// How much of the message the decision reads: its end, where the question
/// is. The decision is a generation of its own, so every character of this is
/// prefilled once more than the answer needs.
const DECISION_TAIL_CHARS: usize = 600;
/// Words a compacted query keeps.
const COMPACT_WORDS: usize = 24;
const MAX_QUERY_CHARS: usize = 300;
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
    /// What was searched for and how it was obtained.
    pub query: String,
    pub query_source: QuerySource,
    /// Probability the model gave to "this message needs documents", when it
    /// was asked.
    pub decision: Option<f64>,
    /// The collection the search was kept to, when the request named one.
    pub collection: Option<String>,
}

impl Retrieved {
    /// For the request audit and the log: where the context came from, never
    /// the text.
    pub(crate) fn summary(&self) -> Value {
        json!({
            "node_id": self.provider.node_id,
            "plugin": self.provider.plugin_code,
            "query": self.query,
            "query_source": self.query_source.as_str(),
            "collection": self.collection,
            "needed": self.decision.map(|yes| (yes * 1000.0).round() / 1000.0),
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
    /// Whether a long message may be reduced to search terms by the model
    /// (`POM_RAG_REWRITE=0` keeps it to the word filter).
    pub rewrite: bool,
    pub rewrite_min_chars: usize,
    /// Whether the model is first asked if the message needs documents at all.
    /// Off unless `POM_RAG_DECISION=1`: the decision is an extra pass through
    /// the pipeline, and it only pays for itself when a good share of the
    /// messages sent with retrieval on turn out not to need documents.
    pub decision: bool,
    pub decision_min: f64,
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
        read("RAG_REWRITE").as_deref(),
        read("RAG_REWRITE_MIN_CHARS").as_deref(),
        read("RAG_DECISION").as_deref(),
        read("RAG_DECISION_MIN").as_deref(),
    )
}

fn switched_off(value: Option<&str>) -> bool {
    matches!(
        value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

fn switched_on(value: Option<&str>) -> bool {
    matches!(
        value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn settings_from(
    enabled: Option<&str>,
    passages: Option<&str>,
    min_score: Option<&str>,
    timeout_ms: Option<&str>,
    rewrite: Option<&str>,
    rewrite_min_chars: Option<&str>,
    decision: Option<&str>,
    decision_min: Option<&str>,
) -> Settings {
    Settings {
        enabled: !switched_off(enabled),
        decision: switched_on(decision),
        decision_min: decision_min
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|probability| (0.0..=1.0).contains(probability))
            .unwrap_or(DEFAULT_DECISION_MIN),
        rewrite: !switched_off(rewrite),
        rewrite_min_chars: rewrite_min_chars
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|chars| (50..=20_000).contains(chars))
            .unwrap_or(DEFAULT_REWRITE_MIN_CHARS),
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

/// Words that carry no meaning for a search, in Portuguese and English.
const STOP_WORDS: &[&str] = &[
    "a", "à", "ao", "aos", "as", "às", "com", "como", "da", "das", "de", "dela", "dele", "do",
    "dos", "e", "é", "ela", "ele", "em", "entre", "era", "essa", "esse", "esta", "está", "este",
    "eu", "foi", "há", "isso", "isto", "já", "mais", "mas", "me", "meu", "minha", "na", "no",
    "nas", "no", "nos", "o", "os", "ou", "para", "pela", "pelo", "por", "qual", "quais", "quando",
    "quanto", "que", "quem", "se", "sem", "ser", "seu", "sua", "são", "também", "tem", "um", "uma",
    "você", "vocês", "about", "an", "and", "are", "at", "be", "been", "but", "by", "can", "do",
    "does", "for", "from", "had", "has", "have", "how", "i", "if", "in", "into", "is", "it", "its",
    "my", "not", "of", "on", "or", "our", "so", "than", "that", "the", "their", "then", "there",
    "these", "this", "to", "was", "we", "were", "what", "when", "where", "which", "who", "will",
    "with",
];

/// A long text reduced to search terms without asking anyone: stop words and
/// repeats leave, names, numbers and nouns stay, in the order they came. The
/// end of the text is read first, because that is where a question sits after
/// its preamble.
pub(crate) fn compact_query(text: &str) -> String {
    let words: Vec<&str> = text
        .split(|glyph: char| {
            !(glyph.is_alphanumeric() || matches!(glyph, '/' | '-' | '_' | '.' | ','))
        })
        .map(|word| word.trim_matches(|glyph: char| !glyph.is_alphanumeric()))
        .filter(|word| word.chars().count() >= 2)
        .collect();
    let mut seen: Vec<String> = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    for word in words.iter().rev() {
        let lower = word.to_lowercase();
        if STOP_WORDS.contains(&lower.as_str()) || seen.contains(&lower) {
            continue;
        }
        seen.push(lower);
        kept.push(word);
        if kept.len() == COMPACT_WORDS {
            break;
        }
    }
    kept.reverse();
    kept.join(" ")
}

/// The part of a long message the model reads to name the search terms.
pub(crate) fn rewrite_excerpt(message: &str) -> String {
    let glyphs: Vec<char> = message.trim().chars().collect();
    if glyphs.len() <= REWRITE_HEAD_CHARS + REWRITE_TAIL_CHARS {
        return glyphs.into_iter().collect();
    }
    let head: String = glyphs[..REWRITE_HEAD_CHARS].iter().collect();
    let tail: String = glyphs[glyphs.len() - REWRITE_TAIL_CHARS..].iter().collect();
    format!("{head}\n[...]\n{tail}")
}

/// The prompt of the short generation that turns a long message into a query.
pub(crate) fn rewrite_prompt(message: &str) -> String {
    format!(
        "You write search queries for a document index. Read the text below and reply with only \
         the search query that would find the documents needed to answer it: the names, dates, \
         numbers and key nouns, in the language of the text, at most 12 words, on one line, with \
         no explanation.\n\nText:\n{}\n\nSearch query:",
        rewrite_excerpt(message)
    )
}

/// The query in a model's reply: its first line, without quotes or a label,
/// bounded. `None` when the reply holds nothing usable.
pub(crate) fn rewritten_query(reply: &str) -> Option<String> {
    let line = reply
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .trim_start_matches("Search query:")
        .trim_matches(|glyph: char| {
            glyph.is_whitespace() || matches!(glyph, '"' | '\'' | '`' | '*')
        });
    let query: String = line.chars().take(MAX_QUERY_CHARS).collect();
    (query
        .chars()
        .filter(|glyph| glyph.is_alphanumeric())
        .count()
        >= 2)
        .then_some(query)
}

/// What a request that asked for retrieval lends to the query rewrite: the
/// identity and the model the short generation runs with.
pub(crate) struct Rewriter {
    pub model: String,
    pub inference_token: String,
    pub caller: crate::reqlog::Caller,
    pub preferred_head: Option<String>,
}

/// Asks the pipeline for the search terms of a long message. It is a
/// generation like any other, small and without thinking; whatever goes wrong
/// with it, the caller falls back to the word filter.
async fn rewrite_with_model(
    state: &crate::gateway::GatewayState,
    rewriter: &Rewriter,
    message: &str,
) -> Option<String> {
    let prompt = rewrite_prompt(message);
    let limit = state
        .request_output_limit_for_model(Some(&rewriter.model), REWRITE_MAX_TOKENS, &prompt)
        .await;
    if limit.rejection_reason().is_some() {
        return None;
    }
    let generation = crate::gateway::generate_blocking_with_stops(
        "/v1/rag-query",
        rewriter.caller.clone(),
        crate::gateway::GenerationInput {
            trace_id: Some(format!("rag-{}", uuid::Uuid::new_v4().simple())),
            state: state.clone(),
            inference_token: rewriter.inference_token.clone(),
            model: rewriter.model.clone(),
            prompt,
            images: Vec::new(),
            chat_template: true,
            thinking_enabled: false,
            budget: limit.budget(),
            stops: vec!["\n".to_owned()],
            // Its own short session: nothing of it is worth keeping.
            cache_key: None,
            preferred_head: rewriter.preferred_head.clone(),
            request_details: json!({ "type": "rag-query" }),
            probe: None,
        },
    );
    match tokio::time::timeout(REWRITE_TIMEOUT, generation).await {
        Ok(Ok((reply, _))) => rewritten_query(&reply),
        Ok(Err(error)) => {
            tracing::debug!(%error, "the RAG query generation failed");
            None
        }
        Err(_) => None,
    }
}

/// The typed question that decides whether a message needs documents: a JEV
/// yes-or-no decision, answered with one token whose probability the last
/// peer reports. Its head never changes, so the prefix cache keeps it and
/// each decision only prefills the message.
pub(crate) fn decision_question() -> crate::decisions::Question {
    crate::decisions::Question {
        id: "needs_documents".into(),
        instructions: "Would looking up the user's own stored documents (files, records, reports, \
                       manuals) help answer the message in the STATE? Answer Yes when it asks \
                       about specific facts, people, dates, amounts or content such documents \
                       could hold. Answer No for greetings, small talk, general knowledge, \
                       opinions, and writing or coding help that needs no stored document."
            .into(),
        kind: crate::decisions::Kind::Noul {
            yes: Some("the answer depends on stored documents".into()),
            no: Some("it can be answered without them".into()),
        },
    }
}

/// The end of the message, which is what the decision reads.
pub(crate) fn decision_excerpt(message: &str) -> String {
    let glyphs: Vec<char> = message.trim().chars().collect();
    let start = glyphs.len().saturating_sub(DECISION_TAIL_CHARS);
    glyphs[start..].iter().collect()
}

/// Asks the pipeline whether `message` needs documents. Returns the
/// probability of "yes", or `None` when the decision could not be made - and
/// then the search goes ahead, because the person asked for retrieval.
async fn needs_documents(
    state: &crate::gateway::GatewayState,
    rewriter: &Rewriter,
    message: &str,
) -> Option<f64> {
    let question = decision_question();
    let messages: Vec<crate::openai::ChatMessage> = serde_json::from_value(json!([
        {"role": "system", "content": crate::decisions::SYSTEM_MESSAGE},
        {"role": "user", "content": question.user_message(&json!(decision_excerpt(message)))},
    ]))
    .ok()?;
    let (prompt, _) = crate::openai::render_prompt_with_images(&messages, &[]).ok()?;
    // Keyed by the question alone: the message changes with every request.
    let retain_prefix_bytes = crate::decisions::reusable_prefix_end(&prompt, &question);
    let cache_key = retain_prefix_bytes.map(|end| prompt[..end].to_owned());
    let limit = state
        .request_output_limit_for_model(Some(&rewriter.model), 1, &prompt)
        .await;
    if limit.rejection_reason().is_some() {
        return None;
    }
    let labels = question.labels();
    let generation = crate::gateway::generate_blocking_with_stops(
        "/v1/rag-decision",
        rewriter.caller.clone(),
        crate::gateway::GenerationInput {
            trace_id: Some(format!("rag-{}", uuid::Uuid::new_v4().simple())),
            state: state.clone(),
            inference_token: rewriter.inference_token.clone(),
            model: rewriter.model.clone(),
            prompt,
            images: Vec::new(),
            chat_template: true,
            thinking_enabled: false,
            budget: limit.budget(),
            stops: Vec::new(),
            cache_key,
            preferred_head: rewriter.preferred_head.clone(),
            request_details: json!({ "type": "rag-decision" }),
            probe: Some(crate::prefill::DecisionProbe {
                labels: labels.clone(),
                retain_prefix_bytes,
                question: Some(question.id.clone()),
                question_type: Some(question.type_name().to_owned()),
                options: question.option_names(),
            }),
        },
    );
    let (_, summary) = tokio::time::timeout(REWRITE_TIMEOUT, generation)
        .await
        .ok()?
        .ok()?;
    yes_probability(summary.trace.as_ref()?.probe.as_ref()?, labels.len())
}

/// The probability of "yes" in the label probabilities the last peer reported.
pub(crate) fn yes_probability(probe: &Value, labels: usize) -> Option<f64> {
    let raw: Vec<f64> = probe["probabilities"]
        .as_array()?
        .iter()
        .map(|value| value.as_f64().unwrap_or(0.0))
        .collect();
    (raw.len() == labels && labels > 0).then(|| crate::decisions::normalized(&raw)[0])
}

/// What the retrieval step did for a request.
pub(crate) enum Outcome {
    Retrieved(Retrieved),
    /// The model judged that the message needs no documents.
    NotNeeded {
        probability: f64,
    },
    /// Retrieval is off, there is no server, or nothing relevant was found.
    Nothing,
}

/// How the search query of a request was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuerySource {
    /// The user's message, as written.
    Message,
    /// Search terms the model named for a long message.
    Model,
    /// A long message with stop words and repeats removed.
    Words,
}

impl QuerySource {
    fn as_str(self) -> &'static str {
        match self {
            QuerySource::Message => "message",
            QuerySource::Model => "model",
            QuerySource::Words => "words",
        }
    }
}

/// Decides what to search for. A short message is the query. A long one is
/// reduced: by the model when the request asked for retrieval and lent a
/// `Rewriter`, by the word filter otherwise or when the model does not answer.
async fn search_query(
    state: &crate::gateway::GatewayState,
    message: &str,
    rewriter: Option<&Rewriter>,
    settings: Settings,
) -> Option<(String, QuerySource)> {
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    if message.chars().count() <= settings.rewrite_min_chars {
        return Some((search_text(message)?, QuerySource::Message));
    }
    if let (true, Some(rewriter)) = (settings.rewrite, rewriter) {
        if let Some(query) = rewrite_with_model(state, rewriter, message).await {
            return Some((query, QuerySource::Model));
        }
    }
    let words = compact_query(message);
    (!words.is_empty()).then_some((words, QuerySource::Words))
}

struct Directory {
    provider: Option<Provider>,
    refreshed: Option<Instant>,
    /// The collections the server offers, and when they were read.
    collections: Vec<String>,
    collections_read: Option<Instant>,
}

fn directory() -> &'static Mutex<Directory> {
    static DIRECTORY: OnceLock<Mutex<Directory>> = OnceLock::new();
    DIRECTORY.get_or_init(|| {
        Mutex::new(Directory {
            provider: None,
            refreshed: None,
            collections: Vec::new(),
            collections_read: None,
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

/// A collection name as a request may give it: what the RAG plugin accepts,
/// so nothing else ever travels to it.
pub(crate) fn collection_name(name: Option<&str>) -> Option<String> {
    let name = name?.trim();
    let plain = (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|glyph| glyph.is_ascii_alphanumeric() || matches!(glyph, '_' | '-'));
    plain.then(|| name.to_owned())
}

async fn search(
    state: &crate::gateway::GatewayState,
    provider: &Provider,
    question: &str,
    collection: Option<&str>,
    settings: Settings,
) -> Option<Value> {
    // More than needed is asked for: the score filter drops some.
    let mut params = json!({ "query": question, "limit": settings.passages * 2 });
    if let Some(collection) = collection {
        params["collection"] = json!(collection);
    }
    peer_call(state, provider, "search", params, settings.timeout).await
}

/// The collections a request may keep its search to, for the Chat's picker.
/// Read from the RAG server at most every `DIRECTORY_TTL`; empty when there is
/// no server or it is an older plugin with a single collection.
pub(crate) async fn collections(state: &crate::gateway::GatewayState) -> Vec<String> {
    let settings = settings();
    if !settings.enabled {
        return Vec::new();
    }
    let Some(provider) = provider(state).await else {
        return Vec::new();
    };
    {
        let directory = lock_directory();
        if directory
            .collections_read
            .is_some_and(|read| read.elapsed() < DIRECTORY_TTL)
        {
            return directory.collections.clone();
        }
    }
    let answer = tokio::time::timeout(
        settings.timeout,
        peer_call(state, &provider, "collections", json!({}), settings.timeout),
    )
    .await
    .ok()
    .flatten();
    let names: Vec<String> = answer
        .as_ref()
        .and_then(|answer| answer["collections"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|name| collection_name(name.as_str()))
        .take(64)
        .collect();
    let mut directory = lock_directory();
    directory.collections = names.clone();
    directory.collections_read = Some(Instant::now());
    names
}

/// One `peer.<operation>` of the RAG plugin, on this machine or on a peer.
async fn peer_call(
    state: &crate::gateway::GatewayState,
    provider: &Provider,
    operation: &str,
    params: Value,
    timeout: Duration,
) -> Option<Value> {
    if provider.node_id == state.network.node_id {
        // The plugin is on this machine: the same operation, without the hop.
        let profile = state.auth.read().await.profile;
        let role = match state
            .admin_trust
            .effective_authority(&state.authority_principal, profile)
        {
            Some(crate::admin_trust::EffectiveAuthority::Principal) => "principal",
            Some(_) => "admin",
            None if profile == crate::profile::NodeProfile::Visitor => "visitor",
            None => "member",
        };
        let caller = json!({
            "node_id": state.network.node_id,
            "head": true,
            "local": true,
            "role": role,
        });
        return state
            .plugins
            .query_audit(
                &provider.plugin_code,
                &format!("peer.{operation}"),
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
    let path = format!(
        "/api/internal/plugins/{}/peer/{operation}",
        provider.plugin_code
    );
    // The measured route, not every announced address: a peer behind NAT is
    // reached by its tunnel at once instead of after its dead addresses.
    let response = state
        .control_post_json_routed(&peer, &path, &params, timeout)
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// Retrieval for one request: decides whether documents are needed, what to
/// search for, searches, and returns the passages. Any failure along the way
/// is `Outcome::Nothing` and the request goes on unchanged.
///
/// `rewriter` is lent by a request that asked for retrieval explicitly. Only
/// then may the step cost short generations of its own: the yes-or-no decision
/// and, for a long message, the search terms. The pipeline generates the
/// answer after all of this.
///
/// `collection` keeps the search to one collection of the server; without it
/// every collection a folder is linked to is searched.
pub(crate) async fn retrieve(
    state: &crate::gateway::GatewayState,
    question: &str,
    rewriter: Option<&Rewriter>,
    collection: Option<&str>,
) -> Outcome {
    let collection = collection_name(collection);
    let settings = settings();
    if !settings.enabled {
        return Outcome::Nothing;
    }
    // No server, no work: the short generations are never paid for nothing.
    let Some(provider) = provider(state).await else {
        return Outcome::Nothing;
    };
    let mut decision = None;
    if let (true, Some(rewriter)) = (settings.decision, rewriter) {
        decision = needs_documents(state, rewriter, question).await;
        if let Some(probability) = decision.filter(|yes| *yes < settings.decision_min) {
            return Outcome::NotNeeded { probability };
        }
    }
    let Some((question, query_source)) = search_query(state, question, rewriter, settings).await
    else {
        return Outcome::Nothing;
    };
    let answer = match tokio::time::timeout(
        settings.timeout,
        search(state, &provider, &question, collection.as_deref(), settings),
    )
    .await
    {
        Ok(Some(answer)) => answer,
        Ok(None) | Err(_) => {
            // The server may have moved or stopped: ask again next time.
            lock_directory().refreshed = None;
            tracing::debug!(node_id = %provider.node_id, "the RAG server did not answer the search");
            return Outcome::Nothing;
        }
    };
    let passages = parse_passages(&answer, settings);
    if passages.is_empty() {
        return Outcome::Nothing;
    }
    Outcome::Retrieved(Retrieved {
        provider,
        passages,
        query: question,
        query_source,
        decision,
        collection,
    })
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
        settings_from(None, None, None, None, None, None, None, None)
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
                rewrite: true,
                rewrite_min_chars: DEFAULT_REWRITE_MIN_CHARS,
                decision: false,
                decision_min: DEFAULT_DECISION_MIN,
            }
        );
        let asked = settings_from(None, None, None, None, None, None, Some("1"), Some("0.6"));
        assert!(asked.decision);
        assert_eq!(asked.decision_min, 0.6);
        assert_eq!(defaults().passages, 1);
        let no_rewrite = settings_from(None, None, None, None, Some("0"), Some("900"), None, None);
        assert!(!no_rewrite.rewrite);
        assert_eq!(no_rewrite.rewrite_min_chars, 900);
        for off in ["0", "false", " OFF ", "no"] {
            assert!(
                !settings_from(Some(off), None, None, None, None, None, None, None).enabled,
                "{off}"
            );
        }
        assert!(settings_from(Some("1"), None, None, None, None, None, None, None).enabled);
        let tuned = settings_from(
            None,
            Some("8"),
            Some("0.9"),
            Some("120"),
            None,
            None,
            None,
            None,
        );
        assert_eq!((tuned.passages, tuned.min_score), (8, 0.9));
        assert_eq!(tuned.timeout, Duration::from_millis(120));
        // Out of range or unreadable values fall back instead of breaking.
        let bad = settings_from(
            None,
            Some("0"),
            Some("7"),
            Some("60000"),
            None,
            None,
            None,
            None,
        );
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
        let four = Settings {
            passages: 4,
            ..defaults()
        };
        let passages = parse_passages(&answer, four);
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
            query: "warranty".into(),
            query_source: QuerySource::Model,
            decision: Some(0.91234),
            collection: Some("folha".into()),
        };
        let summary = retrieved.summary();
        assert_eq!(summary["node_id"], "node-a");
        assert_eq!(
            summary["passages"][0],
            json!({"path": "manual.pdf", "score": 0.877})
        );
        assert_eq!(summary["query_source"], "model");
        assert_eq!(summary["needed"], 0.912);
        assert_eq!(summary["collection"], "folha");
        assert_eq!(
            collection_name(Some(" folha_2025 ")).as_deref(),
            Some("folha_2025")
        );
        for bad in [None, Some(""), Some("a/b"), Some("a b"), Some("../x")] {
            assert_eq!(collection_name(bad), None);
        }
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
    fn a_long_text_is_reduced_to_its_search_terms() {
        let text = "Olá, tudo bem? Eu preciso de uma ajuda com a folha. Quanto foi o pagamento da \
                    funcionária Rafaela em dezembro de 2025, e qual foi o desconto de INSS dela?";
        let query = compact_query(text);
        for kept in ["pagamento", "Rafaela", "dezembro", "2025", "INSS", "folha"] {
            assert!(query.contains(kept), "{kept} missing from {query:?}");
        }
        for dropped in [" de ", " da ", " o ", " uma ", " com ", " qual "] {
            assert!(
                !format!(" {query} ").contains(dropped),
                "{dropped:?} in {query:?}"
            );
        }
        // Order of the text is kept, repeats are not, and the size is bounded.
        assert!(query.find("Rafaela") < query.find("INSS"));
        let long =
            "relatório ".repeat(50) + &(0..60).map(|n| format!("termo{n} ")).collect::<String>();
        let compact = compact_query(&long);
        assert_eq!(compact.split(' ').count(), COMPACT_WORDS);
        assert!(compact.ends_with("termo59"));
        assert_eq!(compact_query("de a o que"), "");
    }

    #[test]
    fn the_model_reads_the_start_and_the_end_of_a_long_message() {
        let message = format!("START {} END", "filler ".repeat(600));
        let excerpt = rewrite_excerpt(&message);
        assert!(excerpt.starts_with("START") && excerpt.ends_with("END"));
        assert!(excerpt.contains("[...]"));
        assert!(excerpt.chars().count() <= REWRITE_HEAD_CHARS + REWRITE_TAIL_CHARS + 8);
        assert_eq!(rewrite_excerpt("  short  "), "short");
        let prompt = rewrite_prompt("how long is the warranty?");
        assert!(prompt.contains("how long is the warranty?") && prompt.ends_with("Search query:"));
    }

    #[test]
    fn the_models_reply_is_cleaned_into_one_query() {
        assert_eq!(
            rewritten_query("  \"pagamento Rafaela dezembro 2025\"\nExplanation: ...").as_deref(),
            Some("pagamento Rafaela dezembro 2025")
        );
        assert_eq!(
            rewritten_query("Search query: warranty period\n").as_deref(),
            Some("warranty period")
        );
        assert_eq!(rewritten_query("\n\n"), None);
        assert_eq!(rewritten_query("\"\""), None);
        let long = "palavra ".repeat(100);
        assert_eq!(
            rewritten_query(&long).unwrap().chars().count(),
            MAX_QUERY_CHARS
        );
    }

    #[test]
    fn the_decision_is_a_yes_or_no_question_with_a_reusable_head() {
        let question = decision_question();
        assert_eq!(question.labels(), ["Yes", "No"]);
        let turn = question.user_message(&json!("Quanto a Rafaela recebeu em dezembro?"));
        // The question comes first and never changes: that is what is cached.
        assert!(turn.starts_with(&question.question_head()));
        assert!(turn.contains("Quanto a Rafaela recebeu em dezembro?"));
        assert!(turn.ends_with("No - it can be answered without them"));
        let prompt = format!("system\n\nUser: {turn}");
        let end = crate::decisions::reusable_prefix_end(&prompt, &question).unwrap();
        assert!(!prompt[..end].contains("Rafaela"));
    }

    #[test]
    fn the_decision_reads_only_the_end_of_the_message() {
        let message = format!("{} what was the December payment?", "preamble ".repeat(500));
        let excerpt = decision_excerpt(&message);
        assert_eq!(excerpt.chars().count(), DECISION_TAIL_CHARS);
        assert!(excerpt.ends_with("what was the December payment?"));
        assert_eq!(decision_excerpt("  short  "), "short");
    }

    #[test]
    fn the_probability_of_yes_is_read_from_the_label_probabilities() {
        assert_eq!(
            yes_probability(&json!({"probabilities": [0.8, 0.2]}), 2),
            Some(0.8)
        );
        // Normalized over the two labels, whatever mass they got.
        assert_eq!(
            yes_probability(&json!({"probabilities": [0.1, 0.3]}), 2),
            Some(0.25)
        );
        assert_eq!(
            yes_probability(&json!({"probabilities": [0.0, 0.0]}), 2),
            Some(0.5)
        );
        assert_eq!(yes_probability(&json!({"probabilities": [0.8]}), 2), None);
        assert_eq!(yes_probability(&json!({}), 2), None);
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
            architectures: Vec::new(),
            settings: Vec::new(),
            stages: false,
            pom_settings: Vec::new(),
        };
        assert_eq!(rag_plugin(&[service("rag", "ready")]), Some("qdrant"));
        assert_eq!(rag_plugin(&[service("rag", "starting")]), None);
        assert_eq!(rag_plugin(&[service("search", "ready")]), None);
        assert_eq!(rag_plugin(&[]), None);
    }
}
