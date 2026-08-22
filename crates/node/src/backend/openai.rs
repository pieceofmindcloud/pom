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

use crate::auth::ApiKeyAuth;
use crate::gateway::{GatewayState, GenEvent};
use crate::tool_call::{
    parse_output, render_tool_call, render_tools_block, OutputPart, ScanEvent, TagScanner, ToolDef,
};
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
use std::collections::VecDeque;
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

pub(crate) async fn preflight() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, cors_headers())
}

/// The API is unauthenticated on a trusted LAN; allowing any origin keeps
/// browser-based clients working without a proxy.
pub(crate) fn cors_headers() -> HeaderMap {
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
    /// Present on an assistant message that called a tool — OpenAI puts
    /// these in a flat array alongside `content`, unlike Anthropic's ordered
    /// content blocks. `id`/`tool_call_id` aren't needed for rendering (the
    /// model doesn't need to see its own prior call's id, just what it was),
    /// so — like `tool_choice` — they're accepted without a struct field.
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallRef>>,
}

fn default_content() -> Content {
    Content::Null
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallRef {
    #[serde(default)]
    pub function: Option<FunctionCallRef>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FunctionCallRef {
    pub name: String,
    /// OpenAI encodes this as a JSON **string**, not a nested object like
    /// Anthropic's `input` — has to be re-parsed wherever it's rendered back
    /// into a `<tool_call>` tag.
    #[serde(default)]
    pub arguments: String,
}

impl ChatMessage {
    /// Content text empty *and* no tool calls. The plain
    /// `!content.as_text().trim().is_empty()` check used before tool support
    /// existed silently dropped an assistant message that was *only* a tool
    /// call (`content: null`, non-empty `tool_calls`) — the call would never
    /// reach the model on replay.
    fn is_empty(&self) -> bool {
        self.content.as_text().trim().is_empty()
            && self.tool_calls.as_ref().is_none_or(|c| c.is_empty())
    }

    /// Content text, then each tool call rendered as `<tool_call>` — the
    /// exact shape the model itself emits, so replayed history stays
    /// self-consistent with what it wrote.
    fn rendered_body(&self) -> String {
        let mut parts = Vec::new();
        let text = self.content.as_text();
        if !text.trim().is_empty() {
            parts.push(text);
        }
        if let Some(calls) = &self.tool_calls {
            for tc in calls {
                if let Some(f) = &tc.function {
                    let arguments: serde_json::Value =
                        serde_json::from_str(&f.arguments).unwrap_or_else(|_| json!({}));
                    parts.push(render_tool_call(&f.name, &arguments));
                }
            }
        }
        parts.join("\n")
    }
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
    #[serde(default)]
    pub tools: Vec<ChatToolDef>,
}

/// OpenAI wraps each tool in `{"type":"function","function":{...}}`, unlike
/// Anthropic's flat `{name, description, input_schema}` — `ToolDef` (shared
/// with `crate::messages`) matches the latter, so this unwraps into it via
/// `ChatToolDef::into_tool_def` rather than pretending the two wire shapes
/// are the same.
#[derive(Debug, Deserialize)]
pub struct ChatToolDef {
    function: ChatFunctionDef,
}

#[derive(Debug, Deserialize)]
struct ChatFunctionDef {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: serde_json::Value,
}

impl ChatToolDef {
    fn into_tool_def(self) -> ToolDef {
        ToolDef {
            name: self.function.name,
            description: self.function.description,
            input_schema: self.function.parameters,
        }
    }
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

/// Flatten a conversation (plus any declared tools) into the single prompt
/// string the pipeline takes.
///
/// The first peer already applies the model's chat template once, so
/// emitting a second one here would nest one inside the other. Instead the
/// turns are labelled in plain text and handed over as one user message.
///
/// A lone user message with no tools passes through untouched, so a simple
/// client behaves exactly like `POST /api/generate` does today.
pub fn render_prompt(messages: &[ChatMessage], tools: &[ToolDef]) -> String {
    let non_empty: Vec<&ChatMessage> = messages.iter().filter(|m| !m.is_empty()).collect();

    let tools_block = render_tools_block(tools);

    if tools_block.is_empty() {
        if let [only] = non_empty.as_slice() {
            if only.role == "user" {
                return only.content.as_text();
            }
        }
    }

    let mut system: Vec<String> = Vec::new();
    let mut turns: Vec<String> = Vec::new();

    for m in &non_empty {
        match m.role.as_str() {
            "system" | "developer" => system.push(m.content.as_text()),
            "assistant" => turns.push(format!("Assistant: {}", m.rendered_body())),
            "tool" | "function" => {
                let text = m.content.as_text();
                turns.push(format!("User: <tool_response>\n{text}\n</tool_response>"));
            }
            _ => turns.push(format!("User: {}", m.content.as_text())),
        }
    }

    let mut system_text = system.join("\n\n");
    if !tools_block.is_empty() {
        if !system_text.trim().is_empty() {
            system_text.push_str("\n\n");
        }
        system_text.push_str(&tools_block);
    }

    let mut out = String::new();
    if !system_text.trim().is_empty() {
        out.push_str(system_text.trim());
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
pub(crate) fn approx_tokens(text: &str) -> u32 {
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
/// Os nomes que um cliente pode pedir.
///
/// Além do rótulo pelado de cada modelo, cada implantação aparece como
/// `<modelo>@<deploy_id>`. É o que dá ao cliente como escolher **qual**
/// implantação atende quando o mesmo modelo está no ar mais de uma vez; o nome
/// pelado continua valendo e vai para a que quem monta o pipeline escolher.
pub(crate) async fn model_ids(state: &GatewayState) -> Vec<String> {
    let peers = state.all_peers().await;
    let mut ids: Vec<String> = peers
        .iter()
        .filter(|p| !p.model.is_empty())
        .map(|p| p.model.clone())
        .collect();
    for p in &peers {
        if !p.model.is_empty() && !p.deploy_id.is_empty() {
            ids.push(format!("{}@{}", p.model, p.deploy_id));
        }
    }
    let active = state.active_model().await;
    if !active.is_empty() {
        ids.push(active);
    }
    ids.sort();
    ids.dedup();
    ids
}

async fn list_models(_auth: ApiKeyAuth, State(state): State<GatewayState>) -> impl IntoResponse {
    let created = chrono::Utc::now().timestamp();
    let data: Vec<ModelObject> = model_ids(&state)
        .await
        .into_iter()
        .map(|id| ModelObject {
            id,
            object: "model",
            created,
            owned_by: "pom",
        })
        .collect();

    (
        cors_headers(),
        Json(json!({ "object": "list", "data": data })),
    )
}

async fn get_model(
    _auth: ApiKeyAuth,
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
                "owned_by": "pom",
            })),
        )
            .into_response()
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            format!("modelo '{id}' não existe nesta rede"),
        )
    }
}

// ── Chat completions ─────────────────────────────────────────────────────────

async fn chat_completions(
    auth: ApiKeyAuth,
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
    let max_tokens = req.max_tokens();
    let tools: Vec<ToolDef> = req
        .tools
        .into_iter()
        .map(ChatToolDef::into_tool_def)
        .collect();
    let prompt = render_prompt(&req.messages, &tools);
    let prompt_tokens = approx_tokens(&prompt);

    if let Some(cfg) = state.runtime.config().await {
        if let Some(error) = crate::gateway::prompt_budget_error(&cfg, &prompt) {
            return error_response(StatusCode::PAYLOAD_TOO_LARGE, error);
        }
    }

    info!(
        "🤖 /v1/chat/completions model={model} stream={} msgs={} tools={} prompt_len={}",
        req.stream,
        req.messages.len(),
        tools.len(),
        prompt.len()
    );

    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();

    if req.stream {
        stream_response(
            state,
            auth.into_caller(),
            "/v1/chat/completions",
            model,
            prompt,
            max_tokens,
            id,
            created,
            Mode::Chat,
        )
    } else {
        blocking_response(
            state,
            auth.into_caller(),
            "/v1/chat/completions",
            model,
            prompt,
            max_tokens,
            id,
            created,
            prompt_tokens,
            Mode::Chat,
        )
        .await
    }
}

async fn completions(
    auth: ApiKeyAuth,
    State(state): State<GatewayState>,
    Json(req): Json<CompletionRequest>,
) -> Response {
    let model = match req.model.clone() {
        Some(m) if !m.is_empty() => resolve_model(&state, m).await,
        _ => state.active_model().await,
    };
    let max_tokens = req
        .max_tokens
        .unwrap_or(DEFAULT_MAX_TOKENS)
        .clamp(1, HARD_MAX_TOKENS);
    let prompt_tokens = approx_tokens(&req.prompt);
    let id = format!("cmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();

    if req.stream {
        stream_response(
            state,
            auth.into_caller(),
            "/v1/completions",
            model,
            req.prompt,
            max_tokens,
            id,
            created,
            Mode::Completion,
        )
    } else {
        blocking_response(
            state,
            auth.into_caller(),
            "/v1/completions",
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
pub(crate) async fn resolve_model(state: &GatewayState, requested: String) -> String {
    if model_ids(state).await.contains(&requested) {
        return requested;
    }
    // Um `@<deploy_id>` que ainda não apareceu no anúncio de ninguém continua
    // valendo: recusar aqui trocaria "esta implantação não respondeu" por
    // "atendi noutra", que é pior - o cliente pediu uma configuração
    // específica. Quem não achar peer nenhum falha adiante, dizendo isso.
    if crate::deployment::split_selector(&requested).1.is_some() {
        return requested;
    }
    {
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
    fn applies_chat_template(self) -> bool {
        matches!(self, Self::Chat)
    }

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
    caller: crate::reqlog::Caller,
    route: &'static str,
    model: String,
    prompt: String,
    max_tokens: u32,
    id: String,
    created: i64,
    prompt_tokens: u32,
    mode: Mode,
) -> Response {
    match crate::gateway::generate_blocking(
        state,
        route,
        caller,
        model.clone(),
        prompt,
        mode.applies_chat_template(),
        max_tokens,
    )
    .await
    {
        Ok((text, summary)) => {
            let (message_or_text, saw_tool_call): (serde_json::Value, bool) = match mode {
                Mode::Chat => chat_message_json(&parse_output(&text)),
                Mode::Completion => (json!(text), false),
            };
            let finish = finish_reason(&summary, max_tokens, saw_tool_call);
            let choice = match mode {
                Mode::Chat => json!({
                    "index": 0,
                    "message": message_or_text,
                    "finish_reason": finish,
                }),
                Mode::Completion => json!({
                    "index": 0,
                    "text": message_or_text,
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
        Err(e) => error_response(StatusCode::SERVICE_UNAVAILABLE, e),
    }
}

fn finish_reason(
    summary: &crate::gateway::GenSummary,
    max_tokens: u32,
    saw_tool_call: bool,
) -> &'static str {
    if saw_tool_call {
        "tool_calls"
    } else if summary.tokens_generated >= max_tokens {
        "length"
    } else {
        "stop"
    }
}

/// An assistant message from parsed output: OpenAI's `content` is one flat
/// string-or-null slot (unlike Anthropic's ordered content-block array), so
/// text parts collapse into it rather than interleaving with tool calls.
/// `tool_calls` collects every call found, `arguments` re-serialized back to
/// a JSON string — the inverse of `ChatMessage::rendered_body`'s parsing.
fn chat_message_json(parts: &[OutputPart]) -> (serde_json::Value, bool) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for p in parts {
        match p {
            OutputPart::Text(s) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(s);
            }
            OutputPart::ToolCall { name, arguments } => {
                tool_calls.push(json!({
                    "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() },
                }));
            }
        }
    }
    let saw_tool_call = !tool_calls.is_empty();
    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() { serde_json::Value::Null } else { json!(text) },
    });
    if saw_tool_call {
        message["tool_calls"] = json!(tool_calls);
    }
    (message, saw_tool_call)
}

#[allow(clippy::too_many_arguments)]
fn stream_response(
    state: GatewayState,
    caller: crate::reqlog::Caller,
    route: &'static str,
    model: String,
    prompt: String,
    max_tokens: u32,
    id: String,
    created: i64,
    mode: Mode,
) -> Response {
    let (tx, rx) = mpsc::channel::<GenEvent>(64);
    crate::gateway::spawn_logged_generation(
        state,
        route,
        caller,
        model.clone(),
        prompt,
        mode.applies_chat_template(),
        max_tokens,
        tx,
    );

    let stream = sse_stream(rx, id, created, model, mode, max_tokens);
    (
        cors_headers(),
        Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()),
    )
        .into_response()
}

/// Turns scanner events into OpenAI streaming `delta` values. Unlike
/// Anthropic's content blocks, OpenAI's delta model has no block-index
/// concept for plain text at all — just one implicit content stream, plus a
/// separately-indexed `tool_calls` array (`tool_call_index`, bumped once per
/// completed call). Synchronous and free of any axum/SSE type so it's
/// directly unit-testable.
///
/// Tool-call arguments are emitted as a single delta carrying the whole JSON
/// blob, not truly incrementally — same reasoning as the Anthropic adapter:
/// the tool's name is only known once the entire `<tool_call>` tag closes,
/// so there is nothing to stream before that point.
fn apply_scan_events_openai(
    events: Vec<ScanEvent>,
    tool_call_index: &mut u32,
    saw_tool_call: &mut bool,
    out: &mut Vec<serde_json::Value>,
) {
    for e in events {
        match e {
            ScanEvent::Text(s) if !s.is_empty() => out.push(json!({ "content": s })),
            ScanEvent::Text(_) | ScanEvent::ToolCallOpen | ScanEvent::ToolCallChunk(_) => {}
            ScanEvent::ToolCallClose(Ok((name, arguments))) => {
                *saw_tool_call = true;
                let idx = *tool_call_index;
                *tool_call_index += 1;
                let id = format!("call_{}", uuid::Uuid::new_v4().simple());
                out.push(json!({
                    "tool_calls": [{
                        "index": idx, "id": id, "type": "function",
                        "function": { "name": name, "arguments": "" },
                    }]
                }));
                out.push(json!({
                    "tool_calls": [{
                        "index": idx,
                        "function": { "arguments": arguments.to_string() },
                    }]
                }));
            }
            ScanEvent::ToolCallClose(Err(raw)) => {
                apply_scan_events_openai(
                    vec![ScanEvent::Text(raw)],
                    tool_call_index,
                    saw_tool_call,
                    out,
                );
            }
        }
    }
}

/// Turn generation events into the SSE chunk sequence clients expect:
/// a role-only opening chunk, one chunk per token/tool-call delta (see
/// `apply_scan_events_openai`), a chunk carrying `finish_reason`, then the
/// literal `[DONE]` sentinel. `Mode::Completion` never touches the scanner —
/// the real API has no tool-calling concept there either — so that path
/// stays byte-for-byte what it was before tool support existed.
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
        queue: VecDeque<Event>,
        scanner: TagScanner,
        tool_call_index: u32,
        saw_tool_call: bool,
    }

    let state = S {
        rx,
        phase: Phase::Opening,
        id,
        created,
        model,
        mode,
        max_tokens,
        queue: VecDeque::new(),
        scanner: TagScanner::new(),
        tool_call_index: 0,
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
                    if s.mode == Mode::Chat {
                        let chunk = chunk_json(
                            &s.id,
                            s.created,
                            &s.model,
                            s.mode,
                            json!({ "role": "assistant", "content": "" }),
                            None,
                            None,
                        );
                        s.queue.push_back(Event::default().data(chunk));
                    }
                    continue;
                }
                Phase::Streaming => match s.rx.recv().await {
                    Some(GenEvent::Token(text)) => {
                        match s.mode {
                            Mode::Completion => {
                                let chunk = chunk_json(
                                    &s.id,
                                    s.created,
                                    &s.model,
                                    s.mode,
                                    json!(text),
                                    None,
                                    None,
                                );
                                s.queue.push_back(Event::default().data(chunk));
                            }
                            Mode::Chat => {
                                let events = s.scanner.feed(&text);
                                let mut deltas = Vec::new();
                                apply_scan_events_openai(
                                    events,
                                    &mut s.tool_call_index,
                                    &mut s.saw_tool_call,
                                    &mut deltas,
                                );
                                for delta in deltas {
                                    let chunk = chunk_json(
                                        &s.id, s.created, &s.model, s.mode, delta, None, None,
                                    );
                                    s.queue.push_back(Event::default().data(chunk));
                                }
                            }
                        }
                        continue;
                    }
                    Some(GenEvent::Done(summary)) => {
                        if s.mode == Mode::Chat {
                            let scanner = std::mem::replace(&mut s.scanner, TagScanner::new());
                            let finish_events = scanner.finish();
                            let mut deltas = Vec::new();
                            apply_scan_events_openai(
                                finish_events,
                                &mut s.tool_call_index,
                                &mut s.saw_tool_call,
                                &mut deltas,
                            );
                            for delta in deltas {
                                let chunk = chunk_json(
                                    &s.id, s.created, &s.model, s.mode, delta, None, None,
                                );
                                s.queue.push_back(Event::default().data(chunk));
                            }
                        }
                        let reason = finish_reason(&summary, s.max_tokens, s.saw_tool_call);
                        let delta = match s.mode {
                            Mode::Chat => json!({}),
                            Mode::Completion => json!(""),
                        };
                        let chunk = chunk_json(
                            &s.id,
                            s.created,
                            &s.model,
                            s.mode,
                            delta,
                            Some(reason),
                            Some(&summary),
                        );
                        s.queue.push_back(Event::default().data(chunk));
                        s.queue.push_back(Event::default().data("[DONE]"));
                        s.phase = Phase::Ended;
                        continue;
                    }
                    Some(GenEvent::Failed(err)) => {
                        let payload =
                            json!({ "error": { "message": err, "type": "server_error" } });
                        s.queue
                            .push_back(Event::default().data(payload.to_string()));
                        s.queue.push_back(Event::default().data("[DONE]"));
                        s.phase = Phase::Ended;
                        continue;
                    }
                    None => {
                        s.phase = Phase::Ended;
                        s.queue.push_back(Event::default().data("[DONE]"));
                        continue;
                    }
                },
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
    summary: Option<&crate::gateway::GenSummary>,
) -> String {
    let choice = match mode {
        Mode::Chat => json!({ "index": 0, "delta": delta, "finish_reason": finish }),
        Mode::Completion => json!({ "index": 0, "text": delta, "finish_reason": finish }),
    };
    let mut payload = json!({
        "id": id,
        "object": mode.object(true),
        "created": created,
        "model": model,
        "choices": [choice],
    });
    if let Some(summary) = summary {
        payload["x_pom"] = json!({
            "source": summary.source,
            "tokens_per_sec": summary.tokens_per_sec,
            "tokens_generated": summary.tokens_generated,
            "duration_ms": summary.duration_ms,
            "partial": summary.partial,
            "trace": summary.trace,
        });
    }
    payload.to_string()
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
            tool_calls: None,
        }
    }

    // ── Escolha de implantação ───────────────────────────────────────────────

    /// Cada implantação aparece na lista com o próprio sufixo, para o cliente
    /// ter como pedir uma configuração específica.
    #[tokio::test]
    async fn deployments_are_listed_alongside_the_plain_model_name() {
        let mut p1 = crate::gateway::testkit::peer("n1", "n1", 8080);
        let mut p2 = crate::gateway::testkit::peer("n2", "n2", 8080);
        p1.model = "qwen3:8b".into();
        p1.deploy_id = "d-aaaaaaaa".into();
        p2.model = "qwen3:8b".into();
        p2.deploy_id = "d-bbbbbbbb".into();

        let state = crate::gateway::testkit::state_with("eu", vec![p1, p2]).await;
        let ids = model_ids(&state).await;

        assert!(ids.contains(&"qwen3:8b".to_string()), "{ids:?}");
        assert!(ids.contains(&"qwen3:8b@d-aaaaaaaa".to_string()), "{ids:?}");
        assert!(ids.contains(&"qwen3:8b@d-bbbbbbbb".to_string()), "{ids:?}");
    }

    /// Pedir uma implantação que ninguém anuncia **não** pode cair calado na
    /// ativa: quem escreveu o sufixo pediu aquela configuração, e responder
    /// com outra é pior que falhar.
    #[tokio::test]
    async fn an_explicit_deployment_is_never_silently_swapped() {
        let mut p1 = crate::gateway::testkit::peer("n1", "n1", 8080);
        p1.model = "qwen3:8b".into();
        p1.deploy_id = "d-aaaaaaaa".into();
        let state = crate::gateway::testkit::state_with("eu", vec![p1]).await;

        assert_eq!(
            resolve_model(&state, "qwen3:8b@d-cccccccc".into()).await,
            "qwen3:8b@d-cccccccc"
        );
        // Um nome pelado desconhecido continua caindo na ativa, como sempre.
        assert_ne!(
            resolve_model(&state, "modelo-que-nao-existe".into()).await,
            "modelo-que-nao-existe"
        );
    }

    // ── render_prompt ────────────────────────────────────────────────────────

    #[test]
    fn chat_requests_enable_template_but_completions_stay_raw() {
        assert!(Mode::Chat.applies_chat_template());
        assert!(!Mode::Completion.applies_chat_template());
    }

    #[tokio::test]
    async fn chat_endpoint_delivers_the_template_mode_with_the_user_prompt() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None::<serde_json::Value>));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new()
            .route(
                "/health",
                axum::routing::get(|| async {
                    axum::Json(json!({"status": "ok", "model_loaded": true}))
                }),
            )
            .route(
                "/v1/prefill",
                axum::routing::post({
                    let seen = seen.clone();
                    move |body: axum::body::Bytes| {
                        let seen = seen.clone();
                        async move {
                            let (meta, _) = crate::wire::decode_inference_request(&body).unwrap();
                            *seen.lock().unwrap() = Some(meta);
                            let response = json!({
                                "done": true,
                                "more_chunks": false,
                                "token_id": 1,
                                "text": "ok"
                            });
                            let bytes = crate::wire::encode_batch_single(
                                &response,
                                None,
                                crate::wire::WireDtype::F16,
                            )
                            .unwrap();
                            (
                                StatusCode::OK,
                                [("content-type", "application/octet-stream")],
                                bytes,
                            )
                                .into_response()
                        }
                    }
                }),
            )
            .route(
                "/v1/session/release",
                axum::routing::post(|| async { StatusCode::OK }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut peer = crate::gateway::testkit::peer("head", "head", 8080);
        peer.model = "lab".into();
        peer.start_layer = 0;
        peer.end_layer = 0;
        peer.total_layers = 1;
        peer.inference_port = Some(port);
        let state = crate::gateway::testkit::state_with("eu", vec![peer]).await;
        state
            .runtime
            .seed_config_for_test(crate::gateway::testkit::spec_config())
            .await;

        let response = chat_completions(
            ApiKeyAuth(crate::reqlog::Caller::anonymous()),
            State(state),
            Json(ChatRequest {
                model: Some("lab".into()),
                messages: vec![msg("user", "Oi, teste")],
                stream: false,
                max_tokens: Some(1),
                max_completion_tokens: None,
                tools: Vec::new(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let meta = seen
            .lock()
            .unwrap()
            .clone()
            .expect("the chat request reached the inference head");
        assert_eq!(meta["prompt"], "Oi, teste");
        assert_eq!(meta["chat_template"], true);
    }

    #[tokio::test]
    async fn oversized_streaming_chat_is_rejected_before_sse_starts() {
        let state = crate::gateway::testkit::state_with("eu", Vec::new()).await;
        state
            .runtime
            .seed_config_for_test(crate::gateway::testkit::spec_config())
            .await;

        let response = chat_completions(
            ApiKeyAuth(crate::reqlog::Caller::anonymous()),
            State(state),
            Json(ChatRequest {
                model: Some("lab".into()),
                messages: vec![
                    msg("system", &"agent instructions ".repeat(2_000)),
                    msg("user", "hello"),
                ],
                stream: true,
                max_tokens: Some(32_000),
                max_completion_tokens: None,
                tools: Vec::new(),
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn single_user_message_passes_through_unchanged() {
        // Must stay byte-identical to what /api/generate would receive, so the
        // simple path behaves the same in both APIs.
        assert_eq!(
            render_prompt(&[msg("user", "O que e inferencia distribuida?")], &[]),
            "O que e inferencia distribuida?"
        );
    }

    #[test]
    fn system_prompt_leads_the_conversation() {
        let out = render_prompt(
            &[msg("system", "Fale em portugues."), msg("user", "Oi")],
            &[],
        );
        assert!(out.starts_with("Fale em portugues."));
        assert!(out.ends_with("User: Oi"));
    }

    #[test]
    fn multi_turn_conversation_is_labelled() {
        let out = render_prompt(
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
    fn empty_messages_are_dropped() {
        let out = render_prompt(&[msg("system", "   "), msg("user", "oi")], &[]);
        assert_eq!(out, "oi");
    }

    #[test]
    fn content_parts_are_concatenated() {
        let m = ChatMessage {
            role: "user".into(),
            content: Content::Parts(vec![
                ContentPart {
                    text: Some("linha 1".into()),
                },
                ContentPart {
                    text: Some("linha 2".into()),
                },
            ]),
            tool_calls: None,
        };
        assert_eq!(render_prompt(&[m], &[]), "linha 1\nlinha 2");
    }

    #[test]
    fn parses_array_content_from_real_clients() {
        let raw = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"oi"}]}]}"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(render_prompt(&req.messages, &[]), "oi");
        assert!(!req.stream);
    }

    #[test]
    fn parses_null_content_on_assistant_messages() {
        let raw =
            r#"{"messages":[{"role":"assistant","content":null},{"role":"user","content":"oi"}]}"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(render_prompt(&req.messages, &[]), "oi");
    }

    /// Mirrors `chat_completions`'s own unwrap-the-wire-shape step.
    fn tool_defs(req: &ChatRequest) -> Vec<ToolDef> {
        req.tools
            .iter()
            .map(|t| ToolDef {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                input_schema: t.function.parameters.clone(),
            })
            .collect()
    }

    #[test]
    fn tool_role_message_renders_as_a_tool_response() {
        let raw = r#"{"messages":[
            {"role":"user","content":"list files"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]},
            {"role":"tool","tool_call_id":"call_1","content":"a.txt\nb.txt"}
        ]}"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        let out = render_prompt(&req.messages, &tool_defs(&req));
        assert!(out.contains("<tool_call>"));
        assert!(out.contains("<function=Bash>"));
        assert!(out.contains("<parameter=command>\nls\n</parameter>"));
        assert!(out.contains("User: <tool_response>\na.txt\nb.txt\n</tool_response>"));
    }

    #[test]
    fn assistant_message_with_only_tool_calls_is_not_dropped() {
        // Regression guard: content:null + non-empty tool_calls used to be
        // indistinguishable from a genuinely empty message and get filtered
        // out of the prompt entirely, silently losing the call on replay.
        let raw = r#"{"messages":[
            {"role":"user","content":"list files"},
            {"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"ls\"}"}}]}
        ]}"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        let out = render_prompt(&req.messages, &tool_defs(&req));
        assert!(out.contains("Assistant:"));
        assert!(out.contains("<tool_call>"));
        assert!(out.contains("Bash"));
    }

    #[test]
    fn parses_a_request_with_tools_and_the_shortcut_yields_to_the_tools_block() {
        // OpenAI wraps each tool in {"type":"function","function":{...}},
        // unlike Anthropic's flat shape — this is the wire format Codex CLI
        // / aider / LangChain actually send.
        let raw = r#"{
            "model": "gpt-4",
            "messages": [{"role":"user","content":"list files"}],
            "tools": [{"type":"function","function":{"name":"Bash","description":"Run a shell command","parameters":{"type":"object","properties":{"command":{"type":"string"}}}}}]
        }"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].function.name, "Bash");
        let out = render_prompt(&req.messages, &tool_defs(&req));
        assert!(out.contains("<tools>"));
        assert!(out.contains("Bash"));
        assert!(out.ends_with("User: list files"));
    }

    #[test]
    fn no_tools_renders_identically_to_before_tool_support() {
        let out = render_prompt(
            &[msg("system", "Fale em portugues."), msg("user", "Oi")],
            &[],
        );
        assert_eq!(out, "Fale em portugues.\n\nUser: Oi");
    }

    #[test]
    fn max_tokens_is_clamped_and_defaulted() {
        let mk = |json: &str| -> ChatRequest { serde_json::from_str(json).unwrap() };
        assert_eq!(mk(r#"{"messages":[]}"#).max_tokens(), DEFAULT_MAX_TOKENS);
        assert_eq!(mk(r#"{"messages":[],"max_tokens":10}"#).max_tokens(), 10);
        assert_eq!(
            mk(r#"{"messages":[],"max_tokens":2048}"#).max_tokens(),
            2048
        );
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
    fn finish_reason_reflects_the_limit_and_tool_calls() {
        let summary = |n: u32| crate::gateway::GenSummary {
            source: "pipeline:a→b".into(),
            tokens_generated: n,
            tokens_per_sec: 1.0,
            duration_ms: 10,
            partial: false,
            trace: None,
        };
        assert_eq!(finish_reason(&summary(5), 100, false), "stop");
        assert_eq!(finish_reason(&summary(100), 100, false), "length");
        assert_eq!(finish_reason(&summary(5), 100, true), "tool_calls");
    }

    // ── chat_message_json ────────────────────────────────────────────────────

    #[test]
    fn chat_message_json_with_no_calls_is_plain_content() {
        let (message, saw_tool_call) = chat_message_json(&[OutputPart::Text("hi there".into())]);
        assert!(!saw_tool_call);
        assert_eq!(message["content"], "hi there");
        assert!(message.get("tool_calls").is_none());
    }

    #[test]
    fn chat_message_json_with_text_and_a_call_has_both() {
        let parts = vec![
            OutputPart::Text("checking now".into()),
            OutputPart::ToolCall {
                name: "Bash".into(),
                arguments: json!({"command": "ls"}),
            },
        ];
        let (message, saw_tool_call) = chat_message_json(&parts);
        assert!(saw_tool_call);
        assert_eq!(message["content"], "checking now");
        let calls = message["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0]["id"].as_str().unwrap().starts_with("call_"));
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "Bash");
        // arguments re-serialized back to a JSON string, OpenAI's convention.
        assert_eq!(calls[0]["function"]["arguments"], r#"{"command":"ls"}"#);
    }

    #[test]
    fn chat_message_json_with_only_a_call_has_null_content() {
        let parts = vec![OutputPart::ToolCall {
            name: "Bash".into(),
            arguments: json!({}),
        }];
        let (message, saw_tool_call) = chat_message_json(&parts);
        assert!(saw_tool_call);
        assert!(message["content"].is_null());
    }

    // ── apply_scan_events_openai ─────────────────────────────────────────────

    #[test]
    fn apply_scan_events_openai_emits_a_content_delta_for_plain_text() {
        let mut tool_call_index = 0u32;
        let mut saw_tool_call = false;
        let mut out = Vec::new();
        apply_scan_events_openai(
            vec![ScanEvent::Text("hi".into())],
            &mut tool_call_index,
            &mut saw_tool_call,
            &mut out,
        );
        assert_eq!(out, vec![json!({ "content": "hi" })]);
        assert!(!saw_tool_call);
    }

    #[test]
    fn apply_scan_events_openai_emits_a_two_delta_tool_call_sequence() {
        let mut tool_call_index = 0u32;
        let mut saw_tool_call = false;
        let mut out = Vec::new();
        apply_scan_events_openai(
            vec![ScanEvent::ToolCallClose(Ok((
                "Bash".into(),
                json!({"command": "ls"}),
            )))],
            &mut tool_call_index,
            &mut saw_tool_call,
            &mut out,
        );
        assert!(saw_tool_call);
        assert_eq!(out.len(), 2);
        let first = &out[0]["tool_calls"][0];
        assert_eq!(first["index"], 0);
        assert_eq!(first["type"], "function");
        assert_eq!(first["function"]["name"], "Bash");
        assert_eq!(first["function"]["arguments"], "");
        let second = &out[1]["tool_calls"][0];
        assert_eq!(second["index"], 0);
        assert_eq!(second["function"]["arguments"], r#"{"command":"ls"}"#);
        assert_eq!(tool_call_index, 1);
    }

    #[test]
    fn apply_scan_events_openai_indexes_two_calls_separately() {
        let mut tool_call_index = 0u32;
        let mut saw_tool_call = false;
        let mut out = Vec::new();
        apply_scan_events_openai(
            vec![
                ScanEvent::ToolCallClose(Ok(("A".into(), json!({})))),
                ScanEvent::ToolCallClose(Ok(("B".into(), json!({})))),
            ],
            &mut tool_call_index,
            &mut saw_tool_call,
            &mut out,
        );
        assert_eq!(out[0]["tool_calls"][0]["index"], 0);
        assert_eq!(out[2]["tool_calls"][0]["index"], 1);
        assert_eq!(tool_call_index, 2);
    }

    #[test]
    fn apply_scan_events_openai_falls_back_malformed_call_to_content() {
        let mut tool_call_index = 0u32;
        let mut saw_tool_call = false;
        let mut out = Vec::new();
        apply_scan_events_openai(
            vec![ScanEvent::ToolCallClose(Err(
                "<tool_call>bad</tool_call>".into()
            ))],
            &mut tool_call_index,
            &mut saw_tool_call,
            &mut out,
        );
        assert!(!saw_tool_call);
        assert_eq!(
            out,
            vec![json!({ "content": "<tool_call>bad</tool_call>" })]
        );
    }

    #[test]
    fn final_stream_chunk_carries_generation_summary() {
        let summary = crate::gateway::GenSummary {
            source: "pipeline:a→b".into(),
            tokens_generated: 12,
            tokens_per_sec: 4.5,
            duration_ms: 2666,
            partial: false,
            trace: Some(crate::prefill::RequestTrace::new(
                "qwen35",
                &[("no1", 0, 15)],
            )),
        };
        let payload: serde_json::Value = serde_json::from_str(&chunk_json(
            "chatcmpl-test",
            1,
            "qwen35",
            Mode::Chat,
            json!({}),
            Some("stop"),
            Some(&summary),
        ))
        .expect("JSON válido");

        assert_eq!(payload["x_pom"]["tokens_per_sec"], 4.5);
        assert_eq!(payload["x_pom"]["tokens_generated"], 12);
        assert_eq!(payload["x_pom"]["trace"]["model"], "qwen35");
    }
}
