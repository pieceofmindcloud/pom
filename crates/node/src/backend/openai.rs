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
    parse_output, parse_output_truncated, render_tool_call, render_tools_block, OutputPart,
    ScanEvent, TagScanner, ToolDef,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{sse::Event, IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use base64::Engine as _;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::VecDeque;
use std::convert::Infallible;
use tokio::sync::mpsc;
use tracing::info;

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
                    return Err("imagem deve ser um data URL base64".to_string());
                };
                if !prefix.starts_with("data:image/") || !prefix.contains(";base64") {
                    return Err("imagem deve ser um data URL base64".to_string());
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|error| format!("imagem base64 inválida: {error}"))?;
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
    /// Escolha por inferência do Chat. Ausente mantém o padrão habilitado;
    /// o peer de inferência ainda força `false` quando a implantação desligou
    /// thinking globalmente.
    #[serde(default)]
    pub thinking_enabled: Option<bool>,
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
            // A OpenAI não tem interruptor para isto: ela **sempre** transmite
            // os argumentos em fragmentos, conforme gera. Não há campo a
            // espelhar, e inventar um seria estender o schema dela. Quem
            // decide nesta rota é `EAGER_STREAMING_OPENAI`.
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

/// Chave do cache de prefixo: o mesmo prompt canônico que será tokenizado pela
/// cabeça da pipeline.
///
/// A chave precisa ter exatamente a mesma ordenação que `render_prompt_with_images`.
/// Em especial, system/developer e ferramentas são movidos para o início do
/// prompt. Chavear pela ordem original das mensagens podia declarar um hit
/// quando um agente acrescentava um developer message no fim: a chave antiga
/// continuava prefixo, mas o prompt real tinha mudado desde o primeiro token.
/// O peer então recusava todo o prefixo e reprocessava o prompt inteiro.
pub fn conversation_cache_key(messages: &[ChatMessage], tools: &[ToolDef]) -> String {
    render_prompt(messages, tools)
}

/// Flatten a conversation (plus any declared tools) into the single prompt
/// string the pipeline takes.
///
/// The first peer already applies the model's chat template once, so
/// emitting a second one here would nest one inside the other. Instead the
/// turns are labelled in plain text and handed over as one user message.
///
/// **Todo turno é rotulado, inclusive o primeiro.** Uma mensagem de usuário
/// solitária já passou crua por aqui, e a partir da segunda entrava o rótulo:
/// `"q1"` virava `"User: q1\n\nAssistant: …"`, de modo que o prompt do turno 1
/// não era prefixo do prompt do turno 2 e o KV do turno anterior não servia
/// para nada. A transição 1→2, o primeiro momento em que um chat poderia
/// economizar, era a única que **nunca** podia acertar. Rotular sempre torna o
/// prompt de um turno prefixo exato do prompt do seguinte, que é a propriedade
/// sobre a qual o reuso inteiro se apoia (ver `crate::prefix_cache`).
///
/// Cliente que queira o texto cru, sem rótulo nenhum, tem `/v1/completions` e
/// `POST /api/generate`, que passam o prompt adiante intocado.
#[allow(dead_code)]
pub fn render_prompt(messages: &[ChatMessage], tools: &[ToolDef]) -> String {
    render_prompt_with_images(messages, tools)
        .map(|(prompt, _)| prompt)
        .unwrap_or_default()
}

/// Renderiza o prompt e extrai as imagens em bytes para o projector. O texto
/// usa o marcador oficial do mtmd no mesmo lugar de cada `image_url`.
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
        Some("este modelo não tem visão")
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
) -> ModelObject {
    ModelObject {
        display_name: id.clone(),
        id,
        object: "model",
        created,
        owned_by: "pom",
        model_type: "model",
        created_at: "1970-01-01T00:00:00Z",
        capabilities: None,
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
/// Os nomes que um cliente pode pedir.
///
/// Além do rótulo pelado de cada modelo, cada implantação aparece como
/// `<modelo>@<deploy_id>`. É o que dá ao cliente como escolher **qual**
/// implantação atende quando o mesmo modelo está no ar mais de uma vez; o nome
/// pelado continua valendo e vai para a que quem monta o pipeline escolher.
///
/// Um peer ocioso não tem `runtime_capacity` enquanto os pesos estão fora da
/// memória, mas continua sendo uma implantação servível: a geração o acorda
/// depois de planejar a rota. Só um peer que não está ocioso e ainda não
/// confirmou `/health` deve esconder o modelo do catálogo.
fn model_ids_from_peers(peers: &[crate::network::PeerInfo]) -> Vec<String> {
    let alive_models: std::collections::BTreeSet<String> = peers
        .iter()
        .filter(|p| p.is_alive() && !p.model.is_empty())
        .map(|p| p.model.clone())
        .collect();
    let mut ids: Vec<String> = alive_models
        .into_iter()
        .filter(|model| {
            peers
                .iter()
                .filter(|p| p.is_alive() && p.model == *model)
                .all(|p| p.runtime_capacity.is_some() || p.idle)
        })
        .collect();
    let deployments: std::collections::BTreeSet<(String, String)> = peers
        .iter()
        .filter(|p| p.is_alive() && !p.model.is_empty() && !p.deploy_id.is_empty())
        .map(|p| (p.model.clone(), p.deploy_id.clone()))
        .collect();
    for (model, deploy_id) in deployments {
        let ready = peers
            .iter()
            .filter(|p| p.is_alive() && p.model == model && p.deploy_id == deploy_id)
            .all(|p| p.runtime_capacity.is_some() || p.idle);
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
        data.push(model_object(
            id,
            created,
            context_length,
            max_completion_tokens,
        ));
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
        (
            cors_headers(),
            Json(model_object(
                id,
                chrono::Utc::now().timestamp(),
                context_length,
                max_completion_tokens,
            )),
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
    let request_details = serde_json::to_value(&req).unwrap_or_else(|_| json!({}));

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
    let preferred_head = req.inference_head.clone();
    let (prompt, images) = match render_prompt_with_images(&req.messages, &tools) {
        Ok(rendered) => rendered,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };
    // Chaveia o cache de prefixo pelas mensagens, não pelo texto renderizado -
    // ver `conversation_cache_key`.
    let cache_key = conversation_cache_key(&req.messages, &tools);
    let prompt_tokens = approx_tokens(&prompt);
    let inference_token = auth.token().to_string();
    let caller = auth.into_caller();

    let output_limit = state
        .request_output_limit_for_model(Some(&model), requested_max_tokens, &prompt)
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
    let budget = output_limit.budget();
    let thinking_enabled = req.thinking_enabled.unwrap_or(true);

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
            preferred_head.clone(),
            thinking_enabled,
            request_details.clone(),
            output_limit,
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
            preferred_head,
            thinking_enabled,
            request_details,
            output_limit,
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
            // Completion crua não tem conversa: o prefixo é o próprio texto.
            None,
            None,
            true,
            request_details.clone(),
            output_limit,
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
            true,
            request_details,
            output_limit,
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
    // Um `@<deploy_id>` que ainda não apareceu no anúncio de ninguém continua
    // valendo: recusar aqui trocaria "esta implantação não respondeu" por
    // "atendi noutra", que é pior - o cliente pediu uma configuração
    // específica. Quem não achar peer nenhum falha adiante, dizendo isso.
    if crate::deployment::split_selector(&requested).1.is_some() {
        return Ok(requested);
    }
    Err(format!(
        "modelo '{requested}' não existe ou não está carregado nesta rede; \
         consulte GET /v1/models para os nomes servíveis"
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
    preferred_head: Option<String>,
    thinking_enabled: bool,
    request_details: serde_json::Value,
    output_limit: OutputTokenLimit,
) -> Response {
    match crate::gateway::generate_blocking_with_stops(
        route,
        caller,
        GenerationInput {
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
        Ok((text, summary)) => {
            // Quem encerrou foi o teto, não o modelo. Uma chamada ainda aberta
            // aqui foi cortada ao meio, e é assim que ela volta: estruturada e
            // incompleta, com `finish_reason: "length"` - o que a OpenAI faz.
            // Ver `TagScanner::finish_truncated`.
            let truncated = summary.finish_reason.as_deref() == Some("length");
            let (message_or_text, saw_tool_call, truncated_tool_call): (
                serde_json::Value,
                bool,
                bool,
            ) = match mode {
                Mode::Chat => {
                    let (parts, cut_inside_call) = if truncated {
                        parse_output_truncated(&text)
                    } else {
                        (parse_output(&text), false)
                    };
                    let (message, saw) = chat_message_json(&parts);
                    (message, saw, cut_inside_call)
                }
                Mode::Completion => (json!(text), false, false),
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
                        "total_tokens": prompt_tokens + summary.tokens_generated,
                        // Quantos tokens do prompt o pipeline **não**
                        // reprocessou, medido no lado que executa. Mesmo nome e
                        // lugar que a OpenAI usa, para o cliente ler sem
                        // adaptação. Ver `RequestTrace::cached_tokens`.
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
                        "duration_ms": summary.duration_ms,
                        "timings": summary.timings,
                        // `partial` é `true` sempre que a resposta não é a
                        // que o modelo teria dado: perda de transporte, ou o
                        // teto cortando uma chamada de ferramenta ao meio.
                        "partial": summary.partial || truncated_tool_call,
                        "finish_reason": summary.finish_reason,
                        "stop_sequence": summary.stop_sequence,
                        "trace": summary.trace,
                        "output_limits": output_limit,
                        // `true` quando o teto do cliente caiu **dentro** de
                        // uma chamada de ferramenta. A chamada volta
                        // estruturada e incompleta, com `length`: o cliente
                        // repete o pedido com um teto maior, não executa.
                        "truncated_tool_call": truncated_tool_call,
                        // Quantas gerações de backend esta resposta costurou.
                        // `1` é uma só; maior que `1` diz que a saída passou do
                        // `max_new_tokens` da implantação sem que ele mudasse.
                        "continuation_legs": summary.legs,
                    },
                })),
            )
                .into_response()
        }
        Err(e) => error_response(StatusCode::SERVICE_UNAVAILABLE, e),
    }
}

/// `max_tokens` aqui é o **total** da requisição, nunca o teto de uma perna do
/// laço de continuação: uma perna esgotada não é `length`, é o meio da
/// resposta. Vazar o `length` intermediário para o cliente era o defeito - o
/// harness lê `max_tokens` como condição terminal e larga a chamada de
/// ferramenta pela metade.
///
/// Atingir o total, esse sim, é **parada terminal**: `length` vem primeiro,
/// mesmo com uma chamada de ferramenta na saída, porque a chamada que o teto
/// pegou aberta está incompleta e não é fim de turno. É a regra da OpenAI, e
/// é o que faz o cliente repetir o pedido com um teto maior em vez de executar
/// meia chamada.
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
    // Só chamadas **completas** contam: ver `OutputPart::ToolCallTruncated`.
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
            // A chamada que o teto cortou entra na lista - é o que a OpenAI
            // devolve -, mas **não** conta como chamada vista: o turno não
            // terminou nela, terminou no teto, e o motivo de parada tem de
            // dizer `length`.
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
    preferred_head: Option<String>,
    thinking_enabled: bool,
    request_details: serde_json::Value,
    output_limit: OutputTokenLimit,
) -> Response {
    let (tx, rx) = mpsc::channel::<GenEvent>(64);
    crate::gateway::spawn_logged_generation_with_stops(
        route,
        caller,
        GenerationInput {
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

    let stream = sse_stream(rx, id, created, model, mode, budget.total, output_limit);
    (
        cors_headers(),
        Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()),
    )
        .into_response()
}

/// A OpenAI **sempre** transmite os argumentos em fragmentos, conforme gera -
/// não existe interruptor para isso na API dela, e por isso não há campo a
/// espelhar em `ChatToolDef`. Esta rota é ansiosa sempre.
const EAGER_STREAMING_OPENAI: bool = true;

/// O estado de uma chamada de ferramenta sendo transmitida.
///
/// Ela nasce em `ToolCallOpen`, cresce a cada `ToolCallChunk` e morre no
/// fechamento. Enquanto o nome não sai, nada é transmitido: um índice de
/// `tool_calls` aberto sem nome não serve para nada ao cliente.
struct ChamadaEmCurso {
    /// O corpo cru visto até agora - é dele que o transcodificador vive.
    body: String,
    streamer: crate::tool_call::CallArgsStreamer,
    /// O índice já anunciado ao cliente, quando a abertura já saiu.
    index: Option<u32>,
}

/// Turns scanner events into OpenAI streaming `delta` values. Unlike
/// Anthropic's content blocks, OpenAI's delta model has no block-index
/// concept for plain text at all — just one implicit content stream, plus a
/// separately-indexed `tool_calls` array (`tool_call_index`, bumped once per
/// call). Synchronous and free of any axum/SSE type so it's directly
/// unit-testable.
///
/// **Os argumentos saem em fragmentos, conforme o modelo os gera.** Era aqui
/// que eles ficavam represados: o corpo da chamada era acumulado e só virava
/// delta quando a tag fechava, o que no ksia deu 663 caracteres num pedaço só,
/// aos 11,01 s. `CallArgsStreamer` transcodifica a marcação em JSON conforme
/// ela chega, e a concatenação dos fragmentos é exatamente a string que o
/// caminho não-streaming produziria.
fn apply_scan_events_openai(
    events: Vec<ScanEvent>,
    tool_call_index: &mut u32,
    saw_tool_call: &mut bool,
    em_curso: &mut Option<ChamadaEmCurso>,
    out: &mut Vec<serde_json::Value>,
) {
    /// A abertura: índice, id e nome, com os argumentos ainda vazios. Sai uma
    /// vez só por chamada, no primeiro fragmento que houver.
    fn abrir(
        nome: &str,
        tool_call_index: &mut u32,
        em_curso: &mut ChamadaEmCurso,
        out: &mut Vec<serde_json::Value>,
    ) -> u32 {
        if let Some(idx) = em_curso.index {
            return idx;
        }
        let idx = *tool_call_index;
        *tool_call_index += 1;
        em_curso.index = Some(idx);
        out.push(json!({
            "tool_calls": [{
                "index": idx,
                "id": format!("call_{}", uuid::Uuid::new_v4().simple()),
                "type": "function",
                "function": { "name": nome, "arguments": "" },
            }]
        }));
        idx
    }

    fn fragmento(idx: u32, texto: String, out: &mut Vec<serde_json::Value>) {
        out.push(json!({
            "tool_calls": [{
                "index": idx,
                "function": { "arguments": texto },
            }]
        }));
    }

    for e in events {
        match e {
            ScanEvent::Text(s) if !s.is_empty() => out.push(json!({ "content": s })),
            ScanEvent::Text(_) => {}
            ScanEvent::ToolCallOpen => {
                *em_curso = Some(ChamadaEmCurso {
                    body: String::new(),
                    streamer: crate::tool_call::CallArgsStreamer::new(EAGER_STREAMING_OPENAI),
                    index: None,
                });
            }
            ScanEvent::ToolCallChunk(fragment) => {
                let Some(chamada) = em_curso.as_mut() else {
                    continue;
                };
                chamada.body.push_str(&fragment);
                let corpo = chamada.body.clone();
                if let Some(delta) = chamada.streamer.advance(&corpo) {
                    let nome = crate::tool_call::peek_call_name(&corpo).unwrap_or_default();
                    let idx = abrir(&nome, tool_call_index, chamada, out);
                    fragmento(idx, delta, out);
                }
            }
            ScanEvent::ToolCallClose(Ok((name, arguments))) => {
                *saw_tool_call = true;
                let mut chamada = em_curso.take().unwrap_or_else(|| ChamadaEmCurso {
                    body: String::new(),
                    streamer: crate::tool_call::CallArgsStreamer::new(EAGER_STREAMING_OPENAI),
                    index: None,
                });
                let idx = abrir(&name, tool_call_index, &mut chamada, out);
                // Só o que ainda não saiu. Repetir o que o cliente já
                // acumulou duplicaria os argumentos dele.
                if let Some(delta) = chamada.streamer.finish_complete(&chamada.body, &arguments) {
                    fragmento(idx, delta, out);
                }
            }
            ScanEvent::ToolCallClose(Err(raw)) => {
                let chamada = em_curso.take();
                // Uma chamada que já começou a sair não pode virar texto: o
                // cliente já tem um `tool_calls[i]` aberto com nome, e não há
                // como desenviar. Ela fica como está - argumentos que não
                // parseiam, que é o caso "JSON inválido" que os provedores
                // documentam e mandam devolver ao modelo.
                if chamada.is_some_and(|c| c.streamer.emitted_len() > 0) {
                    continue;
                }
                apply_scan_events_openai(
                    vec![ScanEvent::Text(raw)],
                    tool_call_index,
                    saw_tool_call,
                    em_curso,
                    out,
                );
            }
            // A chamada cortada pelo teto sai na mesma forma, e os argumentos
            // acabam na string JSON inacabada da Fase 1 - o que já tinha sido
            // transmitido conta, então só o que falta sai aqui.
            // `saw_tool_call` **não** é marcado: o turno terminou no teto, e o
            // quadro final diz `length`.
            ScanEvent::ToolCallTruncated(partial) => {
                let mut chamada = em_curso.take().unwrap_or_else(|| ChamadaEmCurso {
                    body: String::new(),
                    streamer: crate::tool_call::CallArgsStreamer::new(EAGER_STREAMING_OPENAI),
                    index: None,
                });
                let idx = abrir(&partial.name, tool_call_index, &mut chamada, out);
                if let Some(delta) = chamada.streamer.finish_truncated(&partial.arguments) {
                    fragmento(idx, delta, out);
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
fn sse_stream(
    rx: mpsc::Receiver<GenEvent>,
    id: String,
    created: i64,
    model: String,
    mode: Mode,
    max_tokens: u32,
    output_limit: OutputTokenLimit,
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
        /// `true` quando o teto do cliente caiu dentro de uma chamada de
        /// ferramenta. Vai para o `x_pom` do quadro final.
        truncated_tool_call: bool,
        /// A chamada cujos argumentos estão sendo transmitidos agora.
        em_curso: Option<ChamadaEmCurso>,
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
        scanner: TagScanner::new(),
        tool_call_index: 0,
        saw_tool_call: false,
        truncated_tool_call: false,
        em_curso: None,
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
                                    &mut s.em_curso,
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
                    Some(GenEvent::Done(summary)) => {
                        if s.mode == Mode::Chat {
                            let scanner = std::mem::replace(&mut s.scanner, TagScanner::new());
                            // A chamada cortada só pode sair estruturada aqui
                            // porque nada de dentro da tag foi transmitido:
                            // `apply_scan_events_openai` engole `ToolCallOpen`
                            // e `ToolCallChunk`, e o scanner segura o corpo até
                            // a tag fechar. Nada a desenviar, nada vazado.
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
                                &mut s.em_curso,
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

/// [`chunk_json`] com o único dado que só o quadro final conhece: se o teto
/// cortou uma chamada de ferramenta ao meio.
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
        payload["x_pom"] = json!({
            "source": summary.source,
            "tokens_per_sec": summary.tokens_per_sec,
            "tokens_generated": summary.tokens_generated,
            "duration_ms": summary.duration_ms,
            "timings": summary.timings,
            // Ver o mesmo campo no caminho não-streaming: uma chamada
            // cortada ao meio também é resposta parcial.
            "partial": summary.partial || truncated_tool_call,
            "finish_reason": summary.finish_reason,
            "stop_sequence": summary.stop_sequence,
            "trace": summary.trace,
            "output_limits": output_limit,
            "continuation_legs": summary.legs,
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

    #[test]
    fn image_is_rejected_when_the_served_model_has_no_mmproj() {
        assert_eq!(
            image_vision_error(&[vec![0xFF, 0xD8]], None),
            Some("este modelo não tem visão")
        );
        assert_eq!(
            image_vision_error(&[vec![0xFF, 0xD8]], Some("mmproj.gguf")),
            None
        );
        assert_eq!(image_vision_error(&[], None), None);
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

    /// A CLI default is only a selector for the inference path. Depois de um
    /// unload, ele não é um modelo publicado e não pode voltar ao catálogo
    /// para enganar um cliente que autodetecta modelos.
    #[tokio::test]
    async fn model_discovery_ignores_an_unpublished_default_model() {
        let mut state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.default_model = "stale-after-unload".into();

        assert!(model_ids(&state).await.is_empty());
    }

    /// Um anúncio que perdeu a capacidade efetiva representa um peer em
    /// restart, não uma implantação pronta. Ele não pode manter o modelo no
    /// catálogo autodetectado enquanto a camada antiga ainda estiver viva na
    /// tabela de gossip.
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
            Ok("qwen3:8b@d-cccccccc".to_string())
        );
    }

    /// Um nome pelado que nenhum peer anuncia é recusado, não trocado
    /// caladamente pelo modelo ativo - decisão do captain: o cliente que
    /// pediu um modelo inexistente precisa de um erro claro, nunca de uma
    /// resposta 200 servida por outro modelo.
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
            Json(ChatRequest {
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

    /// O que o cliente lê como economia é o que o pipeline **executou**. A
    /// cabeça informa `cached_tokens` no pedaço 0 e o número atravessa
    /// `PrefillOutcome` → `RequestTrace` → `usage`. Antes disto o cliente não
    /// tinha como saber que o acerto de cache não estava economizando nada.
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
            Json(ChatRequest {
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

    /// A resposta do primeiro chunk pode terminar no token 42 mesmo quando o
    /// wire usa chunks de 512. O gateway não pode transformar a ausência de
    /// `next_pos` em `chunk_index * 512`: antes da correção isso despachava o
    /// chunk 1 na posição 512, o peer respondia 400 e o SSE expunha
    /// `server_error` antes de concluir o fallback frio.
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
                                    "chunk fora de ordem: recebi 1 (pos 512) mas a sessão está em 42",
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
            Json(ChatRequest {
                model: Some("whole-model".into()),
                messages: vec![msg("user", "primeiro")],
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
            Json(ChatRequest {
                model: Some("whole-model".into()),
                messages: vec![
                    msg("user", "primeiro"),
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
            "posição derivada do índice enviada ao peer: {frames:?}"
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
            Json(ChatRequest {
                // Sem peer anunciando "lab" nesta fixture, um `model`
                // explícito seria recusado pelo `resolve_model` - o alvo deste
                // teste é a matemática de orçamento/contexto, não a resolução
                // de nome, então omitir usa `active_model()` diretamente.
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

    /// A ausência de um teto absoluto artificial não transforma o pedido em
    /// erro. O contexto efetivo é quem reduz o orçamento, se necessário.
    #[tokio::test]
    async fn a_request_above_the_old_absolute_default_is_not_rejected() {
        assert_eq!(streaming_chat_status(1, 32_000).await, StatusCode::OK);
    }

    /// E a recusa por contexto cheio continua sendo 413, com o pedido dentro
    /// da capacidade: são dois motivos diferentes, com códigos diferentes.
    #[tokio::test]
    async fn a_prompt_that_fills_the_context_is_still_rejected_before_sse_starts() {
        assert_eq!(
            streaming_chat_status(2_000, 4_000).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn single_user_message_is_labelled_like_every_other_turn() {
        // O rótulo vale desde o primeiro turno: é ele que faz o prompt de um
        // turno ser prefixo exato do prompt do seguinte. Ver `render_prompt`.
        assert_eq!(
            render_prompt(&[msg("user", "O que e inferencia distribuida?")], &[]),
            "User: O que e inferencia distribuida?"
        );
    }

    /// A propriedade de que o reuso de prefixo depende, afirmada diretamente:
    /// acrescentar mensagens só acrescenta texto no fim.
    #[test]
    fn every_extra_turn_only_appends_to_the_previous_prompt() {
        let turno1 = vec![msg("user", "1+1?")];
        let turno2 = vec![
            msg("user", "1+1?"),
            msg("assistant", "2"),
            msg("user", "e 2+2?"),
        ];
        let turno3 = {
            let mut m = turno2.clone();
            m.push(msg("assistant", "4"));
            m.push(msg("user", "e 3+3?"));
            m
        };

        let p1 = render_prompt(&turno1, &[]);
        let p2 = render_prompt(&turno2, &[]);
        let p3 = render_prompt(&turno3, &[]);

        assert!(
            p2.starts_with(&p1),
            "turno 2 não continua o turno 1:\n{p1}\n---\n{p2}"
        );
        assert!(p3.starts_with(&p2), "turno 3 não continua o turno 2");
        // E a chave do cache acompanha, que é o que `PrefixCache` consulta.
        let k1 = conversation_cache_key(&turno1, &[]);
        let k2 = conversation_cache_key(&turno2, &[]);
        let k3 = conversation_cache_key(&turno3, &[]);
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
            "o cache não pode declarar prefixo quando a renderização inseriu contexto no início"
        );
    }

    /// Trocar a lista de ferramentas move o começo do prompt, e o prefixo
    /// **tem** que ser descartado - a chave precisa refletir isso.
    #[test]
    fn changing_the_tool_list_breaks_the_cache_key_on_purpose() {
        let messages = vec![msg("user", "oi")];
        let ferramenta = ToolDef {
            eager_input_streaming: None,
            name: "buscar".into(),
            description: None,
            input_schema: json!({"type": "object"}),
        };
        let sem = conversation_cache_key(&messages, &[]);
        let com = conversation_cache_key(&messages, std::slice::from_ref(&ferramenta));
        assert!(
            !com.starts_with(&sem),
            "a chave com ferramenta não pode continuar a sem"
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

    /// `context_length` e `max_completion_tokens` vêm do contexto efetivo da
    /// implantação. Anunciar o `max_new_tokens` (aqui, 1000) faria um cliente
    /// confundir o tamanho de uma perna com o orçamento total.
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
        ))
        .unwrap();

        assert_eq!(value["object"], "model");
        assert_eq!(value["type"], "model");
        assert_eq!(value["display_name"], "Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf");
        assert_eq!(value["created_at"], "1970-01-01T00:00:00Z");
        assert_eq!(value["max_input_tokens"], 100_000);
        assert_eq!(value["max_tokens"], 100_000);
        assert!(value["capabilities"].is_null());
        assert_eq!(value["context_length"], 100_000);
        assert_eq!(value["top_provider"]["max_completion_tokens"], 100_000);
    }

    #[test]
    fn finish_reason_reflects_the_limit_and_tool_calls() {
        let summary = |n: u32| crate::gateway::GenSummary {
            source: "pipeline:a→b".into(),
            tokens_generated: n,
            tokens_per_sec: 1.0,
            duration_ms: 10,
            timings: crate::gateway::timing_metrics(10, 10, 10, 0, n),
            partial: false,
            finish_reason: None,
            stop_sequence: None,
            legs: 1,
            trace: None,
        };
        // Complete tool call within the budget: the model ended its turn by
        // asking for the tool, so the protocol must preserve tool_calls.
        assert_eq!(finish_reason(&summary(3640), 4000, true), "tool_calls");
        // Atingir o teto é parada terminal e vence uma chamada na saída: a
        // chamada que o teto pegou aberta está incompleta, e é o `length` que
        // faz o cliente repetir o pedido em vez de executá-la.
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
        let (message, saw_tool_call) = chat_message_json(&parse_output(raw));

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

    #[test]
    fn final_stream_chunk_carries_generation_summary() {
        let summary = crate::gateway::GenSummary {
            source: "pipeline:a→b".into(),
            tokens_generated: 12,
            tokens_per_sec: 4.5,
            duration_ms: 2666,
            timings: crate::gateway::timing_metrics(2666, 900, 900, 12, 12),
            partial: true,
            finish_reason: Some("cycle".into()),
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
        .expect("JSON válido");

        assert_eq!(payload["x_pom"]["tokens_per_sec"], 4.5);
        assert_eq!(payload["x_pom"]["tokens_generated"], 12);
        assert_eq!(payload["x_pom"]["timings"]["ttft_ms"], 900);
        assert!(payload["x_pom"]["timings"]["generation_tokens_per_sec"].is_number());
        assert_eq!(payload["x_pom"]["trace"]["model"], "qwen35");
        assert_eq!(payload["choices"][0]["finish_reason"], "stop");
        assert_eq!(payload["x_pom"]["finish_reason"], "cycle");
        assert_eq!(payload["x_pom"]["partial"], true);
    }
}
