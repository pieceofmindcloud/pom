//! Anthropic Messages API surface — what Claude Code (and anything else built
//! on `ANTHROPIC_BASE_URL`) speaks, distinct from the OpenAI protocol already
//! served in [`crate::openai`].
//!
//! Same shape as that module: a protocol adapter, nothing more. It renders
//! `messages`/`system` into the single prompt string the pipeline already
//! accepts, calls [`crate::gateway::generate_stream`], and dresses the result
//! as Anthropic Messages API responses (SSE event names included). No
//! inference behaviour is changed or duplicated here — this file and
//! `openai.rs` both bottom out in the same generation path.
//!
//! **Known gap**: Claude Code's agentic loop (file edits, bash, etc.) relies
//! on the model emitting structured `tool_use` content blocks in response to
//! the `tools` it declares on every request. This backend has no
//! grammar-constrained decoding for that — it only ever produces plain text.
//! Every response comes back as a single `text` block, `stop_reason` is never
//! `tool_use`. A model behind this endpoint can chat, but cannot drive Claude
//! Code's tools. Neither `tools` nor `stop_sequences` need a struct field
//! below to be accepted: serde drops unknown JSON fields by default (neither
//! request type opts into `deny_unknown_fields`), so a real Claude Code
//! request parses fine — the fields are just never looked at.
//!
//! Endpoints: `POST /v1/messages` (`stream: true` → SSE),
//! `POST /v1/messages/count_tokens`.

use crate::auth::ApiKeyAuth;
use crate::gateway::{GatewayState, GenEvent, GenSummary};
use crate::openai::{approx_tokens, cors_headers, preflight, resolve_model};
use axum::{
    extract::State,
    http::StatusCode,
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::post,
    Json, Router,
};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::json;
use std::collections::VecDeque;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tracing::info;

/// Tokens generated when the client does not ask for a limit — Anthropic's
/// real API makes `max_tokens` mandatory, but a lenient default costs
/// nothing and keeps a hand-rolled test request from 400ing.
const DEFAULT_MAX_TOKENS: u32 = 512;
/// Ceiling regardless of what the client asks for, matching `openai.rs`.
const HARD_MAX_TOKENS: u32 = 4096;

pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/messages", axum::routing::options(preflight))
        .route("/v1/messages/count_tokens", axum::routing::options(preflight))
}

// ── Message content ──────────────────────────────────────────────────────────

/// `content` is a bare string on the simple path and an array of typed blocks
/// once tools, images, or prompt caching are involved. Claude Code always
/// sends the array form. Blocks this adapter can't render (`tool_use`,
/// `tool_result`, `image`, ...) are parsed — so an unsupported block never
/// fails the request — and simply contribute no text.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

impl AnthropicContent {
    fn as_text(&self) -> String {
        match self {
            AnthropicContent::Text(s) => s.clone(),
            AnthropicContent::Blocks(blocks) => blocks
                .iter()
                .filter(|b| b.kind == "text")
                .filter_map(|b| b.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// The top-level `system` field: a bare string, or blocks (Claude Code sends
/// blocks, each carrying `cache_control` this backend has no use for).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl SystemPrompt {
    fn as_text(&self) -> String {
        match self {
            SystemPrompt::Text(s) => s.clone(),
            SystemPrompt::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| b.text.clone())
                .collect::<Vec<_>>()
                .join("\n\n"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicContent,
}

// ── Requests ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MessagesRequest {
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub system: Option<SystemPrompt>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_max_tokens() -> u32 {
    DEFAULT_MAX_TOKENS
}

impl MessagesRequest {
    fn clamped_max_tokens(&self) -> u32 {
        self.max_tokens.clamp(1, HARD_MAX_TOKENS)
    }
}

#[derive(Debug, Deserialize)]
pub struct CountTokensRequest {
    pub messages: Vec<AnthropicMessage>,
    #[serde(default)]
    pub system: Option<SystemPrompt>,
}

// ── Prompt rendering ─────────────────────────────────────────────────────────

/// Flatten `system` + `messages` into the single prompt string the pipeline
/// takes — same reasoning as `openai::render_prompt`: the first peer's
/// `inference_server.py` already applies the model's chat template, so a
/// second one here would nest inside it. A lone user message with no system
/// prompt passes through untouched.
pub fn render_prompt(system: Option<&SystemPrompt>, messages: &[AnthropicMessage]) -> String {
    let non_empty: Vec<&AnthropicMessage> = messages
        .iter()
        .filter(|m| !m.content.as_text().trim().is_empty())
        .collect();

    let system_text = system.map(|s| s.as_text()).unwrap_or_default();
    let system_text = system_text.trim();

    if system_text.is_empty() {
        if let [only] = non_empty.as_slice() {
            if only.role == "user" {
                return only.content.as_text();
            }
        }
    }

    let mut out = String::new();
    if !system_text.is_empty() {
        out.push_str(system_text);
        out.push_str("\n\n");
    }

    let turns: Vec<String> = non_empty
        .iter()
        .map(|m| {
            let text = m.content.as_text();
            match m.role.as_str() {
                "assistant" => format!("Assistant: {text}"),
                _ => format!("User: {text}"),
            }
        })
        .collect();
    out.push_str(&turns.join("\n\n"));
    out
}

// ── Messages ─────────────────────────────────────────────────────────────────

async fn messages(
    auth: ApiKeyAuth,
    State(state): State<GatewayState>,
    Json(req): Json<MessagesRequest>,
) -> Response {
    if req.messages.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "`messages` cannot be empty",
        );
    }

    let model = match req.model.clone() {
        Some(m) if !m.is_empty() => resolve_model(&state, m).await,
        _ => state.active_model().await,
    };
    let prompt = render_prompt(req.system.as_ref(), &req.messages);
    let max_tokens = req.clamped_max_tokens();
    let input_tokens = approx_tokens(&prompt);

    info!(
        "🤖 /v1/messages model={model} stream={} msgs={} prompt_len={}",
        req.stream,
        req.messages.len(),
        prompt.len()
    );

    let id = format!("msg_{}", uuid::Uuid::new_v4().simple());

    if req.stream {
        stream_response(
            state,
            auth.into_caller(),
            model,
            prompt,
            max_tokens,
            id,
            input_tokens,
        )
    } else {
        blocking_response(
            state,
            auth.into_caller(),
            model,
            prompt,
            max_tokens,
            id,
            input_tokens,
        )
        .await
    }
}

async fn count_tokens(_auth: ApiKeyAuth, Json(req): Json<CountTokensRequest>) -> Response {
    let prompt = render_prompt(req.system.as_ref(), &req.messages);
    (
        cors_headers(),
        Json(json!({ "input_tokens": approx_tokens(&prompt) })),
    )
        .into_response()
}

fn stop_reason(summary: &GenSummary, max_tokens: u32) -> &'static str {
    if summary.tokens_generated >= max_tokens {
        "max_tokens"
    } else {
        "end_turn"
    }
}

async fn blocking_response(
    state: GatewayState,
    caller: crate::reqlog::Caller,
    model: String,
    prompt: String,
    max_tokens: u32,
    id: String,
    input_tokens: u32,
) -> Response {
    match crate::gateway::generate_blocking(state, "/v1/messages", caller, model.clone(), prompt, max_tokens)
        .await
    {
        Ok((text, summary)) => {
            let reason = stop_reason(&summary, max_tokens);
            (
                cors_headers(),
                Json(json!({
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [{ "type": "text", "text": text }],
                    "stop_reason": reason,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": summary.tokens_generated,
                    },
                    // Not part of the Anthropic schema; kept so the hop chain
                    // that proves distributed execution stays visible.
                    "x_rdma_p2p": {
                        "source": summary.source,
                        "tokens_per_sec": summary.tokens_per_sec,
                        "duration_ms": summary.duration_ms,
                        "partial": summary.partial,
                        "trace": summary.trace,
                    },
                })),
            )
                .into_response()
        }
        Err(e) => error_response(StatusCode::SERVICE_UNAVAILABLE, "api_error", e),
    }
}

fn stream_response(
    state: GatewayState,
    caller: crate::reqlog::Caller,
    model: String,
    prompt: String,
    max_tokens: u32,
    id: String,
    input_tokens: u32,
) -> Response {
    let (tx, rx) = mpsc::channel::<GenEvent>(64);
    crate::gateway::spawn_logged_generation(
        state,
        "/v1/messages",
        caller,
        model.clone(),
        prompt,
        max_tokens,
        tx,
    );

    let stream = sse_stream(rx, id, model, max_tokens, input_tokens);
    (
        cors_headers(),
        Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()),
    )
        .into_response()
}

/// Turn generation events into the Anthropic streaming event sequence:
/// `message_start` → `content_block_start` → one `content_block_delta` per
/// token → `content_block_stop` → `message_delta` (carries `stop_reason` and
/// the final `output_tokens`) → `message_stop`. A mid-stream failure emits a
/// single `error` event instead of the trailer and ends there, mirroring how
/// the real API has no graceful way to fail a stream already in progress.
fn sse_stream(
    rx: mpsc::Receiver<GenEvent>,
    id: String,
    model: String,
    max_tokens: u32,
    input_tokens: u32,
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
        model: String,
        max_tokens: u32,
        input_tokens: u32,
        /// Holds events a single generator event expanded into (e.g. `Done`
        /// becomes three: content_block_stop, message_delta, message_stop).
        queue: VecDeque<Event>,
    }

    fn ev(name: &'static str, data: serde_json::Value) -> Event {
        Event::default().event(name).data(data.to_string())
    }

    let state = S {
        rx,
        phase: Phase::Opening,
        id,
        model,
        max_tokens,
        input_tokens,
        queue: VecDeque::new(),
    };

    futures::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(e) = s.queue.pop_front() {
                return Some((Ok(e), s));
            }

            match s.phase {
                Phase::Ended => return None,
                Phase::Opening => {
                    s.phase = Phase::Streaming;
                    s.queue.push_back(ev(
                        "message_start",
                        json!({
                            "type": "message_start",
                            "message": {
                                "id": s.id,
                                "type": "message",
                                "role": "assistant",
                                "model": s.model,
                                "content": [],
                                "stop_reason": null,
                                "stop_sequence": null,
                                "usage": { "input_tokens": s.input_tokens, "output_tokens": 0 },
                            }
                        }),
                    ));
                    s.queue.push_back(ev(
                        "content_block_start",
                        json!({
                            "type": "content_block_start",
                            "index": 0,
                            "content_block": { "type": "text", "text": "" },
                        }),
                    ));
                    continue;
                }
                Phase::Streaming => match s.rx.recv().await {
                    Some(GenEvent::Token(text)) => {
                        return Some((
                            Ok(ev(
                                "content_block_delta",
                                json!({
                                    "type": "content_block_delta",
                                    "index": 0,
                                    "delta": { "type": "text_delta", "text": text },
                                }),
                            )),
                            s,
                        ));
                    }
                    Some(GenEvent::Done(summary)) => {
                        let reason = stop_reason(&summary, s.max_tokens);
                        s.queue.push_back(ev(
                            "content_block_stop",
                            json!({ "type": "content_block_stop", "index": 0 }),
                        ));
                        s.queue.push_back(ev(
                            "message_delta",
                            json!({
                                "type": "message_delta",
                                "delta": { "stop_reason": reason, "stop_sequence": null },
                                "usage": { "output_tokens": summary.tokens_generated },
                            }),
                        ));
                        s.queue
                            .push_back(ev("message_stop", json!({ "type": "message_stop" })));
                        s.phase = Phase::Ended;
                        continue;
                    }
                    Some(GenEvent::Failed(err)) => {
                        s.phase = Phase::Ended;
                        return Some((
                            Ok(ev(
                                "error",
                                json!({
                                    "type": "error",
                                    "error": { "type": "api_error", "message": err },
                                }),
                            )),
                            s,
                        ));
                    }
                    None => {
                        // Client hung up or the generator dropped its sender
                        // without a terminal event; nothing more to say.
                        s.phase = Phase::Ended;
                        continue;
                    }
                },
            }
        }
    })
}

fn error_response(status: StatusCode, kind: &str, message: impl Into<String>) -> Response {
    (
        status,
        cors_headers(),
        Json(json!({
            "type": "error",
            "error": { "type": kind, "message": message.into() }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, text: &str) -> AnthropicMessage {
        AnthropicMessage {
            role: role.into(),
            content: AnthropicContent::Text(text.into()),
        }
    }

    #[test]
    fn single_user_message_with_no_system_passes_through_unchanged() {
        assert_eq!(
            render_prompt(None, &[msg("user", "O que e RDMA?")]),
            "O que e RDMA?"
        );
    }

    #[test]
    fn system_prompt_leads_the_conversation() {
        let out = render_prompt(
            Some(&SystemPrompt::Text("Fale em portugues.".into())),
            &[msg("user", "Oi")],
        );
        assert!(out.starts_with("Fale em portugues."));
        assert!(out.ends_with("User: Oi"));
    }

    #[test]
    fn multi_turn_conversation_is_labelled() {
        let out = render_prompt(
            None,
            &[
                msg("user", "1+1?"),
                msg("assistant", "2"),
                msg("user", "e 2+2?"),
            ],
        );
        assert!(out.contains("User: 1+1?"));
        assert!(out.contains("Assistant: 2"));
        assert!(out.trim_end().ends_with("User: e 2+2?"));
    }

    #[test]
    fn content_blocks_are_concatenated_and_non_text_blocks_are_ignored() {
        let raw = r#"{"role":"user","content":[
            {"type":"text","text":"linha 1"},
            {"type":"tool_result","tool_use_id":"x","content":"ignored"},
            {"type":"text","text":"linha 2"}
        ]}"#;
        let m: AnthropicMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(m.content.as_text(), "linha 1\nlinha 2");
    }

    #[test]
    fn parses_a_real_claude_code_style_request_with_tools() {
        let raw = r#"{
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "system": [{"type":"text","text":"You are Claude Code.","cache_control":{"type":"ephemeral"}}],
            "messages": [{"role":"user","content":[{"type":"text","text":"oi"}]}],
            "tools": [{"name":"Bash","description":"...","input_schema":{}}],
            "tool_choice": {"type":"auto"},
            "stream": true
        }"#;
        let req: MessagesRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.max_tokens, 1024);
        assert!(req.stream);
        let out = render_prompt(req.system.as_ref(), &req.messages);
        assert!(out.starts_with("You are Claude Code."));
        assert!(out.ends_with("User: oi"));
    }

    #[test]
    fn max_tokens_is_clamped() {
        let mk = |json: &str| -> MessagesRequest { serde_json::from_str(json).unwrap() };
        assert_eq!(
            mk(r#"{"messages":[],"max_tokens":10}"#).clamped_max_tokens(),
            10
        );
        assert_eq!(
            mk(r#"{"messages":[],"max_tokens":999999}"#).clamped_max_tokens(),
            HARD_MAX_TOKENS
        );
        assert_eq!(mk(r#"{"messages":[]}"#).max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn stop_reason_reflects_the_limit() {
        let summary = |n: u32| GenSummary {
            source: "pipeline:a→b".into(),
            tokens_generated: n,
            tokens_per_sec: 1.0,
            duration_ms: 10,
            partial: false,
            trace: None,
        };
        assert_eq!(stop_reason(&summary(5), 100), "end_turn");
        assert_eq!(stop_reason(&summary(100), 100), "max_tokens");
    }
}
