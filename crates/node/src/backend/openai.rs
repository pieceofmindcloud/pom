//! OpenAI-compatible HTTP surface.
//!
//! A protocol adapter, nothing more: it renders chat messages into the single
//! prompt string the pipeline already accepts, calls
//! [`crate::gateway::generate_stream_with_stops`], and dresses the resulting tokens as
//! OpenAI responses. No inference behaviour is changed or duplicated here.
//!
//! Every node serves these routes, so any peer address works as an OpenAI base
//! URL — not just the primary.
//!
//! Endpoints: `GET /v1/models`, `GET /v1/models/{id}`,
//! `POST /v1/chat/completions` (with `stream: true` SSE), `POST /v1/completions`.

use crate::auth::ApiKeyAuth;
use crate::gateway::{
    log_output_limit, GatewayState, GenEvent, GenerationInput, OutputTokenLimit, StopInput,
    DEFAULT_MAX_TOKENS,
};
use crate::tool_call::{
    parse_output_truncated_with_tools, parse_output_with_tools, render_tool_call, render_tools_block, OutputPart,
    ScanEvent, TagScanner, ToolDef, ToolSignatures,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use axum::extract::DefaultBodyLimit;
use base64::Engine as _;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tracing::info;

/// Images arrive inline as base64, so axum's 2 MiB default turns a single
/// photo into a 413 before the handler sees it.
pub(crate) const PROMPT_BODY_LIMIT: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/models/:id", get(get_model))
        .route(
            "/v1/chat/completions",
            post(chat_completions).layer(DefaultBodyLimit::max(PROMPT_BODY_LIMIT)),
        )
        .route(
            "/v1/completions",
            post(completions).layer(DefaultBodyLimit::max(PROMPT_BODY_LIMIT)),
        )
        // Typed decisions in the TypeSafe Jev shape (see `crate::decisions`).
        .route("/v1/systemone", post(crate::decisions::systemone))
        .route("/v1/decisions", post(crate::decisions::systemone))
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
    /// `content: null` on assistant messages that only carry tool calls.
    Null,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub image_url: Option<ImageUrl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
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

    fn has_images(&self) -> bool {
        matches!(self, Content::Parts(parts) if parts.iter().any(|part| part.image_url.is_some()))
    }

    fn prompt_parts(&self) -> Result<(String, Vec<Vec<u8>>), String> {
        let Content::Parts(parts) = self else {
            return Ok((self.as_text(), Vec::new()));
        };
        let mut text = String::new();
        let mut images = Vec::new();
        for part in parts {
            if let Some(value) = part.text.as_deref() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
            }
            if let Some(image) = part.image_url.as_ref() {
                let Some((prefix, encoded)) = image.url.split_once(",") else {
                    return Err("image must be a base64 data URL".to_string());
                };
                if !prefix.starts_with("data:image/") || !prefix.contains(";base64") {
                    return Err("image must be a base64 data URL".to_string());
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|error| format!("invalid base64 image: {error}"))?;
                text.push_str("<__media__>");
                images.push(bytes);
            }
        }
        Ok((text, images))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRef {
    #[serde(default)]
    pub function: Option<FunctionCallRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            && !self.content.has_images()
            && self.tool_calls.as_ref().is_none_or(|c| c.is_empty())
    }

    fn rendered_body_with_images(&self) -> Result<(String, Vec<Vec<u8>>), String> {
        let mut parts = Vec::new();
        let (text, images) = self.content.prompt_parts()?;
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
        Ok((parts.join("\n"), images))
    }
}

// ── Requests ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
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
    #[serde(default)]
    pub stop: Option<StopInput>,
    /// Optional explicit layer-0 peer chosen by the Chat UI. It is ignored
    /// when the peer is not a live head for the requested model.
    #[serde(default)]
    pub inference_head: Option<String>,
    /// The Chat's per-inference choice. Absent keeps the enabled default;
    /// the inference peer still forces `false` when the deployment turned
    /// thinking off globally.
    #[serde(default)]
    pub thinking_enabled: Option<bool>,
    /// Names the conversation this turn belongs to, for a plugin engine's K/V sessions: the next turn with the same
    /// name finds the session this one saved (`plugin_session`, `docs/subsystems/inference.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// `false` keeps retrieved context (`crate::rag`) out of this request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rag: Option<bool>,
    /// Keeps the retrieval to one collection of the RAG server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rag_collection: Option<String>,
}

/// OpenAI wraps each tool in `{"type":"function","function":{...}}`, unlike
/// Anthropic's flat `{name, description, input_schema}` — `ToolDef` (shared
/// with `crate::messages`) matches the latter, so this unwraps into it via
/// `ChatToolDef::into_tool_def` rather than pretending the two wire shapes
/// are the same.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChatToolDef {
    function: ChatFunctionDef,
}

#[derive(Debug, Serialize, Deserialize)]
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
            // OpenAI has no switch for this: it **always** streams
            // the arguments in fragments, as it generates. There is no field to
            // mirror, and inventing one would extend its schema. What
            // decides on this route is `EAGER_STREAMING_OPENAI`.
            eager_input_streaming: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompletionRequest {
    #[serde(default)]
    pub model: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub stop: Option<StopInput>,
}

impl ChatRequest {
    fn max_tokens(&self) -> u32 {
        self.max_tokens
            .or(self.max_completion_tokens)
            .unwrap_or(DEFAULT_MAX_TOKENS)
            .max(1)
    }
}

// ── Prompt rendering ─────────────────────────────────────────────────────────

/// Prefix cache key: the same canonical prompt that will be tokenized by the
/// head of the pipeline.
///
/// The key must have exactly the same ordering as `render_prompt_with_images`.
/// In particular, system/developer messages and tools are moved to the start of the
/// prompt. Keying by the original message order could declare a hit
/// when an agent appended a developer message at the end: the old key
/// was still a prefix, but the real prompt had changed since the first token.
/// The peer then refused the whole prefix and reprocessed the entire prompt.
pub fn conversation_cache_key(messages: &[ChatMessage], tools: &[ToolDef]) -> String {
    render_prompt(messages, tools)
}

/// The question retrieval searches for: the text of the last message, when a
/// person wrote it. A tool result, an assistant turn or a system message at
/// the end is an agent in the middle of its own loop, not a question.
pub(crate) fn last_user_question(messages: &[ChatMessage]) -> Option<String> {
    let last = messages.iter().rev().find(|message| !message.is_empty())?;
    if last.role != "user" {
        return None;
    }
    let text = last.content.as_text();
    (!text.trim().is_empty()).then_some(text)
}

/// A probe that scores nothing and only tells the head where the reusable
/// part of the prompt ends. See `prefill::DecisionProbe`.
fn retain_boundary(bytes: Option<usize>) -> Option<crate::prefill::DecisionProbe> {
    bytes.map(|bytes| crate::prefill::DecisionProbe {
        retain_prefix_bytes: Some(bytes),
        ..Default::default()
    })
}

/// Flatten a conversation (plus any declared tools) into the single prompt
/// string the pipeline takes.
///
/// The first peer already applies the model's chat template once, so
/// emitting a second one here would nest one inside the other. Instead the
/// turns are labelled in plain text and handed over as one user message.
///
/// **Every turn is labelled, the first one included.** A lone user message
/// used to pass through here raw, and from the second one on the label came in:
/// `"q1"` became `"User: q1\n\nAssistant: …"`, so the turn 1 prompt
/// was not a prefix of the turn 2 prompt and the previous turn's KV was of no
/// use. The 1→2 transition, the first moment a chat could
/// save anything, was the only one that could **never** hit. Always labelling makes
/// one turn's prompt an exact prefix of the next one's prompt, which is the property
/// the whole reuse rests on (see `crate::prefix_cache`).
///
/// A client that wants the raw text, with no label at all, has `/v1/completions` and
/// `POST /api/generate`, which pass the prompt along untouched.
#[allow(dead_code)]
pub fn render_prompt(messages: &[ChatMessage], tools: &[ToolDef]) -> String {
    render_prompt_with_images(messages, tools)
        .map(|(prompt, _)| prompt)
        .unwrap_or_default()
}

/// Renders the prompt and extracts the images as bytes for the projector. The text
/// uses mtmd's official marker in the same place as each `image_url`.
pub fn render_prompt_with_images(
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Result<(String, Vec<Vec<u8>>), String> {
    let non_empty: Vec<&ChatMessage> = messages.iter().filter(|m| !m.is_empty()).collect();

    let tools_block = render_tools_block(tools);

    let mut system: Vec<String> = Vec::new();
    let mut turns: Vec<String> = Vec::new();
    let mut images = Vec::new();

    for m in &non_empty {
        match m.role.as_str() {
            "system" | "developer" => {
                let (text, mut media) = m.rendered_body_with_images()?;
                system.push(text);
                images.append(&mut media);
            }
            "assistant" => {
                let (text, mut media) = m.rendered_body_with_images()?;
                turns.push(format!("Assistant: {text}"));
                images.append(&mut media);
            }
            "tool" | "function" => {
                let (text, mut media) = m.rendered_body_with_images()?;
                turns.push(format!("User: <tool_response>\n{text}\n</tool_response>"));
                images.append(&mut media);
            }
            _ => {
                let (text, mut media) = m.rendered_body_with_images()?;
                turns.push(format!("User: {text}"));
                images.append(&mut media);
            }
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
    Ok((out, images))
}

pub(crate) fn image_vision_error(
    images: &[Vec<u8>],
    mmproj_path: Option<&str>,
) -> Option<&'static str> {
    if !images.is_empty() && mmproj_path.is_none() {
        Some("this model has no vision")
    } else {
        None
    }
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
    #[serde(rename = "type")]
    model_type: &'static str,
    /// Anthropic's model-list contract. A local deployment has no provider
    /// release timestamp, so the documented epoch sentinel is used.
    created_at: &'static str,
    display_name: String,
    capabilities: Option<serde_json::Value>,
    max_input_tokens: Option<u32>,
    max_tokens: Option<u32>,
    /// OpenRouter-compatible model capability metadata. `None` means this
    /// gateway has no loaded deployment from which to read the limit.
    context_length: Option<u32>,
    top_provider: ModelProviderLimits,
}

#[derive(Debug, Serialize)]
struct ModelProviderLimits {
    context_length: Option<u32>,
    max_completion_tokens: Option<u32>,
}

fn model_object(
    id: String,
    created: i64,
    context_length: Option<u32>,
    max_completion_tokens: Option<u32>,
    vision: bool,
) -> ModelObject {
    ModelObject {
        display_name: id.clone(),
        id,
        object: "model",
        created,
        owned_by: "pom",
        model_type: "model",
        created_at: "1970-01-01T00:00:00Z",
        capabilities: Some(json!({ "vision": vision })),
        max_input_tokens: context_length,
        max_tokens: max_completion_tokens,
        context_length,
        top_provider: ModelProviderLimits {
            context_length,
            max_completion_tokens,
        },
    }
}

/// Models this cluster can actually serve: whatever labels the peers advertise.
/// The names a client may ask for.
///
/// Besides each model's bare label, each deployment appears as
/// `<model>@<deploy_id>`. This is what gives the client a way to choose **which**
/// deployment serves when the same model is up more than once; the bare
/// name still works and goes to the one whoever assembles the pipeline picks.
///
/// An idle peer has no `runtime_capacity` while the weights are out of
/// memory, but it is still a servable deployment: generation wakes it up
/// after planning the route. Only a peer that is not idle and has not yet
/// confirmed `/health` should hide the model from the catalog.
/// A machine that serves the whole model by itself (layers 0 to the end): it can answer any request alone.
fn covers_whole_model(p: &crate::network::PeerInfo) -> bool {
    p.total_layers > 0 && p.start_layer == 0 && p.end_layer.saturating_add(1) >= p.total_layers
}

/// Whether a group of peers that serve one model can answer: each of them is ready (a pipeline needs all of its
/// stages), or one of them is ready and serves the whole model alone (replicas do not wait for each other: a machine
/// that is loading must not hide the one that serves).
fn group_ready(group: &[&crate::network::PeerInfo]) -> bool {
    let ready = |p: &crate::network::PeerInfo| p.runtime_capacity.is_some() || p.idle;
    group.iter().all(|p| ready(p))
        || group.iter().any(|p| ready(p) && covers_whole_model(p))
}

pub(crate) fn model_ids_from_peers(peers: &[crate::network::PeerInfo]) -> Vec<String> {
    let alive_models: std::collections::BTreeSet<String> = peers
        .iter()
        .filter(|p| p.is_alive() && !p.model.is_empty())
        .map(|p| p.model.clone())
        .collect();
    let mut ids: Vec<String> = alive_models
        .into_iter()
        .filter(|model| {
            let group: Vec<&crate::network::PeerInfo> =
                peers.iter().filter(|p| p.is_alive() && p.model == *model).collect();
            group_ready(&group)
        })
        .collect();
    let deployments: std::collections::BTreeSet<(String, String)> = peers
        .iter()
        .filter(|p| p.is_alive() && !p.model.is_empty() && !p.deploy_id.is_empty())
        .map(|p| (p.model.clone(), p.deploy_id.clone()))
        .collect();
    for (model, deploy_id) in deployments {
        let group: Vec<&crate::network::PeerInfo> = peers
            .iter()
            .filter(|p| p.is_alive() && p.model == model && p.deploy_id == deploy_id)
            .collect();
        let ready = group_ready(&group);
        if ready {
            ids.push(format!("{model}@{deploy_id}"));
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

pub(crate) async fn model_ids(state: &GatewayState) -> Vec<String> {
    let peers = state.all_peers().await;
    model_ids_from_peers(&peers)
}

async fn list_models(_auth: ApiKeyAuth, State(state): State<GatewayState>) -> impl IntoResponse {
    let created = chrono::Utc::now().timestamp();
    let peers = state.all_peers().await;
    let mut data = Vec::new();
    for id in model_ids_from_peers(&peers) {
        let (context_length, max_completion_tokens) = state
            .advertised_output_limits_from_peers(&peers, Some(&id))
            .await;
        let vision = crate::webui::model_vision_supported(&state, &id).await;
        data.push(model_object(id, created, context_length, max_completion_tokens, vision));
    }

    let first_id = data.first().map(|model| model.id.clone());
    let last_id = data.last().map(|model| model.id.clone());
    (
        cors_headers(),
        Json(json!({
            "object": "list",
            "data": data,
            "first_id": first_id,
            "has_more": false,
            "last_id": last_id,
        })),
    )
}

async fn get_model(
    _auth: ApiKeyAuth,
    State(state): State<GatewayState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let peers = state.all_peers().await;
    if model_ids_from_peers(&peers).contains(&id) {
        let (context_length, max_completion_tokens) = state
            .advertised_output_limits_from_peers(&peers, Some(&id))
            .await;
        let vision = crate::webui::model_vision_supported(&state, &id).await;
        (
            cors_headers(),
            Json(model_object(
                id,
                chrono::Utc::now().timestamp(),
                context_length,
                max_completion_tokens,
                vision,
            )),
        )
            .into_response()
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            format!("model '{id}' does not exist on this network"),
        )
    }
}

// ── Chat completions ─────────────────────────────────────────────────────────

async fn chat_completions(
    auth: ApiKeyAuth,
    State(state): State<GatewayState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    // The body as the client sent it is the request's details: a plugin engine renders a request with tools from it
    // (`plugin_engine::chat_api`), and the typed request below drops what the flattened prompt does not use - a
    // tool's `"type": "function"`, the calls' ids, the results' `tool_call_id`s - which Strata's reader needs.
    let req: ChatRequest = match serde_json::from_value(body.clone()) {
        Ok(req) => req,
        Err(error) => {
            return error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Failed to deserialize the JSON body into the target type: {error}"),
            )
        }
    };
    if req.messages.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "'messages' cannot be empty");
    }
    let request_details = body;

    let model = match req.model.clone() {
        Some(m) if !m.is_empty() => match resolve_model(&state, m).await {
            Ok(model) => model,
            Err(error) => return error_response(StatusCode::NOT_FOUND, error),
        },
        _ => state.active_model().await,
    };
    let requested_max_tokens = req.max_tokens();
    let stops = req.stop.map(StopInput::into_vec).unwrap_or_default();
    let tools: Vec<ToolDef> = req
        .tools
        .into_iter()
        .map(ChatToolDef::into_tool_def)
        .collect();
    let signatures = ToolSignatures::of(&tools);
    // A demonstration page reads node ids as aliases and sends one back.
    let preferred_head = match req.inference_head.as_deref() {
        Some(head) => Some(crate::user_session::unalias_node(&state, head).await),
        None => None,
    };
    let (mut prompt, images) = match render_prompt_with_images(&req.messages, &tools) {
        Ok(rendered) => rendered,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    // Key the prefix cache by the messages, not by the rendered text -
    // see `conversation_cache_key`.
    let cache_key = conversation_cache_key(&req.messages, &tools);
    // The system part, which a new conversation of the same client shares.
    let system_only: Vec<ChatMessage> = req
        .messages
        .iter()
        .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
        .cloned()
        .collect();
    let root = render_prompt(&system_only, &tools);
    let shared_prefix =
        crate::gateway::SharedPrefix::offer(&prompt, &cache_key, &root, root.trim_end());
    let inference_token = auth.token().to_string();
    let caller = auth.into_caller();
    // Retrieval comes before the generation: passages for the user's question
    // go after the whole conversation, and the head keeps the KV only up to
    // here, so the next turn still finds its prefix. The cache key stays the
    // conversation. A request that asked for retrieval (`"rag": true`) also
    // lets a long message be reduced to search terms by a short generation.
    let mut request_details = request_details;
    let mut retain_prefix_bytes = None;
    if req.rag != Some(false) {
        if let Some(question) = last_user_question(&req.messages) {
            let rewriter = (req.rag == Some(true)).then(|| crate::rag::Rewriter {
                model: model.clone(),
                inference_token: inference_token.clone(),
                caller: caller.clone(),
                preferred_head: preferred_head.clone(),
            });
            match crate::rag::retrieve(
                &state,
                &question,
                rewriter.as_ref(),
                req.rag_collection.as_deref(),
            )
            .await
            {
                crate::rag::Outcome::Retrieved(retrieved) => {
                    retain_prefix_bytes = Some(prompt.len());
                    prompt.push_str(&crate::rag::context_block(&retrieved.passages));
                    let summary = retrieved.summary();
                    info!(
                        "📚 RAG: {} passages added from {summary}",
                        retrieved.passages.len()
                    );
                    request_details["rag"] = summary;
                }
                crate::rag::Outcome::NotNeeded { probability } => {
                    info!("📚 RAG: skipped, the message needs no documents (yes={probability:.2})");
                    request_details["rag"] = json!({
                        "skipped": "not_needed",
                        "needed": (probability * 1000.0).round() / 1000.0,
                    });
                }
                crate::rag::Outcome::Nothing => {}
            }
        }
    }
    let prompt_tokens = approx_tokens(&prompt);

    let output_limit = state
        .request_output_limit_for_head(
            Some(&model),
            preferred_head.as_deref(),
            requested_max_tokens,
            &prompt,
        )
        .await;
    log_output_limit("/v1/chat/completions", output_limit);
    if let Some(error) = output_limit.rejection_reason() {
        let status = if output_limit.capacity_pending() {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::PAYLOAD_TOO_LARGE
        };
        crate::gateway::record_rejected(
            &state,
            "/v1/chat/completions",
            caller,
            model.clone(),
            prompt,
            request_details,
            error.clone(),
        );
        return error_response(status, error);
    }
    if let Some(denial) =
        crate::quota::admit(&state, &caller, &prompt, prompt_tokens, &request_details).await
    {
        crate::gateway::record_rejected(
            &state,
            "/v1/chat/completions",
            caller,
            model.clone(),
            prompt,
            request_details,
            denial.message.clone(),
        );
        return denial.decorate(error_response(
            StatusCode::TOO_MANY_REQUESTS,
            denial.message.clone(),
        ));
    }
    let budget = output_limit.budget();
    let thinking_enabled = client_thinking(req.thinking_enabled, &request_details);

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
            inference_token.clone(),
            caller.clone(),
            "/v1/chat/completions",
            model,
            prompt,
            images,
            budget,
            id,
            created,
            Mode::Chat,
            stops,
            Some(cache_key),
            retain_prefix_bytes,
            shared_prefix,
            preferred_head.clone(),
            thinking_enabled,
            request_details.clone(),
            output_limit,
            signatures.clone(),
        )
    } else {
        blocking_response(
            state,
            inference_token,
            caller,
            "/v1/chat/completions",
            model,
            prompt,
            images,
            budget,
            id,
            created,
            prompt_tokens,
            Mode::Chat,
            stops,
            Some(cache_key),
            retain_prefix_bytes,
            shared_prefix,
            preferred_head,
            thinking_enabled,
            request_details,
            output_limit,
            signatures,
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
        Some(m) if !m.is_empty() => match resolve_model(&state, m).await {
            Ok(model) => model,
            Err(error) => return error_response(StatusCode::NOT_FOUND, error),
        },
        _ => state.active_model().await,
    };
    let requested_max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).max(1);
    let request_details = serde_json::to_value(&req).unwrap_or_else(|_| json!({}));
    let stops = req.stop.map(StopInput::into_vec).unwrap_or_default();
    let prompt_tokens = approx_tokens(&req.prompt);
    let inference_token = auth.token().to_string();
    let output_limit = state
        .request_output_limit_for_model(Some(&model), requested_max_tokens, &req.prompt)
        .await;
    log_output_limit("/v1/completions", output_limit);
    if let Some(error) = output_limit.rejection_reason() {
        let status = if output_limit.capacity_pending() {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::PAYLOAD_TOO_LARGE
        };
        return error_response(status, error);
    }
    if let Some(denial) =
        crate::quota::admit(&state, auth.caller(), &req.prompt, prompt_tokens, &request_details)
            .await
    {
        crate::gateway::record_rejected(
            &state,
            "/v1/completions",
            auth.into_caller(),
            model.clone(),
            req.prompt.clone(),
            request_details,
            denial.message.clone(),
        );
        return denial.decorate(error_response(
            StatusCode::TOO_MANY_REQUESTS,
            denial.message.clone(),
        ));
    }
    let budget = output_limit.budget();
    let id = format!("cmpl-{}", uuid::Uuid::new_v4().simple());
    let created = chrono::Utc::now().timestamp();

    if req.stream {
        stream_response(
            state,
            inference_token.clone(),
            auth.into_caller(),
            "/v1/completions",
            model,
            req.prompt,
            Vec::new(),
            budget,
            id,
            created,
            Mode::Completion,
            stops,
            // A raw completion has no conversation: the prefix is the text itself.
            None,
            None,
            Default::default(),
            None,
            true,
            request_details.clone(),
            output_limit,
            ToolSignatures::default(),
        )
    } else {
        blocking_response(
            state,
            inference_token,
            auth.into_caller(),
            "/v1/completions",
            model,
            req.prompt,
            Vec::new(),
            budget,
            id,
            created,
            prompt_tokens,
            Mode::Completion,
            stops,
            None,
            None,
            Default::default(),
            None,
            true,
            request_details,
            output_limit,
            ToolSignatures::default(),
        )
        .await
    }
}

/// Use the requested model when a peer serves it. A name no peer announces is
/// refused instead of silently swapped for whatever this cluster happens to be
/// running - a client that pinned a model, including one that does not exist,
/// used to get `200` back with a different model's output and no indication
/// of the substitution. `GET /v1/models` remains the source of truth for the
/// names a client may pass here.
pub(crate) async fn resolve_model(
    state: &GatewayState,
    requested: String,
) -> Result<String, String> {
    if model_ids(state).await.contains(&requested) {
        return Ok(requested);
    }
    // An `@<deploy_id>` that has not yet appeared in anyone's announcement still
    // holds: refusing here would swap "this deployment did not answer" for
    // "I served from another one", which is worse - the client asked for a
    // specific configuration. Whoever finds no peer at all fails ahead, saying so.
    if crate::deployment::split_selector(&requested).1.is_some() {
        return Ok(requested);
    }
    Err(format!(
        "model '{requested}' does not exist or is not loaded on this network; see GET /v1/models for the servable names"
    ))
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
    inference_token: String,
    caller: crate::reqlog::Caller,
    route: &'static str,
    model: String,
    prompt: String,
    images: Vec<Vec<u8>>,
    budget: crate::gateway::OutputBudget,
    id: String,
    created: i64,
    prompt_tokens: u32,
    mode: Mode,
    stops: Vec<String>,
    cache_key: Option<String>,
    retain_prefix_bytes: Option<usize>,
    shared_prefix: crate::gateway::SharedPrefix,
    preferred_head: Option<String>,
    thinking_enabled: bool,
    request_details: serde_json::Value,
    output_limit: OutputTokenLimit,
    tools: ToolSignatures,
) -> Response {
    match crate::gateway::generate_blocking_output_with_stops(
        route,
        caller,
        GenerationInput {
            shared_prefix,
            probe: retain_boundary(retain_prefix_bytes),
            trace_id: Some(id.clone()),
            state,
            inference_token,
            model: model.clone(),
            prompt,
            images,
            chat_template: mode.applies_chat_template(),
            budget,
            stops,
            cache_key,
            preferred_head,
            thinking_enabled,
            request_details,
        },
    )
    .await
    {
        Ok((output, summary)) => {
            // It was the ceiling that ended it, not the model. A call still open
            // here was cut in half, and this is how it comes back: structured and
            // incomplete, with `finish_reason: "length"` - what OpenAI does.
            // See `TagScanner::finish_truncated`.
            let truncated = summary.finish_reason.as_deref() == Some("length");
            let (message_or_text, saw_tool_call, truncated_tool_call): (
                serde_json::Value,
                bool,
                bool,
            ) = match mode {
                Mode::Chat => {
                    let (parts, cut_inside_call) = if truncated {
                        parse_output_truncated_with_tools(&output.text, &tools)
                    } else {
                        (parse_output_with_tools(&output.text, &tools), false)
                    };
                    let (mut message, saw) = chat_message_json(&parts);
                    if !output.reasoning.is_empty() {
                        message["reasoning_content"] = json!(output.reasoning);
                    }
                    (message, saw, cut_inside_call)
                }
                Mode::Completion => (json!(output.text), false, false),
            };
            let finish = finish_reason(&summary, budget.total, saw_tool_call);
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
                        "completion_tokens_details": {
                            "reasoning_tokens": summary.reasoning_tokens,
                        },
                        "total_tokens": prompt_tokens + summary.tokens_generated,
                        // How many prompt tokens the pipeline did **not**
                        // reprocess, measured on the side that executes. Same name and
                        // place that OpenAI uses, so the client reads it without
                        // adaptation. See `RequestTrace::cached_tokens`.
                        "prompt_tokens_details": {
                            "cached_tokens": summary
                                .trace
                                .as_ref()
                                .map(|trace| trace.cached_tokens)
                                .unwrap_or(0),
                        },
                    },
                    // Not part of the OpenAI schema; kept so the hop chain that
                    // proves distributed execution stays visible to clients.
                    "x_pom": {
                        "source": summary.source,
                        "tokens_per_sec": summary.tokens_per_sec,
                        "reasoning_tokens": summary.reasoning_tokens,
                        "duration_ms": summary.duration_ms,
                        "timings": summary.timings,
                        // `partial` is `true` whenever the response is not what
                        // the model would have given: transport loss, or the
                        // ceiling cutting a tool call in half.
                        "partial": summary.partial || truncated_tool_call,
                        "error": summary.error,
                        "finish_reason": summary.finish_reason,
                        "stop_sequence": summary.stop_sequence,
                        "trace": summary.trace,
                        "output_limits": output_limit,
                        // `true` when the client's ceiling fell **inside** a
                        // tool call. The call comes back structured and
                        // incomplete, with `length`: the client repeats the
                        // request with a larger ceiling, it does not execute.
                        "truncated_tool_call": truncated_tool_call,
                        // How many backend generations this response stitched together.
                        // `1` is a single one; greater than `1` says the output went past the
                        // deployment's `max_new_tokens` without it changing.
                        "continuation_legs": summary.legs, "payment": summary.payment,
                    },
                })),
            )
                .into_response()
        }
        Err(e) => error_response(StatusCode::SERVICE_UNAVAILABLE, e),
    }
}

/// `max_tokens` here is the request's **total**, never the ceiling of one leg of the
/// continuation loop: an exhausted leg is not `length`, it is the middle of the
/// response. Leaking the intermediate `length` to the client was the defect - the
/// harness reads `max_tokens` as a terminal condition and drops the tool
/// call halfway.
///
/// Reaching the total, that one, is a **terminal stop**: `length` comes first,
/// even with a tool call in the output, because the call the ceiling
/// caught open is incomplete and is not the end of a turn. It is OpenAI's rule, and
/// it is what makes the client repeat the request with a larger ceiling instead of executing
/// half a call.
fn finish_reason(
    summary: &crate::gateway::GenSummary,
    max_tokens: u32,
    saw_tool_call: bool,
) -> &'static str {
    if summary.tokens_generated >= max_tokens {
        "length"
    } else if saw_tool_call {
        "tool_calls"
    } else {
        "stop"
    }
}

/// An assistant message from parsed output: OpenAI's `content` is one flat
/// string-or-null slot (unlike Anthropic's ordered content-block array), so
/// text parts collapse into it rather than interleaving with tool calls.
/// `tool_calls` collects every call found, `arguments` re-serialized back to
/// a JSON string, matching the form parsed by
/// `ChatMessage::rendered_body_with_images`.
fn chat_message_json(parts: &[OutputPart]) -> (serde_json::Value, bool) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    // Only **complete** calls count: see `OutputPart::ToolCallTruncated`.
    let mut complete_calls = 0usize;
    for p in parts {
        match p {
            OutputPart::Text(s) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(s);
            }
            OutputPart::ToolCall { name, arguments } => {
                complete_calls += 1;
                tool_calls.push(json!({
                    "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() },
                }));
            }
            // The call the ceiling cut goes into the list - it is what OpenAI
            // returns -, but it does **not** count as a call seen: the turn did not
            // end on it, it ended at the ceiling, and the stop reason has to
            // say `length`.
            OutputPart::ToolCallTruncated(partial) => {
                tool_calls.push(json!({
                    "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
                    "type": "function",
                    "function": { "name": partial.name, "arguments": partial.arguments },
                }));
            }
        }
    }
    let saw_tool_call = complete_calls > 0;
    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() { serde_json::Value::Null } else { json!(text) },
    });
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    (message, saw_tool_call)
}

#[allow(clippy::too_many_arguments)]
fn stream_response(
    state: GatewayState,
    inference_token: String,
    caller: crate::reqlog::Caller,
    route: &'static str,
    model: String,
    prompt: String,
    images: Vec<Vec<u8>>,
    budget: crate::gateway::OutputBudget,
    id: String,
    created: i64,
    mode: Mode,
    stops: Vec<String>,
    cache_key: Option<String>,
    retain_prefix_bytes: Option<usize>,
    shared_prefix: crate::gateway::SharedPrefix,
    preferred_head: Option<String>,
    thinking_enabled: bool,
    request_details: serde_json::Value,
    output_limit: OutputTokenLimit,
    tools: ToolSignatures,
) -> Response {
    let (tx, rx) = mpsc::channel::<GenEvent>(64);
    crate::gateway::spawn_logged_generation_with_stops(
        route,
        caller,
        GenerationInput {
            shared_prefix,
            probe: retain_boundary(retain_prefix_bytes),
            trace_id: Some(id.clone()),
            state,
            inference_token,
            model: model.clone(),
            prompt,
            images,
            chat_template: mode.applies_chat_template(),
            budget,
            stops,
            cache_key,
            preferred_head,
            thinking_enabled,
            request_details,
        },
        tx,
    );

    let stream = sse_stream(rx, id, created, model, mode, budget.total, output_limit, tools);
    (
        cors_headers(),
        Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()),
    )
        .into_response()
}

/// OpenAI **always** streams the arguments in fragments, as it generates -
/// there is no switch for this in its API, and so there is no field to
/// mirror in `ChatToolDef`. This route is always eager.
const EAGER_STREAMING_OPENAI: bool = true;

/// The state of a tool call being streamed.
///
/// It is born at `ToolCallOpen`, grows with each `ToolCallChunk` and dies at
/// the close. While the name has not come out, nothing is streamed: an open `tool_calls`
/// index with no name is of no use to the client.
struct CallInProgress {
    /// The raw body seen so far - it is what the transcoder lives on.
    body: String,
    streamer: crate::tool_call::CallArgsStreamer,
    /// The index already announced to the client, once the opening has gone out.
    index: Option<u32>,
}

/// Turns scanner events into OpenAI streaming `delta` values. Unlike
/// Anthropic's content blocks, OpenAI's delta model has no block-index
/// concept for plain text at all — just one implicit content stream, plus a
/// separately-indexed `tool_calls` array (`tool_call_index`, bumped once per
/// call). Synchronous and free of any axum/SSE type so it's directly
/// unit-testable.
///
/// **The arguments come out in fragments, as the model generates them.** This is
/// where they used to be held back: the call body was accumulated and only became a
/// delta when the tag closed, which on ksia gave 663 characters in one piece,
/// at 11.01 s. `CallArgsStreamer` transcodes the markup into JSON as
/// it arrives, and the concatenation of the fragments is exactly the string the
/// non-streaming path would produce.
fn apply_scan_events_openai(
    events: Vec<ScanEvent>,
    tool_call_index: &mut u32,
    saw_tool_call: &mut bool,
    in_progress: &mut Option<CallInProgress>,
    out: &mut Vec<serde_json::Value>,
) {
    /// The opening: index, id and name, with the arguments still empty. It goes out
    /// only once per call, on the first fragment there is.
    fn open_call(
        name: &str,
        tool_call_index: &mut u32,
        in_progress: &mut CallInProgress,
        out: &mut Vec<serde_json::Value>,
    ) -> u32 {
        if let Some(idx) = in_progress.index {
            return idx;
        }
        let idx = *tool_call_index;
        *tool_call_index += 1;
        in_progress.index = Some(idx);
        out.push(json!({
            "tool_calls": [{
                "index": idx,
                "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
                "type": "function",
                "function": { "name": name, "arguments": "" },
            }]
        }));
        idx
    }

    fn emit_fragment(idx: u32, text: String, out: &mut Vec<serde_json::Value>) {
        out.push(json!({
            "tool_calls": [{
                "index": idx,
                "function": { "arguments": text },
            }]
        }));
    }

    for e in events {
        match e {
            ScanEvent::Text(s) if !s.is_empty() => out.push(json!({ "content": s })),
            ScanEvent::Text(_) => {}
            ScanEvent::ToolCallOpen => {
                *in_progress = Some(CallInProgress {
                    body: String::new(),
                    streamer: crate::tool_call::CallArgsStreamer::new(EAGER_STREAMING_OPENAI),
                    index: None,
                });
            }
            ScanEvent::ToolCallChunk(fragment) => {
                let Some(call) = in_progress.as_mut() else {
                    continue;
                };
                call.body.push_str(&fragment);
                let body = call.body.clone();
                if let Some(delta) = call.streamer.advance(&body) {
                    let name = crate::tool_call::peek_call_name(&body).unwrap_or_default();
                    let idx = open_call(&name, tool_call_index, call, out);
                    emit_fragment(idx, delta, out);
                }
            }
            ScanEvent::ToolCallClose(Ok((name, arguments))) => {
                *saw_tool_call = true;
                let mut call = in_progress.take().unwrap_or_else(|| CallInProgress {
                    body: String::new(),
                    streamer: crate::tool_call::CallArgsStreamer::new(EAGER_STREAMING_OPENAI),
                    index: None,
                });
                let idx = open_call(&name, tool_call_index, &mut call, out);
                // Only what has not gone out yet. Repeating what the client already
                // accumulated would duplicate its arguments.
                if let Some(delta) = call.streamer.finish_complete(&call.body, &arguments) {
                    emit_fragment(idx, delta, out);
                }
            }
            ScanEvent::ToolCallClose(Err(raw)) => {
                let call = in_progress.take();
                // A call that has already started going out cannot become text: the
                // client already has an open `tool_calls[i]` with a name, and there is no
                // way to unsend. It stays as it is - arguments that do not
                // parse, which is the "invalid JSON" case the providers
                // document and tell you to hand back to the model.
                if call.is_some_and(|c| c.streamer.emitted_len() > 0) {
                    continue;
                }
                apply_scan_events_openai(
                    vec![ScanEvent::Text(raw)],
                    tool_call_index,
                    saw_tool_call,
                    in_progress,
                    out,
                );
            }
            // The call cut by the ceiling goes out in the same form, and the arguments
            // end up in the unfinished JSON string of Phase 1 - what had already been
            // transmitted counts, so only what is missing goes out here.
            // `saw_tool_call` is **not** set: the turn ended at the ceiling, and the
            // final frame says `length`.
            ScanEvent::ToolCallTruncated(partial) => {
                let mut call = in_progress.take().unwrap_or_else(|| CallInProgress {
                    body: String::new(),
                    streamer: crate::tool_call::CallArgsStreamer::new(EAGER_STREAMING_OPENAI),
                    index: None,
                });
                let idx = open_call(&partial.name, tool_call_index, &mut call, out);
                if let Some(delta) = call.streamer.finish_truncated(&partial.arguments) {
                    emit_fragment(idx, delta, out);
                }
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
fn reasoning_delta(mode: Mode, text: String) -> serde_json::Value {
    match mode {
        // Chat clients such as Pi recognize this llama.cpp-compatible field
        // as a thinking delta.
        Mode::Chat => json!({ "reasoning_content": text }),
        // The legacy completions schema has no distinct reasoning field, so
        // preserve the generated text.
        Mode::Completion => json!(text),
    }
}

fn sse_stream(
    rx: mpsc::Receiver<GenEvent>,
    id: String,
    created: i64,
    model: String,
    mode: Mode,
    max_tokens: u32,
    output_limit: OutputTokenLimit,
    tools: ToolSignatures,
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
        output_limit: OutputTokenLimit,
        queue: VecDeque<Event>,
        scanner: TagScanner,
        tool_call_index: u32,
        saw_tool_call: bool,
        /// `true` when the client's ceiling fell inside a tool
        /// call. Goes into the final frame's `x_pom`.
        truncated_tool_call: bool,
        /// The call whose arguments are being streamed right now.
        in_progress: Option<CallInProgress>,
    }

    let state = S {
        rx,
        phase: Phase::Opening,
        id,
        created,
        model,
        mode,
        max_tokens,
        output_limit,
        queue: VecDeque::new(),
        scanner: TagScanner::with_tools(tools),
        tool_call_index: 0,
        saw_tool_call: false,
        truncated_tool_call: false,
        in_progress: None,
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
                                    &mut s.in_progress,
                                    &mut deltas,
                                );
                                for delta in deltas {
                                    let chunk = chunk_json(
                                        &s.id, s.created, &s.model, s.mode, delta, None, None, None,
                                    );
                                    s.queue.push_back(Event::default().data(chunk));
                                }
                            }
                        }
                        continue;
                    }
                    Some(GenEvent::Reasoning(text)) => {
                        let delta = reasoning_delta(s.mode, text);
                        let chunk =
                            chunk_json(&s.id, s.created, &s.model, s.mode, delta, None, None, None);
                        s.queue.push_back(Event::default().data(chunk));
                        continue;
                    }
                    Some(GenEvent::Done(summary)) => {
                        if s.mode == Mode::Chat {
                            let scanner = std::mem::replace(&mut s.scanner, TagScanner::new());
                            // The cut call can only come out structured here
                            // because nothing inside the tag was streamed:
                            // `apply_scan_events_openai` swallows `ToolCallOpen`
                            // and `ToolCallChunk`, and the scanner holds the body until
                            // the tag closes. Nothing to unsend, nothing leaked.
                            let (finish_events, cut_inside_call) =
                                if summary.finish_reason.as_deref() == Some("length") {
                                    scanner.finish_truncated()
                                } else {
                                    (scanner.finish(), false)
                                };
                            s.truncated_tool_call = cut_inside_call;
                            let mut deltas = Vec::new();
                            apply_scan_events_openai(
                                finish_events,
                                &mut s.tool_call_index,
                                &mut s.saw_tool_call,
                                &mut s.in_progress,
                                &mut deltas,
                            );
                            for delta in deltas {
                                let chunk = chunk_json(
                                    &s.id, s.created, &s.model, s.mode, delta, None, None, None,
                                );
                                s.queue.push_back(Event::default().data(chunk));
                            }
                        }
                        let reason = finish_reason(&summary, s.max_tokens, s.saw_tool_call);
                        let delta = match s.mode {
                            Mode::Chat => json!({}),
                            Mode::Completion => json!(""),
                        };
                        let chunk = chunk_json_final(
                            &s.id,
                            s.created,
                            &s.model,
                            s.mode,
                            delta,
                            Some(reason),
                            Some(&summary),
                            Some(&s.output_limit),
                            s.truncated_tool_call,
                        );
                        s.queue.push_back(Event::default().data(chunk));
                        // A peer failed after some text went out: the text stays, and
                        // the client still learns that the model did not end there.
                        if let Some(err) = &summary.error {
                            let payload =
                                json!({ "error": { "message": err, "type": "server_error" } });
                            s.queue
                                .push_back(Event::default().data(payload.to_string()));
                        }
                        s.queue.push_back(Event::default().data("[DONE]"));
                        s.phase = Phase::Ended;
                        continue;
                    }
                    Some(
                        GenEvent::Failed(err) | GenEvent::FailedWithTelemetry { error: err, .. },
                    ) => {
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
    output_limit: Option<&OutputTokenLimit>,
) -> String {
    chunk_json_final(
        id,
        created,
        model,
        mode,
        delta,
        finish,
        summary,
        output_limit,
        false,
    )
}

/// [`chunk_json`] with the one datum only the final frame knows: whether the ceiling
/// cut a tool call in half.
#[allow(clippy::too_many_arguments)]
fn chunk_json_final(
    id: &str,
    created: i64,
    model: &str,
    mode: Mode,
    delta: serde_json::Value,
    finish: Option<&str>,
    summary: Option<&crate::gateway::GenSummary>,
    output_limit: Option<&OutputTokenLimit>,
    truncated_tool_call: bool,
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
        let prompt_tokens = summary
            .trace
            .as_ref()
            .map(|trace| trace.input_tokens)
            .unwrap_or(0);
        payload["usage"] = json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": summary.tokens_generated,
            "total_tokens": prompt_tokens + summary.tokens_generated,
            "prompt_tokens_details": {
                "cached_tokens": summary
                    .trace
                    .as_ref()
                    .map(|trace| trace.cached_tokens)
                    .unwrap_or(0),
            },
            "completion_tokens_details": {
                "reasoning_tokens": summary.reasoning_tokens,
            },
        });
        payload["x_pom"] = json!({
            "source": summary.source,
            "tokens_per_sec": summary.tokens_per_sec,
            "tokens_generated": summary.tokens_generated,
            "reasoning_tokens": summary.reasoning_tokens,
            "duration_ms": summary.duration_ms,
            "timings": summary.timings,
            // See the same field on the non-streaming path: a call
            // cut in half is also a partial response.
            "partial": summary.partial || truncated_tool_call,
            "error": summary.error,
            "finish_reason": summary.finish_reason,
            "stop_sequence": summary.stop_sequence,
            "trace": summary.trace,
            "output_limits": output_limit,
            "continuation_legs": summary.legs, "payment": summary.payment,
            "truncated_tool_call": truncated_tool_call,
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

/// The thinking a chat client asked for: the POM's own `thinking_enabled`, else the
/// `chat_template_kwargs.enable_thinking` OpenAI-compatible clients send, else `reasoning_effort: "none"`
/// as off; on when none is given. Reading only the first left `enable_thinking: false` thinking anyway,
/// while a plugin engine was told the client had chosen (`thinking_asked`).
fn client_thinking(explicit: Option<bool>, details: &serde_json::Value) -> bool {
    if let Some(value) = explicit {
        return value;
    }
    if let Some(value) = details["chat_template_kwargs"]["enable_thinking"].as_bool() {
        return value;
    }
    details["reasoning_effort"].as_str() != Some("none")
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_client_turns_thinking_off_through_its_template_kwargs() {
        use serde_json::json;
        let off = json!({"chat_template_kwargs": {"enable_thinking": false}});
        assert!(!super::client_thinking(None, &off));
        assert!(super::client_thinking(Some(true), &off));
        assert!(!super::client_thinking(None, &json!({"reasoning_effort": "none"})));
        assert!(super::client_thinking(None, &json!({"reasoning_effort": "low"})));
        assert!(super::client_thinking(None, &json!({})));
    }


    #[test]
    fn reasoning_uses_the_structured_chat_delta_instead_of_visible_content() {
        let delta = reasoning_delta(Mode::Chat, "checking".into());
        assert_eq!(delta["reasoning_content"], "checking");
        assert!(delta.get("content").is_none());
    }

    #[test]
    fn retrieval_searches_for_what_the_person_last_wrote() {
        let messages = |value: serde_json::Value| -> Vec<ChatMessage> {
            serde_json::from_value(value).unwrap()
        };
        let asked = messages(json!([
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "answer"},
            {"role": "user", "content": [{"type": "text", "text": "how long is the warranty?"}]},
        ]));
        assert_eq!(
            last_user_question(&asked).as_deref(),
            Some("how long is the warranty?")
        );
        // An agent in the middle of its loop is not asking a question.
        let tool_result = messages(json!([
            {"role": "user", "content": "run it"},
            {"role": "assistant", "content": null, "tool_calls": [{"function": {"name": "run", "arguments": "{}"}}]},
            {"role": "tool", "content": "done"},
        ]));
        assert_eq!(last_user_question(&tool_result), None);
        let assistant_last = messages(json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
        ]));
        assert_eq!(last_user_question(&assistant_last), None);
        assert_eq!(last_user_question(&[]), None);
    }

    #[test]
    fn retrieved_context_never_enters_the_reusable_prefix() {
        let messages = |value: serde_json::Value| -> Vec<ChatMessage> {
            serde_json::from_value(value).unwrap()
        };
        let turn_one = messages(json!([{"role": "user", "content": "how long is the warranty?"}]));
        let turn_two = messages(json!([
            {"role": "user", "content": "how long is the warranty?"},
            {"role": "assistant", "content": "Two years."},
            {"role": "user", "content": "and what does it cover?"},
        ]));
        let conversation = render_prompt(&turn_one, &[]);
        let context = crate::rag::context_block(&[crate::rag::Passage {
            path: "manual.pdf".into(),
            text: "The warranty lasts twenty four months.".into(),
            score: 0.9,
        }]);
        let sent = format!("{conversation}{context}");
        let boundary = retain_boundary(Some(conversation.len())).unwrap();
        // The head keeps the conversation and nothing of the context...
        assert_eq!(boundary.retain_prefix_bytes, Some(conversation.len()));
        assert!(boundary.labels.is_empty());
        assert_eq!(&sent[..conversation.len()], conversation);
        // ...and the next turn's prompt, with or without its own context,
        // starts with exactly what was kept, as the prefix cache key does.
        let next = render_prompt(&turn_two, &[]);
        assert!(next.starts_with(&conversation));
        assert!(conversation_cache_key(&turn_two, &[])
            .starts_with(&conversation_cache_key(&turn_one, &[])));
        assert!(!next.contains("Retrieved context"));
        assert_eq!(retain_boundary(None), None);
    }

    use super::*;

    /// A typed request as the body a client sends (the handler takes the body as it came: see `chat_completions`).
    fn body(req: ChatRequest) -> Json<serde_json::Value> {
        Json(serde_json::to_value(req).expect("a chat request serializes"))
    }

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: Content::Text(text.into()),
            tool_calls: None,
        }
    }

    #[test]
    fn image_is_rejected_when_the_served_model_has_no_mmproj() {
        assert_eq!(
            image_vision_error(&[vec![0xFF, 0xD8]], None),
            Some("this model has no vision")
        );
        assert_eq!(
            image_vision_error(&[vec![0xFF, 0xD8]], Some("mmproj.gguf")),
            None
        );
        assert_eq!(image_vision_error(&[], None), None);
    }

    // ── Deployment choice ────────────────────────────────────────────────────

    /// Each deployment appears in the list with its own suffix, so the client
    /// has a way to ask for a specific configuration.
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

    /// Replicas of the whole model: one that serves answers for the name even while another is still loading. The
    /// loading one must not hide the model from the list (it did, because every peer had to be ready).
    #[tokio::test]
    async fn a_loading_replica_does_not_hide_a_model_another_replica_serves() {
        let mut serving = crate::gateway::testkit::peer("n1", "n1", 8080);
        serving.model = "qwen3:8b".into();
        serving.start_layer = 0;
        serving.end_layer = 47;
        serving.total_layers = 48;
        let mut loading = crate::gateway::testkit::peer("n2", "n2", 8080);
        loading.model = "qwen3:8b".into();
        loading.start_layer = 0;
        loading.end_layer = 47;
        loading.total_layers = 48;
        loading.runtime_capacity = None;
        loading.idle = false;

        let state = crate::gateway::testkit::state_with("eu", vec![serving, loading]).await;
        assert!(model_ids(&state).await.contains(&"qwen3:8b".to_string()));
    }

    /// A pipeline still waits for every stage: a loading stage keeps the model out of the list.
    #[tokio::test]
    async fn a_loading_pipeline_stage_still_hides_the_model() {
        let mut head = crate::gateway::testkit::peer("n1", "n1", 8080);
        head.model = "qwen3:8b".into();
        head.start_layer = 0;
        head.end_layer = 23;
        head.total_layers = 48;
        let mut tail = crate::gateway::testkit::peer("n2", "n2", 8080);
        tail.model = "qwen3:8b".into();
        tail.start_layer = 24;
        tail.end_layer = 47;
        tail.total_layers = 48;
        tail.runtime_capacity = None;
        tail.idle = false;

        let state = crate::gateway::testkit::state_with("eu", vec![head, tail]).await;
        assert!(!model_ids(&state).await.contains(&"qwen3:8b".to_string()));
    }

    /// A whole-model peer that is loading answers for nothing: with no ready replica the model stays hidden.
    #[tokio::test]
    async fn a_loading_whole_model_peer_alone_is_not_listed() {
        let mut loading = crate::gateway::testkit::peer("n1", "n1", 8080);
        loading.model = "qwen3:8b".into();
        loading.start_layer = 0;
        loading.end_layer = 47;
        loading.total_layers = 48;
        loading.runtime_capacity = None;
        loading.idle = false;

        let state = crate::gateway::testkit::state_with("eu", vec![loading]).await;
        assert!(!model_ids(&state).await.contains(&"qwen3:8b".to_string()));
    }

    /// A CLI default is only a selector for the inference path. After an
    /// unload, it is not a published model and cannot return to the catalog
    /// to mislead a client that auto-detects models.
    #[tokio::test]
    async fn model_discovery_ignores_an_unpublished_default_model() {
        let mut state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.default_model = "stale-after-unload".into();

        assert!(model_ids(&state).await.is_empty());
    }

    /// An announcement that lost its effective capacity represents a peer
    /// restarting, not a ready deployment. It cannot keep the model in the
    /// auto-detected catalog while the old layer is still alive in the
    /// gossip table.
    #[tokio::test]
    async fn model_discovery_ignores_a_peer_during_runtime_restart() {
        let mut peer = crate::gateway::testkit::peer("n1", "n1", 8080);
        peer.model = "stale-after-restart".into();
        peer.runtime_capacity = None;
        let state = crate::gateway::testkit::state_with("eu", vec![peer]).await;

        assert!(model_ids(&state).await.is_empty());
    }

    /// An idle deployment remains a valid selector even though unloading the
    /// weights clears its runtime capacity. The request must resolve the
    /// deployment before the generation path can wake its peers.
    #[tokio::test]
    async fn idle_deployment_remains_resolvable_by_model_name() {
        let mut head = crate::gateway::testkit::peer("head", "head", 8080);
        head.model = "ornith-35b".into();
        head.deploy_id = "d-idle1234".into();
        head.inference_port = Some(8181);
        head.start_layer = 0;
        head.end_layer = 15;
        head.total_layers = 32;
        head.runtime_capacity = None;
        head.idle = true;

        let mut tail = crate::gateway::testkit::peer("tail", "tail", 8081);
        tail.model = "ornith-35b".into();
        tail.deploy_id = "d-idle1234".into();
        tail.inference_port = Some(8182);
        tail.start_layer = 16;
        tail.end_layer = 31;
        tail.total_layers = 32;
        tail.runtime_capacity = None;
        tail.idle = true;

        let state = crate::gateway::testkit::state_with("gateway", vec![head, tail]).await;

        let ids = model_ids(&state).await;
        assert!(ids.contains(&"ornith-35b".to_string()), "{ids:?}");
        assert!(
            ids.contains(&"ornith-35b@d-idle1234".to_string()),
            "{ids:?}"
        );
        assert_eq!(
            resolve_model(&state, "ornith-35b".into()).await,
            Ok("ornith-35b".to_string())
        );

        let limit = state
            .request_output_limit_for_model(Some("ornith-35b"), 32, "oi")
            .await;
        assert!(
            limit.rejection_reason().is_none(),
            "a gateway must let an idle deployment reach reload_model: {limit:?}"
        );
    }

    /// Asking for a deployment that nobody announces **cannot** silently fall back to the
    /// active one: whoever wrote the suffix asked for that configuration, and answering
    /// with another is worse than failing.
    #[tokio::test]
    async fn an_explicit_deployment_is_never_silently_swapped() {
        let mut p1 = crate::gateway::testkit::peer("n1", "n1", 8080);
        p1.model = "qwen3:8b".into();
        p1.deploy_id = "d-aaaaaaaa".into();
        let state = crate::gateway::testkit::state_with("eu", vec![p1]).await;

        assert_eq!(
            resolve_model(&state, "qwen3:8b@d-cccccccc".into()).await,
            Ok("qwen3:8b@d-cccccccc".to_string())
        );
    }

    /// A bare name that no peer announces is refused, not silently swapped
    /// for the active model - captain's decision: a client that
    /// asked for a nonexistent model needs a clear error, never a
    /// 200 response served by another model.
    #[tokio::test]
    async fn an_unknown_plain_model_name_is_refused_not_silently_swapped() {
        let mut p1 = crate::gateway::testkit::peer("n1", "n1", 8080);
        p1.model = "qwen3:8b".into();
        p1.deploy_id = "d-aaaaaaaa".into();
        let state = crate::gateway::testkit::state_with("eu", vec![p1]).await;

        let error = resolve_model(&state, "modelo-que-nao-existe".into())
            .await
            .expect_err("unknown model must be refused");
        assert!(error.contains("modelo-que-nao-existe"), "{error}");
    }

    // ── render_prompt ────────────────────────────────────────────────────────

    #[test]
    fn chat_requests_enable_template_but_completions_stay_raw() {
        assert!(Mode::Chat.applies_chat_template());
        assert!(!Mode::Completion.applies_chat_template());
    }

    #[tokio::test]
    async fn chat_endpoint_structures_the_audited_tool_call_and_keeps_intro() {
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
                                "text": "I'll build a complete Tetris game. Let me start by loading the brainstorming skill to understand your full requirements.\n\n<tool_call>\n<function=skill>\n<parameter=name>\nbrainstorming\n</parameter>\n</function>\n</tool_call>"
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
            ApiKeyAuth::for_test(crate::reqlog::Caller::anonymous()),
            State(state),
            body(ChatRequest {
                session: None,
                // Retrieval is not what this test is about.
                rag: Some(false),
                rag_collection: None,
                model: Some("lab".into()),
                messages: vec![msg("user", "Oi, teste")],
                stream: false,
                max_tokens: Some(128),
                max_completion_tokens: None,
                tools: vec![ChatToolDef {
                    function: ChatFunctionDef {
                        name: "skill".into(),
                        description: Some("Load a skill".into()),
                        parameters: json!({
                            "type": "object",
                            "properties": {"name": {"type": "string"}}
                        }),
                    },
                }],
                stop: None,
                inference_head: None,
                thinking_enabled: Some(false),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(
            body["choices"][0]["message"]["content"],
            "I'll build a complete Tetris game. Let me start by loading the brainstorming skill to understand your full requirements.\n\n"
        );
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["type"],
            "function"
        );
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "skill"
        );
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            r#"{"name":"brainstorming"}"#
        );
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");

        let meta = seen
            .lock()
            .unwrap()
            .clone()
            .expect("the chat request reached the inference head");
        assert!(meta["prompt"]
            .as_str()
            .unwrap()
            .ends_with("User: Oi, teste"));
        assert_eq!(meta["chat_template"], true);
        assert_eq!(meta["thinking_enabled"], false);
    }

    /// What the client reads as savings is what the pipeline **executed**. The
    /// head reports `cached_tokens` in chunk 0 and the number crosses
    /// `PrefillOutcome` → `RequestTrace` → `usage`. Before this the client had
    /// no way to know the cache hit was not saving anything.
    #[tokio::test]
    async fn usage_reports_the_prefix_tokens_the_pipeline_actually_reused() {
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
                axum::routing::post(|_body: axum::body::Bytes| async move {
                    let response = json!({
                        "done": true,
                        "more_chunks": false,
                        "n_prompt_tokens": 40,
                        "reuse_prefix": true,
                        "cached_tokens": 33,
                        "start_pos": 0,
                        "next_pos": 40,
                        "stable_len": 40,
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
                }),
            )
            .route(
                "/v1/session/rewind",
                axum::routing::post(|| async {
                    axum::Json(json!({"rewound": true, "n_past": 40}))
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
            ApiKeyAuth::for_test(crate::reqlog::Caller::anonymous()),
            State(state),
            body(ChatRequest {
                session: None,
                // Retrieval is not what this test is about.
                rag: Some(false),
                rag_collection: None,
                model: Some("lab".into()),
                messages: vec![msg("user", "Oi, teste")],
                stream: false,
                max_tokens: Some(1),
                max_completion_tokens: None,
                tools: Vec::new(),
                stop: None,
                inference_head: None,
                thinking_enabled: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["usage"]["prompt_tokens_details"]["cached_tokens"], 33);
        assert_eq!(body["x_pom"]["trace"]["cached_tokens"], 33);
        assert_eq!(body["x_pom"]["trace"]["input_tokens"], 40);
    }

    /// The first chunk's response can end at token 42 even when the
    /// wire uses chunks of 512. The gateway cannot turn the absence of
    /// `next_pos` into `chunk_index * 512`: before the fix this dispatched
    /// chunk 1 at position 512, the peer answered 400 and the SSE exposed
    /// `server_error` before concluding the cold fallback.
    #[tokio::test]
    async fn streaming_chat_falls_back_without_sending_a_chunk_index_position() {
        let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
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
                    let frames = frames.clone();
                    move |body: axum::body::Bytes| {
                        let frames = frames.clone();
                        async move {
                            let (meta, _) = crate::wire::decode_inference_request(&body).unwrap();
                            frames.lock().unwrap().push(meta.clone());
                            let chunk_index = meta["chunk_index"].as_u64().unwrap_or(0);
                            let reuse = meta["reuse_prefix"].as_bool().unwrap_or(false);

                            // Reproduce the captain's peer: the cached first
                            // chunk confirms session position 42 but omits
                            // next_pos. The old gateway invented 512 and
                            // reached this explicit rejection on chunk 1.
                            if reuse && chunk_index == 0 {
                                let response = json!({
                                    "done": false,
                                    "more_chunks": true,
                                    "n_prompt_tokens": 600,
                                    "reuse_prefix": true,
                                    "cached_tokens": 42,
                                    "start_pos": 42,
                                });
                                let bytes = crate::wire::encode_batch_single(
                                    &response,
                                    None,
                                    crate::wire::WireDtype::F16,
                                )
                                .unwrap();
                                return (
                                    StatusCode::OK,
                                    [("content-type", "application/octet-stream")],
                                    bytes,
                                )
                                    .into_response();
                            }
                            if chunk_index == 1 && meta["start_pos"] == 512 {
                                return (
                                    StatusCode::BAD_REQUEST,
                                    "chunk out of order: received 1 (pos 512) but the session is at 42",
                                )
                                    .into_response();
                            }

                            let response = json!({
                                "done": true,
                                "more_chunks": false,
                                "n_prompt_tokens": 42,
                                "reuse_prefix": false,
                                "cached_tokens": 0,
                                "start_pos": 0,
                                "next_pos": 42,
                                "stable_len": 42,
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
                "/v1/session/rewind",
                axum::routing::post(|axum::Json(body): axum::Json<serde_json::Value>| async move {
                    axum::Json(json!({
                        "rewound": true,
                        "n_past": body["n_tokens"].clone(),
                    }))
                }),
            )
            .route(
                "/v1/session/release",
                axum::routing::post(|| async { StatusCode::OK }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut peer = crate::gateway::testkit::peer("whole", "whole", 8080);
        peer.model = "whole-model".into();
        peer.start_layer = 0;
        peer.end_layer = 0;
        peer.total_layers = 1;
        peer.inference_port = Some(port);
        let state = crate::gateway::testkit::state_with("eu", vec![peer]).await;
        state
            .runtime
            .seed_config_for_test(crate::gateway::testkit::spec_config())
            .await;

        let stream_body = |response: Response| async move {
            String::from_utf8(
                axum::body::to_bytes(response.into_body(), 1 << 20)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap()
        };
        let first = chat_completions(
            ApiKeyAuth::for_test(crate::reqlog::Caller::anonymous()),
            State(state.clone()),
            body(ChatRequest {
                session: None,
                // Retrieval is not what this test is about.
                rag: Some(false),
                rag_collection: None,
                model: Some("whole-model".into()),
                messages: vec![msg("user", "first")],
                stream: true,
                max_tokens: Some(1),
                max_completion_tokens: None,
                tools: Vec::new(),
                stop: None,
                inference_head: None,
                thinking_enabled: None,
            }),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = stream_body(first).await;
        assert!(first_body.contains("[DONE]"));
        assert!(!first_body.contains("server_error"));

        let second = chat_completions(
            ApiKeyAuth::for_test(crate::reqlog::Caller::anonymous()),
            State(state),
            body(ChatRequest {
                session: None,
                // Retrieval is not what this test is about.
                rag: Some(false),
                rag_collection: None,
                model: Some("whole-model".into()),
                messages: vec![
                    msg("user", "first"),
                    msg("assistant", "ok"),
                    msg("user", "segundo"),
                ],
                stream: true,
                max_tokens: Some(1),
                max_completion_tokens: None,
                tools: Vec::new(),
                stop: None,
                inference_head: None,
                thinking_enabled: None,
            }),
        )
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        let second_body = stream_body(second).await;
        assert!(second_body.contains("\"content\":\"\""));
        assert!(second_body.contains("[DONE]"));
        assert!(!second_body.contains("server_error"), "{second_body}");

        let frames = frames.lock().unwrap();
        assert!(frames
            .iter()
            .any(|meta| meta["chunk_index"] == 0 && meta["reuse_prefix"] == true));
        assert!(frames
            .iter()
            .any(|meta| { meta["chunk_index"] == 0 && meta["reuse_prefix"] == false }));
        assert!(
            !frames
                .iter()
                .any(|meta| { meta["chunk_index"] == 1 && meta["start_pos"] == 512 }),
            "position derived from the index sent to the peer: {frames:?}"
        );
    }

    async fn streaming_chat_status(prompt_repeats: usize, max_tokens: u32) -> StatusCode {
        let state = crate::gateway::testkit::state_with("eu", Vec::new()).await;
        state
            .runtime
            .seed_config_for_test(crate::gateway::testkit::spec_config())
            .await;

        chat_completions(
            ApiKeyAuth::for_test(crate::reqlog::Caller::anonymous()),
            State(state),
            body(ChatRequest {
                session: None,
                // Retrieval is not what this test is about.
                rag: Some(false),
                rag_collection: None,
                // With no peer announcing "lab" in this fixture, an explicit
                // `model` would be refused by `resolve_model` - the target of this
                // test is the budget/context math, not name resolution,
                // so omitting it uses `active_model()` directly.
                model: None,
                messages: vec![
                    msg("system", &"agent instructions ".repeat(prompt_repeats)),
                    msg("user", "hello"),
                ],
                stream: true,
                max_tokens: Some(max_tokens),
                max_completion_tokens: None,
                tools: Vec::new(),
                stop: None,
                inference_head: None,
                thinking_enabled: None,
            }),
        )
        .await
        .status()
    }

    /// The absence of an artificial absolute ceiling does not turn the request into an
    /// error. The effective context is what reduces the budget, if necessary.
    #[tokio::test]
    async fn a_request_above_the_old_absolute_default_is_not_rejected() {
        assert_eq!(streaming_chat_status(1, 32_000).await, StatusCode::OK);
    }

    /// And the refusal for a full context is still 413, with the request within
    /// capacity: they are two different reasons, with different codes.
    #[tokio::test]
    async fn a_prompt_that_fills_the_context_is_still_rejected_before_sse_starts() {
        assert_eq!(
            streaming_chat_status(2_000, 4_000).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn single_user_message_is_labelled_like_every_other_turn() {
        // The label holds from the first turn: it is what makes one turn's prompt
        // an exact prefix of the next one's prompt. See `render_prompt`.
        assert_eq!(
            render_prompt(&[msg("user", "O que e inferencia distribuida?")], &[]),
            "User: O que e inferencia distribuida?"
        );
    }

    /// The property prefix reuse depends on, asserted directly:
    /// appending messages only appends text at the end.
    #[test]
    fn every_extra_turn_only_appends_to_the_previous_prompt() {
        let turn1 = vec![msg("user", "1+1?")];
        let turn2 = vec![
            msg("user", "1+1?"),
            msg("assistant", "2"),
            msg("user", "e 2+2?"),
        ];
        let turn3 = {
            let mut m = turn2.clone();
            m.push(msg("assistant", "4"));
            m.push(msg("user", "e 3+3?"));
            m
        };

        let p1 = render_prompt(&turn1, &[]);
        let p2 = render_prompt(&turn2, &[]);
        let p3 = render_prompt(&turn3, &[]);

        assert!(
            p2.starts_with(&p1),
            "turn 2 does not continue turn 1:\n{p1}\n---\n{p2}"
        );
        assert!(p3.starts_with(&p2), "turn 3 does not continue turn 2");
        // And the cache key follows, which is what `PrefixCache` consults.
        let k1 = conversation_cache_key(&turn1, &[]);
        let k2 = conversation_cache_key(&turn2, &[]);
        let k3 = conversation_cache_key(&turn3, &[]);
        assert!(k2.starts_with(&k1));
        assert!(k3.starts_with(&k2));
    }

    #[test]
    fn late_developer_message_invalidates_the_rendered_prefix() {
        let first = vec![
            msg("user", "edite este arquivo"),
            msg("assistant", "vou editar"),
        ];
        let mut next = first.clone();
        next.push(msg("developer", "use o formatter do projeto"));
        next.push(msg("user", "agora aplique a correção"));

        let first_prompt = render_prompt(&first, &[]);
        let next_prompt = render_prompt(&next, &[]);
        let first_key = conversation_cache_key(&first, &[]);
        let next_key = conversation_cache_key(&next, &[]);

        assert_eq!(first_key, first_prompt);
        assert_eq!(next_key, next_prompt);
        assert!(
            !next_key.starts_with(&first_key),
            "the cache cannot declare a prefix when the rendering inserted context at the start"
        );
    }

    /// Changing the tool list moves the start of the prompt, and the prefix
    /// **has** to be discarded - the key must reflect that.
    #[test]
    fn changing_the_tool_list_breaks_the_cache_key_on_purpose() {
        let messages = vec![msg("user", "oi")];
        let tool = ToolDef {
            eager_input_streaming: None,
            name: "buscar".into(),
            description: None,
            input_schema: json!({"type": "object"}),
        };
        let without_tool = conversation_cache_key(&messages, &[]);
        let with_tool = conversation_cache_key(&messages, std::slice::from_ref(&tool));
        assert!(
            !with_tool.starts_with(&without_tool),
            "the key with a tool cannot continue the one without"
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
        assert_eq!(out, "User: oi");
    }

    #[test]
    fn content_parts_are_concatenated() {
        let m = ChatMessage {
            role: "user".into(),
            content: Content::Parts(vec![
                ContentPart {
                    kind: Some("text".into()),
                    text: Some("linha 1".into()),
                    image_url: None,
                },
                ContentPart {
                    kind: Some("text".into()),
                    text: Some("linha 2".into()),
                    image_url: None,
                },
            ]),
            tool_calls: None,
        };
        assert_eq!(render_prompt(&[m], &[]), "User: linha 1\nlinha 2");
    }

    #[test]
    fn parses_array_content_from_real_clients() {
        let raw = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"oi"}]}]}"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(render_prompt(&req.messages, &[]), "User: oi");
        assert!(!req.stream);
    }

    #[test]
    fn parses_null_content_on_assistant_messages() {
        let raw =
            r#"{"messages":[{"role":"assistant","content":null},{"role":"user","content":"oi"}]}"#;
        let req: ChatRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(render_prompt(&req.messages, &[]), "User: oi");
    }

    /// Mirrors `chat_completions`'s own unwrap-the-wire-shape step.
    fn tool_defs(req: &ChatRequest) -> Vec<ToolDef> {
        req.tools
            .iter()
            .map(|t| ToolDef {
                eager_input_streaming: None,
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
    fn max_tokens_is_defaulted_without_an_artificial_upper_bound() {
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
            999999
        );
    }

    /// `context_length` and `max_completion_tokens` come from the deployment's
    /// effective context. Announcing `max_new_tokens` (here, 1000) would make a client
    /// confuse the size of one leg with the total budget.
    #[tokio::test]
    async fn model_metadata_advertises_the_request_ceiling_not_the_leg() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let mut cfg = crate::gateway::testkit::spec_config();
        cfg.n_ctx = 100_000;
        cfg.max_new_tokens = 1_000;
        state.runtime.seed_config_for_test(cfg).await;
        let (context_length, max_completion_tokens) =
            state.advertised_output_limits(Some("ornith")).await;
        let value = serde_json::to_value(model_object(
            "ornith".into(),
            0,
            context_length,
            max_completion_tokens,
            false,
        ))
        .unwrap();

        assert_eq!(value["context_length"], 100_000);
        assert_eq!(value["top_provider"]["context_length"], 100_000);
        assert_eq!(value["top_provider"]["max_completion_tokens"], 100_000);
    }

    #[tokio::test]
    async fn model_metadata_matches_openai_and_anthropic_list_contracts() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let mut cfg = crate::gateway::testkit::spec_config();
        cfg.n_ctx = 100_000;
        state.runtime.seed_config_for_test(cfg).await;
        let (context_length, max_completion_tokens) = state
            .advertised_output_limits(Some("Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf"))
            .await;

        let value = serde_json::to_value(model_object(
            "Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf".into(),
            0,
            context_length,
            max_completion_tokens,
            true,
        ))
        .unwrap();

        assert_eq!(value["object"], "model");
        assert_eq!(value["type"], "model");
        assert_eq!(value["display_name"], "Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf");
        assert_eq!(value["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(value["max_input_tokens"], 100_000);
        assert_eq!(value["max_tokens"], 100_000);
        assert_eq!(value["capabilities"]["vision"], true);
        assert_eq!(value["context_length"], 100_000);
        assert_eq!(value["top_provider"]["max_completion_tokens"], 100_000);
    }

    #[test]
    fn finish_reason_reflects_the_limit_and_tool_calls() {
        let summary = |n: u32| crate::gateway::GenSummary {
            payment: None,
            source: "pipeline:a→b".into(),
            tokens_generated: n,
            reasoning_tokens: 0,
            tokens_per_sec: 1.0,
            duration_ms: 10,
            timings: crate::gateway::timing_metrics(10, 10, 10, 0, n),
            partial: false,
            error: None,
            finish_reason: None,
            stop_sequence: None,
            legs: 1,
            trace: None,
        };
        // Complete tool call within the budget: the model ended its turn by
        // asking for the tool, so the protocol must preserve tool_calls.
        assert_eq!(finish_reason(&summary(3640), 4000, true), "tool_calls");
        // Reaching the ceiling is a terminal stop and wins over a call in the output: the
        // call the ceiling caught open is incomplete, and it is `length` that
        // makes the client repeat the request instead of executing it.
        assert_eq!(finish_reason(&summary(4000), 4000, true), "length");
        // Plain text cut by the budget is length as well.
        assert_eq!(finish_reason(&summary(4000), 4000, false), "length");
        // Plain text that stopped before the budget is a normal stop.
        let mut stopped = summary(3640);
        stopped.finish_reason = Some("stop".into());
        assert_eq!(finish_reason(&stopped, 4000, false), "stop");
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
    fn audited_tool_call_becomes_structured_openai_message_without_losing_intro() {
        let raw = "I'll build a complete Tetris game. Let me start by loading the brainstorming skill to understand your full requirements.\n\n<tool_call>\n<function=skill>\n<parameter=name>\nbrainstorming\n</parameter>\n</function>\n</tool_call>";
        let (message, saw_tool_call) = chat_message_json(&crate::tool_call::parse_output(raw));

        assert!(saw_tool_call);
        assert_eq!(
            message["content"],
            "I'll build a complete Tetris game. Let me start by loading the brainstorming skill to understand your full requirements.\n\n"
        );
        assert_eq!(message["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(message["tool_calls"][0]["type"], "function");
        assert_eq!(message["tool_calls"][0]["function"]["name"], "skill");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            r#"{"name":"brainstorming"}"#
        );
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
            &mut None,
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
            &mut None,
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
            &mut None,
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
            &mut None,
            &mut out,
        );
        assert!(!saw_tool_call);
        assert_eq!(
            out,
            vec![json!({ "content": "<tool_call>bad</tool_call>" })]
        );
    }

    #[test]
    fn incomplete_tool_call_stays_text_and_does_not_mark_a_call_in_openai_stream() {
        let mut scanner = TagScanner::new();
        let mut tool_call_index = 0u32;
        let mut saw_tool_call = false;
        let mut out = Vec::new();

        apply_scan_events_openai(
            scanner.feed("<tool_call><function=write><parameter=content>\n<html>"),
            &mut tool_call_index,
            &mut saw_tool_call,
            &mut None,
            &mut out,
        );
        apply_scan_events_openai(
            scanner.finish(),
            &mut tool_call_index,
            &mut saw_tool_call,
            &mut None,
            &mut out,
        );

        assert!(!saw_tool_call);
        assert_eq!(out.len(), 1);
        assert!(out[0]["content"].as_str().unwrap().contains("<tool_call>"));
    }

    #[test]
    fn audited_tool_call_stream_emits_intro_and_structured_deltas_after_split_tags() {
        let raw = "I'll build a complete Tetris game. Let me start by loading the brainstorming skill to understand your full requirements.\n\n<tool_call>\n<function=skill>\n<parameter=name>\nbrainstorming\n</parameter>\n</function>\n</tool_call>";
        let mut scanner = TagScanner::new();
        let mut tool_call_index = 0u32;
        let mut saw_tool_call = false;
        let mut out = Vec::new();

        for chunk in raw.as_bytes().chunks(7) {
            let chunk = std::str::from_utf8(chunk).expect("fixture is UTF-8");
            apply_scan_events_openai(
                scanner.feed(chunk),
                &mut tool_call_index,
                &mut saw_tool_call,
                &mut None,
                &mut out,
            );
        }
        apply_scan_events_openai(
            scanner.finish(),
            &mut tool_call_index,
            &mut saw_tool_call,
            &mut None,
            &mut out,
        );

        assert!(saw_tool_call);
        let content = out
            .iter()
            .filter_map(|delta| delta["content"].as_str())
            .collect::<String>();
        assert_eq!(
            content,
            "I'll build a complete Tetris game. Let me start by loading the brainstorming skill to understand your full requirements.\n\n"
        );
        let tool_deltas = out
            .iter()
            .filter(|delta| delta.get("tool_calls").is_some())
            .collect::<Vec<_>>();
        assert_eq!(tool_deltas.len(), 2);
        assert_eq!(tool_deltas[0]["tool_calls"][0]["index"], 0);
        assert_eq!(tool_deltas[0]["tool_calls"][0]["function"]["name"], "skill");
        assert_eq!(
            tool_deltas[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"name":"brainstorming"}"#
        );
    }

    #[tokio::test]
    async fn a_partial_stream_ends_in_an_error_after_the_salvaged_text() {
        let mut summary = crate::gateway::GenSummary {
            payment: None,
            source: "pipeline:a (partial)".into(),
            tokens_generated: 1,
            reasoning_tokens: 0,
            tokens_per_sec: 1.0,
            duration_ms: 10,
            timings: crate::gateway::timing_metrics(10, 10, 10, 1, 1),
            partial: true,
            error: None,
            finish_reason: None,
            stop_sequence: None,
            legs: 1,
            trace: None,
        };
        summary.error = Some("pipeline failed at step 1: deadline expired".into());
        let (tx, rx) = mpsc::channel(4);
        tx.send(GenEvent::Token("Oi".into())).await.unwrap();
        tx.send(GenEvent::Done(summary)).await.unwrap();
        drop(tx);
        let stream = sse_stream(
            rx,
            "chatcmpl-test".into(),
            1,
            "flint".into(),
            Mode::Chat,
            100,
            OutputTokenLimit::default(),
            ToolSignatures::default(),
        );
        let body = axum::body::to_bytes(Sse::new(stream).into_response().into_body(), 1 << 20)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        // The text that went out stays, and the cut is an error, not the model's end.
        let text_at = body.find("Oi").expect("salvaged text");
        let error_at = body.find("deadline expired").expect("error event");
        let done_at = body.find("[DONE]").expect("end of stream");
        assert!(text_at < error_at && error_at < done_at, "{body}");
    }

    #[test]
    fn final_stream_chunk_carries_generation_summary() {
        let summary = crate::gateway::GenSummary {
            payment: None,
            source: "pipeline:a→b".into(),
            tokens_generated: 12,
            reasoning_tokens: 5,
            tokens_per_sec: 4.5,
            duration_ms: 2666,
            timings: crate::gateway::timing_metrics(2666, 900, 900, 12, 12),
            partial: true,
            error: None,
            finish_reason: Some("length".into()),
            stop_sequence: None,
            legs: 1,
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
            None,
        ))
        .expect("valid JSON");

        assert_eq!(payload["x_pom"]["tokens_per_sec"], 4.5);
        assert_eq!(payload["x_pom"]["tokens_generated"], 12);
        assert_eq!(payload["x_pom"]["reasoning_tokens"], 5);
        assert_eq!(payload["usage"]["completion_tokens"], 12);
        assert_eq!(
            payload["usage"]["completion_tokens_details"]["reasoning_tokens"],
            5
        );
        assert_eq!(payload["x_pom"]["timings"]["ttft_ms"], 900);
        assert!(payload["x_pom"]["timings"]["generation_tokens_per_sec"].is_number());
        assert_eq!(payload["x_pom"]["trace"]["model"], "qwen35");
        assert_eq!(payload["choices"][0]["finish_reason"], "stop");
        assert_eq!(payload["x_pom"]["finish_reason"], "length");
        assert_eq!(payload["x_pom"]["partial"], true);
    }
}
