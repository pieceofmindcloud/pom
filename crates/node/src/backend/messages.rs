//! Anthropic Messages API surface — what Claude Code (and anything else built
//! on `ANTHROPIC_BASE_URL`) speaks, distinct from the OpenAI protocol already
//! served in [`crate::openai`].
//!
//! Same shape as that module: a protocol adapter, nothing more. It renders
//! `messages`/`system`/`tools` into the single prompt string the pipeline
//! already accepts, calls [`crate::gateway::generate_stream`], and dresses
//! the result as Anthropic Messages API responses (SSE event names
//! included). No inference behaviour is changed or duplicated here — this
//! file and `openai.rs` both bottom out in the same generation path.
//!
//! **Tool calling is prompt-engineered and text-parsed, not protocol-level.**
//! This backend has no grammar-constrained decoding anywhere: the model's
//! chat template is applied exactly once, in `infer_server.rs`, via
//! llama.cpp's `llama_chat_apply_template`, which only takes `(role,
//! content)` string pairs — there is no `tools` parameter the way Python's
//! `transformers.apply_chat_template(messages, tools=...)` has one. So
//! `tools` are rendered into the system prompt as plain text
//! (`render_tools_block`), and the model's raw output is scanned
//! (`TagScanner`) for `<tool_call>{"name":...,"arguments":...}</tool_call>`
//! spans on the way back. That tag convention and its exact wording is not
//! invented — it's copied verbatim from `tokenizer.chat_template` in the
//! actually-deployed GGUF (Qwen's own Hermes-style function-calling
//! template), so the model sees text as close as possible to what it was
//! fine-tuned on. This has real, bounded failure modes a grammar-constrained
//! backend wouldn't: a malformed or nameless `<tool_call>` degrades to plain
//! text (see `parse_tool_call`) rather than failing the request or losing
//! the content. `tool_choice` is parsed-and-ignored like `stop_sequences` —
//! neither needs a struct field to be accepted, since serde silently drops
//! unknown JSON fields on both request types here.
//!
//! Endpoints: `POST /v1/messages` (`stream: true` → SSE),
//! `POST /v1/messages/count_tokens`.

use crate::auth::ApiKeyAuth;
use crate::gateway::{GatewayState, GenEvent, GenSummary};
use crate::openai::{approx_tokens, cors_headers, preflight, resolve_model};
use crate::tool_call::{
    parse_output, render_tool_call, render_tools_block, OutputPart, ScanEvent, TagScanner, ToolDef,
};
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
/// sends the array form.
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
    /// `tool_use` blocks.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    /// `tool_result` blocks. Anthropic allows `content` to be a bare string
    /// or an array of blocks — Claude Code sends both, depending on the tool.
    #[serde(default)]
    pub content: Option<ToolResultContent>,
    #[serde(default)]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Render one content block back to the text form the model itself would
/// produce — `tool_use` as `<tool_call>...</tool_call>` (the exact shape the
/// model emits, so replayed history stays self-consistent with what it
/// wrote), `tool_result` as `<tool_response>...</tool_response>` (both tags
/// copied verbatim from the deployed GGUF's chat template — see the module
/// doc comment). Blocks this adapter can't render at all (image, thinking,
/// ...) contribute nothing, same as before tool support existed.
fn render_block(b: &ContentBlock) -> String {
    match b.kind.as_str() {
        "text" => b.text.clone().unwrap_or_default(),
        "tool_use" => render_tool_call(
            &b.name.clone().unwrap_or_default(),
            &b.input.clone().unwrap_or_else(|| json!({})),
        ),
        "tool_result" => {
            let inner = match &b.content {
                Some(ToolResultContent::Text(s)) => s.clone(),
                Some(ToolResultContent::Blocks(blocks)) => blocks
                    .iter()
                    .map(render_block)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n"),
                None => String::new(),
            };
            let prefix = if b.is_error.unwrap_or(false) {
                "Error: "
            } else {
                ""
            };
            format!("<tool_response>\n{prefix}{inner}\n</tool_response>")
        }
        _ => String::new(),
    }
}

impl AnthropicContent {
    fn as_text(&self) -> String {
        match self {
            AnthropicContent::Text(s) => s.clone(),
            AnthropicContent::Blocks(blocks) => blocks
                .iter()
                .map(render_block)
                .filter(|s| !s.is_empty())
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
                .map(render_block)
                .filter(|s| !s.is_empty())
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
    #[serde(default)]
    pub tools: Vec<ToolDef>,
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
    #[serde(default)]
    pub tools: Vec<ToolDef>,
}

// ── Prompt rendering ─────────────────────────────────────────────────────────

/// Flatten `system` + `tools` + `messages` into the single prompt string the
/// pipeline takes — same reasoning as `openai::render_prompt`: the first
/// peer already applies the model's chat template once, so a second one here
/// would nest inside it. A lone user message with no system prompt and no
/// tools passes through untouched.
pub fn render_prompt(
    system: Option<&SystemPrompt>,
    messages: &[AnthropicMessage],
    tools: &[ToolDef],
) -> String {
    let non_empty: Vec<&AnthropicMessage> = messages
        .iter()
        .filter(|m| !m.content.as_text().trim().is_empty())
        .collect();

    let tools_block = render_tools_block(tools);
    let mut system_text = system.map(|s| s.as_text()).unwrap_or_default();
    if !tools_block.is_empty() {
        if !system_text.trim().is_empty() {
            system_text.push_str("\n\n");
        }
        system_text.push_str(&tools_block);
    }
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

// ── Output shaping ───────────────────────────────────────────────────────────

/// Anthropic `content` blocks from parsed output, plus whether any tool call
/// was found (drives `stop_reason`). Always at least one block, matching the
/// pre-tool-calling response shape when there is nothing to say.
fn content_blocks(parts: &[OutputPart]) -> (Vec<serde_json::Value>, bool) {
    let mut blocks = Vec::new();
    let mut saw_tool_call = false;
    for p in parts {
        match p {
            OutputPart::Text(s) => blocks.push(json!({ "type": "text", "text": s })),
            OutputPart::ToolCall { name, arguments } => {
                saw_tool_call = true;
                blocks.push(json!({
                    "type": "tool_use",
                    "id": format!("toolu_{}", uuid::Uuid::new_v4().simple()),
                    "name": name,
                    "input": arguments,
                }));
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "" }));
    }
    (blocks, saw_tool_call)
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
    let prompt = render_prompt(req.system.as_ref(), &req.messages, &req.tools);
    let max_tokens = req.clamped_max_tokens();
    let input_tokens = approx_tokens(&prompt);

    info!(
        "🤖 /v1/messages model={model} stream={} msgs={} tools={} prompt_len={}",
        req.stream,
        req.messages.len(),
        req.tools.len(),
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
    let prompt = render_prompt(req.system.as_ref(), &req.messages, &req.tools);
    (
        cors_headers(),
        Json(json!({ "input_tokens": approx_tokens(&prompt) })),
    )
        .into_response()
}

fn stop_reason(summary: &GenSummary, max_tokens: u32, saw_tool_call: bool) -> &'static str {
    if saw_tool_call {
        "tool_use"
    } else if summary.tokens_generated >= max_tokens {
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
            let (content, saw_tool_call) = content_blocks(&parse_output(&text));
            let reason = stop_reason(&summary, max_tokens, saw_tool_call);
            (
                cors_headers(),
                Json(json!({
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": content,
                    "stop_reason": reason,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": summary.tokens_generated,
                    },
                    // Not part of the Anthropic schema; kept so the hop chain
                    // that proves distributed execution stays visible.
                    "x_pom": {
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

/// Turns scanner events into `(event_name, payload)` SSE pairs, tracking
/// which content-block index is currently open (`text_open`) and the next
/// free index (`next_index`). Synchronous and free of any axum/SSE type so
/// it's directly unit-testable without a streaming harness.
///
/// Tool-call arguments are emitted as a single `input_json_delta` carrying
/// the whole JSON blob, not truly incrementally: unlike real Claude
/// generation, the tool's name is only known once the entire `<tool_call>`
/// tag closes (name and arguments arrive in the same JSON object here), so
/// there's nothing to stream *before* that point. The blob is small and the
/// wait is sub-second either way — an honest, deliberate trade-off, not a
/// missing feature.
fn apply_scan_events(
    events: Vec<ScanEvent>,
    text_open: &mut Option<u32>,
    next_index: &mut u32,
    saw_tool_call: &mut bool,
    out: &mut Vec<(&'static str, serde_json::Value)>,
) {
    for e in events {
        match e {
            ScanEvent::Text(s) if !s.is_empty() => {
                let idx = match *text_open {
                    Some(idx) => idx,
                    None => {
                        let idx = *next_index;
                        *next_index += 1;
                        out.push((
                            "content_block_start",
                            json!({
                                "type": "content_block_start",
                                "index": idx,
                                "content_block": { "type": "text", "text": "" },
                            }),
                        ));
                        *text_open = Some(idx);
                        idx
                    }
                };
                out.push((
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "text_delta", "text": s },
                    }),
                ));
            }
            ScanEvent::Text(_) | ScanEvent::ToolCallOpen | ScanEvent::ToolCallChunk(_) => {}
            ScanEvent::ToolCallClose(Ok((name, arguments))) => {
                *saw_tool_call = true;
                if let Some(idx) = text_open.take() {
                    out.push((
                        "content_block_stop",
                        json!({ "type": "content_block_stop", "index": idx }),
                    ));
                }
                let idx = *next_index;
                *next_index += 1;
                let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                out.push((
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": idx,
                        "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} },
                    }),
                ));
                out.push((
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": { "type": "input_json_delta", "partial_json": arguments.to_string() },
                    }),
                ));
                out.push((
                    "content_block_stop",
                    json!({ "type": "content_block_stop", "index": idx }),
                ));
            }
            ScanEvent::ToolCallClose(Err(raw)) => {
                apply_scan_events(
                    vec![ScanEvent::Text(raw)],
                    text_open,
                    next_index,
                    saw_tool_call,
                    out,
                );
            }
        }
    }
}

/// Turn generation events into the Anthropic streaming event sequence:
/// `message_start` → `content_block_start` → text/tool_use blocks as the
/// model's output resolves (see `apply_scan_events`) → `message_delta`
/// (carries `stop_reason` and the final `output_tokens`) → `message_stop`. A
/// mid-stream failure emits a single `error` event instead of the trailer
/// and ends there, mirroring how the real API has no graceful way to fail a
/// stream already in progress.
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
        /// becomes several: content_block_stop, message_delta, message_stop).
        queue: VecDeque<Event>,
        scanner: TagScanner,
        /// Index of the currently open text block, if any.
        text_open: Option<u32>,
        next_index: u32,
        saw_tool_call: bool,
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
        scanner: TagScanner::new(),
        text_open: None,
        next_index: 0,
        saw_tool_call: false,
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
                    s.text_open = Some(0);
                    s.next_index = 1;
                    continue;
                }
                Phase::Streaming => match s.rx.recv().await {
                    Some(GenEvent::Token(text)) => {
                        let events = s.scanner.feed(&text);
                        let mut pairs = Vec::new();
                        apply_scan_events(
                            events,
                            &mut s.text_open,
                            &mut s.next_index,
                            &mut s.saw_tool_call,
                            &mut pairs,
                        );
                        for (name, data) in pairs {
                            s.queue.push_back(ev(name, data));
                        }
                        continue;
                    }
                    Some(GenEvent::Done(summary)) => {
                        let scanner = std::mem::replace(&mut s.scanner, TagScanner::new());
                        let finish_events = scanner.finish();
                        let mut pairs = Vec::new();
                        apply_scan_events(
                            finish_events,
                            &mut s.text_open,
                            &mut s.next_index,
                            &mut s.saw_tool_call,
                            &mut pairs,
                        );
                        for (name, data) in pairs {
                            s.queue.push_back(ev(name, data));
                        }
                        if let Some(idx) = s.text_open.take() {
                            s.queue.push_back(ev(
                                "content_block_stop",
                                json!({ "type": "content_block_stop", "index": idx }),
                            ));
                        }
                        let reason = stop_reason(&summary, s.max_tokens, s.saw_tool_call);
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

    // ── render_prompt ────────────────────────────────────────────────────────

    #[test]
    fn single_user_message_with_no_system_passes_through_unchanged() {
        assert_eq!(
            render_prompt(None, &[msg("user", "O que e inferencia distribuida?")], &[]),
            "O que e inferencia distribuida?"
        );
    }

    #[test]
    fn system_prompt_leads_the_conversation() {
        let out = render_prompt(
            Some(&SystemPrompt::Text("Fale em portugues.".into())),
            &[msg("user", "Oi")],
            &[],
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
            &[],
        );
        assert!(out.contains("User: 1+1?"));
        assert!(out.contains("Assistant: 2"));
        assert!(out.trim_end().ends_with("User: e 2+2?"));
    }

    #[test]
    fn no_tools_renders_identically_to_before_tool_support() {
        let out = render_prompt(
            Some(&SystemPrompt::Text("Fale em portugues.".into())),
            &[msg("user", "Oi")],
            &[],
        );
        assert_eq!(out, "Fale em portugues.\n\nUser: Oi");
    }

    #[test]
    fn content_blocks_are_rendered_in_order_including_tool_result() {
        let raw = r#"{"role":"user","content":[
            {"type":"text","text":"linha 1"},
            {"type":"tool_result","tool_use_id":"x","content":"resultado"},
            {"type":"text","text":"linha 2"}
        ]}"#;
        let m: AnthropicMessage = serde_json::from_str(raw).unwrap();
        let out = m.content.as_text();
        assert!(out.starts_with("linha 1"));
        assert!(out.contains("<tool_response>\nresultado\n</tool_response>"));
        assert!(out.ends_with("linha 2"));
    }

    #[test]
    fn parses_a_real_claude_code_style_request_with_tools() {
        let raw = r#"{
            "model": "claude-opus-4-5",
            "max_tokens": 1024,
            "system": [{"type":"text","text":"You are Claude Code.","cache_control":{"type":"ephemeral"}}],
            "messages": [{"role":"user","content":[{"type":"text","text":"oi"}]}],
            "tools": [{"name":"Bash","description":"Run a shell command","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}],
            "tool_choice": {"type":"auto"},
            "stream": true
        }"#;
        let req: MessagesRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.max_tokens, 1024);
        assert!(req.stream);
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "Bash");
        let out = render_prompt(req.system.as_ref(), &req.messages, &req.tools);
        assert!(out.starts_with("You are Claude Code."));
        assert!(out.contains("<tools>"));
        assert!(out.contains("Bash"));
        assert!(out.ends_with("User: oi"));
    }

    #[test]
    fn render_prompt_round_trip_with_tool_use_and_tool_result() {
        let raw = r#"{
            "system": "You are Claude Code.",
            "tools": [{"name":"Bash","description":"Run a shell command","input_schema":{"type":"object","properties":{"command":{"type":"string"}}}}],
            "messages": [
                {"role":"user","content":"list files"},
                {"role":"assistant","content":[
                    {"type":"text","text":"Sure, let me check."},
                    {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}
                ]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_1","content":"file1.txt\nfile2.txt"}
                ]},
                {"role":"assistant","content":"Found two files."}
            ]
        }"#;
        let req: MessagesRequest = serde_json::from_str(raw).unwrap();
        let out = render_prompt(req.system.as_ref(), &req.messages, &req.tools);

        assert!(out.starts_with("You are Claude Code."));
        assert!(out.contains("<tools>"));
        assert!(out.contains("Bash"));

        // The tools block's own instructions also mention `<tool_call>`, so
        // the *real* rendered call (from the assistant turn) is the last
        // occurrence, not the first.
        let tool_call_pos = out.rfind("<tool_call>").expect("tool call rendered");
        let tool_response_pos = out.find("<tool_response>").expect("tool response rendered");
        let sure_pos = out.find("Sure, let me check.").unwrap();
        let found_pos = out.find("Found two files.").unwrap();
        assert!(sure_pos < tool_call_pos);
        assert!(tool_call_pos < tool_response_pos);
        assert!(tool_response_pos < found_pos);
        assert!(out.contains("file1.txt\nfile2.txt"));
    }

    #[test]
    fn tool_use_block_render_and_parse_are_symmetric() {
        let block = ContentBlock {
            kind: "tool_use".into(),
            text: None,
            name: Some("Read".into()),
            input: Some(json!({"file_path": "/tmp/x.txt"})),
            content: None,
            is_error: None,
        };
        let rendered = render_block(&block);
        let parts = parse_output(&rendered);
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            OutputPart::ToolCall { name, arguments } => {
                assert_eq!(name, "Read");
                assert_eq!(arguments["file_path"], "/tmp/x.txt");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
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
    fn stop_reason_reflects_the_limit_and_tool_calls() {
        let summary = |n: u32| GenSummary {
            source: "pipeline:a→b".into(),
            tokens_generated: n,
            tokens_per_sec: 1.0,
            duration_ms: 10,
            partial: false,
            trace: None,
        };
        assert_eq!(stop_reason(&summary(5), 100, false), "end_turn");
        assert_eq!(stop_reason(&summary(100), 100, false), "max_tokens");
        assert_eq!(stop_reason(&summary(5), 100, true), "tool_use");
    }

    // ── content_blocks ───────────────────────────────────────────────────────

    #[test]
    fn content_blocks_builds_a_tool_use_block_with_a_toolu_prefixed_id() {
        let parts = vec![
            OutputPart::Text("ok, checking".into()),
            OutputPart::ToolCall {
                name: "Bash".into(),
                arguments: json!({"command": "ls"}),
            },
        ];
        let (blocks, saw_tool_call) = content_blocks(&parts);
        assert!(saw_tool_call);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert!(blocks[1]["id"].as_str().unwrap().starts_with("toolu_"));
        assert_eq!(blocks[1]["name"], "Bash");
    }

    #[test]
    fn content_blocks_defaults_to_one_empty_text_block_when_there_is_nothing() {
        let (blocks, saw_tool_call) = content_blocks(&[]);
        assert!(!saw_tool_call);
        assert_eq!(blocks, vec![json!({"type": "text", "text": ""})]);
    }

    // ── apply_scan_events ────────────────────────────────────────────────────

    #[test]
    fn apply_scan_events_streams_plain_text_at_the_current_index() {
        let mut text_open = Some(0u32);
        let mut next_index = 1u32;
        let mut saw_tool_call = false;
        let mut out = Vec::new();
        apply_scan_events(
            vec![ScanEvent::Text("hi".into())],
            &mut text_open,
            &mut next_index,
            &mut saw_tool_call,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "content_block_delta");
        assert_eq!(out[0].1["index"], 0);
        assert_eq!(out[0].1["delta"]["text"], "hi");
        assert!(!saw_tool_call);
    }

    #[test]
    fn apply_scan_events_opens_a_tool_use_block_and_closes_the_text_block_first() {
        let mut text_open = Some(0u32);
        let mut next_index = 1u32;
        let mut saw_tool_call = false;
        let mut out = Vec::new();
        apply_scan_events(
            vec![ScanEvent::ToolCallClose(Ok((
                "Bash".into(),
                json!({"command": "ls"}),
            )))],
            &mut text_open,
            &mut next_index,
            &mut saw_tool_call,
            &mut out,
        );
        assert!(saw_tool_call);
        let names: Vec<&str> = out.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
            ]
        );
        assert_eq!(out[0].1["index"], 0); // closed the open text block first
        assert_eq!(out[1].1["index"], 1); // new tool_use block at the next index
        assert_eq!(out[1].1["content_block"]["name"], "Bash");
        assert_eq!(text_open, None); // no text block left open
        assert_eq!(next_index, 2);
    }

    #[test]
    fn apply_scan_events_reopens_a_text_block_lazily_after_a_tool_call() {
        let mut text_open: Option<u32> = None; // as it is right after a successful tool call
        let mut next_index = 2u32;
        let mut saw_tool_call = true;
        let mut out = Vec::new();
        apply_scan_events(
            vec![ScanEvent::Text("done".into())],
            &mut text_open,
            &mut next_index,
            &mut saw_tool_call,
            &mut out,
        );
        let names: Vec<&str> = out.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["content_block_start", "content_block_delta"]);
        assert_eq!(out[0].1["index"], 2);
        assert_eq!(text_open, Some(2));
        assert_eq!(next_index, 3);
    }

    #[test]
    fn apply_scan_events_falls_back_malformed_tool_call_into_the_open_text_block() {
        let mut text_open = Some(0u32);
        let mut next_index = 1u32;
        let mut saw_tool_call = false;
        let mut out = Vec::new();
        apply_scan_events(
            vec![ScanEvent::ToolCallClose(Err(
                "<tool_call>bad</tool_call>".into(),
            ))],
            &mut text_open,
            &mut next_index,
            &mut saw_tool_call,
            &mut out,
        );
        assert!(!saw_tool_call);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "content_block_delta");
        assert_eq!(out[0].1["index"], 0); // same block, no index bump
        assert_eq!(out[0].1["delta"]["text"], "<tool_call>bad</tool_call>");
    }
}
