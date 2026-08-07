//! OpenAI-compatible HTTP surface.
//!
//! A protocol adapter, nothing more: it renders chat messages into the single
//! prompt string the pipeline already accepts, calls
//! [`crate::gateway::generate_stream`], and dresses the resulting tokens as
//! OpenAI responses. No inference behaviour is changed or duplicated here.
//!
//! Every node serves these routes, so any peer address works as an OpenAI base
//! URL — not just the primary.
//!
//! Endpoints: `GET /v1/models`, `GET /v1/models/{id}`,
//! `POST /v1/chat/completions` (with `stream: true` SSE), `POST /v1/completions`.

use crate::gateway::{generate_stream, GatewayState, GenEvent};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tracing::info;

/// Tokens generated when the client does not ask for a limit.
const DEFAULT_MAX_TOKENS: u32 = 512;
/// Ceiling regardless of what the client asks for, so one request cannot pin
/// the cluster indefinitely.
const HARD_MAX_TOKENS: u32 = 4096;

pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/models/:id", get(get_model))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        // Browser-based harnesses preflight before posting.
        .route("/v1/chat/completions", axum::routing::options(preflight))
        .route("/v1/completions", axum::routing::options(preflight))
}

async fn preflight() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, cors_headers())
}

/// The API is unauthenticated on a trusted LAN; allowing any origin keeps
/// browser-based clients working without a proxy.
fn cors_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h
}

// ── Message content ──────────────────────────────────────────────────────────

/// `content` is a bare string in the classic API and an array of typed parts in
/// the vision-era one. Harnesses send both, so accept both.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
    /// `content: null` on assistant messages that only carry tool calls.
    Null,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentPart {
    #[serde(default)]
    pub text: Option<String>,
}

impl Content {
    fn as_text(&self) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| p.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            Content::Null => String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default = "default_content")]
    pub content: Content,
}

fn default_content() -> Content {
    Content::Null
}

// ── Requests ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Newer name for `max_tokens`.
    #[serde(default)]
    pub max_completion_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct CompletionRequest {
    #[serde(default)]
    pub model: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

impl ChatRequest {
    fn max_tokens(&self) -> u32 {
        self.max_tokens
            .or(self.max_completion_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .clamp(1, HARD_MAX_TOKENS)
    }
}

// ── Prompt rendering ─────────────────────────────────────────────────────────

/// Flatten a conversation into the single prompt string the pipeline takes.
///
/// The first peer's `inference_server.py` already wraps whatever it receives in
/// the Qwen chat template (see `_apply_chat_template`), so emitting a second
/// template here would nest one inside the other. Instead the turns are
/// labelled in plain text and handed over as one user message.
///
/// A lone user message is passed through untouched, so a simple client behaves
/// exactly like `POST /api/generate` does today.
pub fn render_prompt(messages: &[ChatMessage]) -> String {
    let non_empty: Vec<&ChatMessage> = messages
        .iter()
        .filter(|m| !m.content.as_text().trim().is_empty())
        .collect();

    if let [only] = non_empty.as_slice() {
        if only.role == "user" {
            return only.content.as_text();
        }
    }

    let mut system: Vec<String> = Vec::new();
    let mut turns: Vec<String> = Vec::new();

    for m in &non_empty {
        let text = m.content.as_text();
        match m.role.as_str() {
            "system" | "developer" => system.push(text),
            "assistant" => turns.push(format!("Assistant: {text}")),
            "tool" | "function" => turns.push(format!("Tool result: {text}")),
            _ => turns.push(format!("User: {text}")),
        }
    }

    let mut out = String::new();
    if !system.is_empty() {
        out.push_str(&system.join("\n\n"));
        out.push_str("\n\n");
    }
    out.push_str(&turns.join("\n\n"));
    out
}

/// Rough token count for the `usage` block.
///
/// There is no tokenizer on the Rust side — the tokenizer lives in the first
/// peer's llama.cpp context. Completion tokens are counted exactly by the
/// pipeline; prompt tokens are the usual ~4-chars-per-token approximation, kept
/// only because clients expect the field to exist.
fn approx_tokens(text: &str) -> u32 {
    ((text.chars().count() as f64) / 4.0).ceil() as u32
}

// ── Models ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ModelObject {
    id: String,
    object: &'static str,
    created: i64,
    owned_by: &'static str,
}

/// Models this cluster can actually serve: whatever labels the peers advertise.
async fn model_ids(state: &GatewayState) -> Vec<String> {
    let mut ids: Vec<String> = state
        .all_peers()
        .await
        .into_iter()
        .map(|p| p.model)
        .filter(|m| !m.is_empty())
        .collect();
    let active = state.active_model().await;
    if !active.is_empty() {
        ids.push(active);
    }
    ids.sort();
    ids.dedup();
    ids
}

async fn list_models(State(state): State<GatewayState>) -> impl IntoResponse {
    let created = chrono::Utc::now().timestamp();
    let data: Vec<ModelObject> = model_ids(&state)
        .await
        .into_iter()
        .map(|id| ModelObject {
            id,
            object: "model",
            created,
            owned_by: "rdma-p2p",
        })
        .collect();

    (
        cors_headers(),
        Json(json!({ "object": "list", "data": data })),
    )
}

async fn get_model(
    State(state): State<GatewayState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if model_ids(&state).await.contains(&id) {
        (
            cors_headers(),
            Json(json!({
                "id": id,
                "object": "model",
                "created": chrono::Utc::now().timestamp(),
                "owned_by": "rdma-p2p",
            })),
        )
            .into_response()
    } else {
        error_response(StatusCode::NOT_FOUND, format!("modelo '{id}' não existe nesta rede"))
    }
}

// ── Chat completions ─────────────────────────────────────────────────────────

async fn chat_completions(
    State(state): State<GatewayState>,
    Json(req): Json<ChatRequest>,
) -> Response {
    if req.messages.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "'messages' não pode ser vazio");
    }

    let model = match req.model.clone() {
        // Harnesses often pin a model name that means nothing here; the cluster
        // serves whatever it has loaded, so an unknown name maps to the active
        // model rather than failing the request.
        Some(m) if !m.is_empty() => resolve_model(&state, m).await,
        _ => state.active_model().await,
    };
    let prompt = render_prompt(&req.messages);
    let max_tokens = req.max_tokens();
    let prompt_tokens = approx_tokens(&prompt);

    info!(
        "🤖 /v1/chat/completions model={model} stream={} msgs={} prompt_len={}",
        req.stream,
        req.messages.len(),
        prompt.len()
    );

    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();

    if req.stream {
        stream_response(state, model, prompt, max_tokens, id, created, Mode::Chat)
    } else {
        blocking_response(state, model, prompt, max_tokens, id, created, prompt_tokens, Mode::Chat)
            .await
    }
}

async fn completions(
    State(state): State<GatewayState>,
    Json(req): Json<CompletionRequest>,
) -> Response {
    let model = match req.model.clone() {
        Some(m) if !m.is_empty() => resolve_model(&state, m).await,
        _ => state.active_model().await,
    };
    let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).clamp(1, HARD_MAX_TOKENS);
    let prompt_tokens = approx_tokens(&req.prompt);
    let id = format!("cmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();

    if req.stream {
        stream_response(state, model, req.prompt, max_tokens, id, created, Mode::Completion)
    } else {
        blocking_response(
            state,
            model,
            req.prompt,
            max_tokens,
            id,
            created,
            prompt_tokens,
            Mode::Completion,
        )
        .await
    }
}

/// Use the requested model when a peer serves it, otherwise fall back to
/// whatever this cluster is actually running.
async fn resolve_model(state: &GatewayState, requested: String) -> String {
    if model_ids(state).await.contains(&requested) {
        requested
    } else {
        let active = state.active_model().await;
        info!("modelo '{requested}' desconhecido — usando '{active}'");
        active
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Chat,
    Completion,
}

impl Mode {
    fn object(&self, streaming: bool) -> &'static str {
        match (self, streaming) {
            (Mode::Chat, false) => "chat.completion",
            (Mode::Chat, true) => "chat.completion.chunk",
            (Mode::Completion, _) => "text_completion",
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn blocking_response(
    state: GatewayState,
    model: String,
    prompt: String,
    max_tokens: u32,
    id: String,
    created: i64,
    prompt_tokens: u32,
    mode: Mode,
) -> Response {
    match crate::gateway::generate_blocking(state, model.clone(), prompt, max_tokens).await {
        Ok((text, summary)) => {
            let finish = finish_reason(&summary, max_tokens);
            let choice = match mode {
                Mode::Chat => json!({
                    "index": 0,
                    "message": { "role": "assistant", "content": text },
                    "finish_reason": finish,
                }),
                Mode::Completion => json!({
                    "index": 0,
                    "text": text,
                    "finish_reason": finish,
                }),
            };
            (
                cors_headers(),
                Json(json!({
                    "id": id,
                    "object": mode.object(false),
                    "created": created,
                    "model": model,
                    "choices": [choice],
                    "usage": {
                        "prompt_tokens": prompt_tokens,
                        "completion_tokens": summary.tokens_generated,
                        "total_tokens": prompt_tokens + summary.tokens_generated,
                    },
                    // Not part of the OpenAI schema; kept so the hop chain that
                    // proves distributed execution stays visible to clients.
                    "x_rdma_p2p": {
                        "source": summary.source,
                        "tokens_per_sec": summary.tokens_per_sec,
                        "duration_ms": summary.duration_ms,
                        "partial": summary.partial,
                    },
                })),
            )
                .into_response()
        }
        Err(e) => error_response(StatusCode::SERVICE_UNAVAILABLE, e),
    }
}

fn finish_reason(summary: &crate::gateway::GenSummary, max_tokens: u32) -> &'static str {
    if summary.tokens_generated >= max_tokens {
        "length"
    } else {
        "stop"
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_response(
    state: GatewayState,
    model: String,
    prompt: String,
    max_tokens: u32,
    id: String,
    created: i64,
    mode: Mode,
) -> Response {
    let (tx, rx) = mpsc::channel::<GenEvent>(64);
    tokio::spawn(generate_stream(state, model.clone(), prompt, max_tokens, tx));

    let stream = sse_stream(rx, id, created, model, mode, max_tokens);
    (
        cors_headers(),
        Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()),
    )
        .into_response()
}

/// Turn generation events into the SSE chunk sequence clients expect:
/// a role-only opening chunk, one chunk per token, a chunk carrying
/// `finish_reason`, then the literal `[DONE]` sentinel.
fn sse_stream(
    rx: mpsc::Receiver<GenEvent>,
    id: String,
    created: i64,
    model: String,
    mode: Mode,
    max_tokens: u32,
) -> impl Stream<Item = Result<Event, Infallible>> {
    enum Phase {
        Opening,
        Streaming,
        Ended,
    }

    struct S {
        rx: mpsc::Receiver<GenEvent>,
        phase: Phase,
        id: String,
        created: i64,
        model: String,
        mode: Mode,
        max_tokens: u32,
        /// Queued after the final content chunk.
        trailer: Option<Event>,
    }

    let state = S {
        rx,
        phase: Phase::Opening,
        id,
        created,
        model,
        mode,
        max_tokens,
        trailer: None,
    };

    futures::stream::unfold(state, |mut s| async move {
        if let Some(trailer) = s.trailer.take() {
            s.phase = Phase::Ended;
            return Some((Ok(trailer), s));
        }

        match s.phase {
            Phase::Ended => return None,
            Phase::Opening => {
                s.phase = Phase::Streaming;
                if s.mode == Mode::Chat {
                    let chunk = chunk_json(
                        &s.id,
                        s.created,
                        &s.model,
                        s.mode,
                        json!({ "role": "assistant", "content": "" }),
                        None,
                    );
                    return Some((Ok(Event::default().data(chunk)), s));
                }
            }
            Phase::Streaming => {}
        }

        loop {
            match s.rx.recv().await {
                Some(GenEvent::Token(text)) => {
                    let delta = match s.mode {
                        Mode::Chat => json!({ "content": text }),
                        Mode::Completion => json!(text),
                    };
                    let chunk = chunk_json(&s.id, s.created, &s.model, s.mode, delta, None);
                    return Some((Ok(Event::default().data(chunk)), s));
                }
                Some(GenEvent::Done(summary)) => {
                    let reason = finish_reason(&summary, s.max_tokens);
                    let delta = match s.mode {
                        Mode::Chat => json!({}),
                        Mode::Completion => json!(""),
                    };
                    let chunk =
                        chunk_json(&s.id, s.created, &s.model, s.mode, delta, Some(reason));
                    s.trailer = Some(Event::default().data("[DONE]"));
                    return Some((Ok(Event::default().data(chunk)), s));
                }
                Some(GenEvent::Failed(err)) => {
                    // The status line is already sent, so the error has to ride
                    // inside the stream. Clients surface it as the last event.
                    let payload = json!({ "error": { "message": err, "type": "server_error" } });
                    s.trailer = Some(Event::default().data("[DONE]"));
                    return Some((Ok(Event::default().data(payload.to_string())), s));
                }
                None => {
                    s.phase = Phase::Ended;
                    return Some((Ok(Event::default().data("[DONE]")), s));
                }
            }
        }
    })
}

/// Serialise one streaming chunk in the shape the mode requires.
fn chunk_json(
    id: &str,
    created: i64,
    model: &str,
    mode: Mode,
    delta: serde_json::Value,
    finish: Option<&str>,
) -> String {
    let choice = match mode {
        Mode::Chat => json!({ "index": 0, "delta": delta, "finish_reason": finish }),
        Mode::Completion => json!({ "index": 0, "text": delta, "finish_reason": finish }),
    };
    json!({
        "id": id,
        "object": mode.object(true),
        "created": created,
        "model": model,
        "choices": [choice],
    })
    .to_string()
}

fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    let message = message.into();
    (
        status,
        cors_headers(),
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": status.as_u16(),
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: Content::Text(text.into()),
        }
    }

    #[test]
    fn single_user_message_passes_through_unchanged() {
        // Must stay byte-identical to what /api/generate would receive, so the
        // simple path behaves the same in both APIs.
        assert_eq!(render_prompt(&[msg("user", "O que e RDMA?")]), "O que e RDMA?");
    }

    #[test]
    fn system_prompt_leads_the_conversation() {
        let out = render_prompt(&[msg("system", "Fale em portugues."), msg("user", "Oi")]);
        assert!(out.starts_with("Fale em portugues."));
        assert!(out.ends_with("User: Oi"));
    }

    #[test]
    fn multi_turn_conversation_is_labelled() {
        let out = render_prompt(&[
            msg("user", "1+1?"),
            msg("assistant", "2"),
            msg("user", "e 2+2?"),
        ]);
        assert!(out.contains("User: 1+1?"));
        assert!(out.contains("Assistant: 2"));
        assert!(out.trim_end().ends_with("User: e 2+2?"));
    }

    #[test]
    fn empty_messages_are_dropped() {
        let out = render_prompt(&[msg("system", "   "), msg("user", "oi")]);
        assert_eq!(out, "oi");
    }

    #[test]
    fn content_parts_are_concatenated() {
        let m = ChatMessage {
            role: "user".into(),
            content: Content::Parts(vec![
                ContentPart { text: Some("linha 1".into()) },
                ContentPart { text: Some("linha 2".into()) },
            ]),
        };
        assert_eq!(render_prompt(&[m]), "linha 1\nlinha 2");
    }

    #[test]
    fn parses_array_content_from_real_clients() {
        let raw = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"oi"}]}]}"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(render_prompt(&req.messages), "oi");
        assert!(!req.stream);
    }

    #[test]
    fn parses_null_content_on_assistant_messages() {
        let raw = r#"{"messages":[{"role":"assistant","content":null},{"role":"user","content":"oi"}]}"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(render_prompt(&req.messages), "oi");
    }

    #[test]
    fn max_tokens_is_clamped_and_defaulted() {
        let mk = |json: &str| -> ChatRequest { serde_json::from_str(json).unwrap() };
        assert_eq!(mk(r#"{"messages":[]}"#).max_tokens(), DEFAULT_MAX_TOKENS);
        assert_eq!(mk(r#"{"messages":[],"max_tokens":10}"#).max_tokens(), 10);
        assert_eq!(
            mk(r#"{"messages":[],"max_completion_tokens":32}"#).max_tokens(),
            32
        );
        assert_eq!(
            mk(r#"{"messages":[],"max_tokens":999999}"#).max_tokens(),
            HARD_MAX_TOKENS
        );
    }

    #[test]
    fn finish_reason_reflects_the_limit() {
        let summary = |n: u32| crate::gateway::GenSummary {
            source: "pipeline:a→b".into(),
            tokens_generated: n,
            tokens_per_sec: 1.0,
            duration_ms: 10,
            partial: false,
        };
        assert_eq!(finish_reason(&summary(5), 100), "stop");
        assert_eq!(finish_reason(&summary(100), 100), "length");
    }
}
