use crate::auth::AuthState;
use crate::model::LayerAssignment;
use crate::network::{Network, PeerInfo};
use crate::ollama::OllamaClient;
use crate::pipeline::{plan_pipeline, LoadTracker, PipelinePlan};
use crate::runtime::NodeRuntime;
use crate::{admin, session};
use anyhow::Result;
use std::time::{Duration, Instant};
use uuid::Uuid;
use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Tokens `/api/generate` will produce before stopping on its own.
const DEFAULT_MAX_TOKENS: u32 = 512;


/// Characters per token, used only to size up a prompt before sending it.
/// Deliberately pessimistic — the real tokenizer lives in the first peer's
/// llama.cpp context, and under-estimating here would let through the very
/// request this guard exists to stop.
const CHARS_PER_TOKEN: usize = 3;

// ── State ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct GatewayState {
    pub network: Arc<Network>,
    pub ollama: Arc<OllamaClient>,
    pub default_model: String,
    /// Current assignment of the local node.
    pub local_assignment: Arc<tokio::sync::RwLock<Option<LayerAssignment>>>,
    /// Tracks in-flight inference requests per peer (for pipeline load balancing).
    pub loads: Arc<LoadTracker>,
    /// Shared HTTP client with long timeout for pipeline calls.
    pub http: Arc<reqwest::Client>,
    /// Mutable node configuration driven by the web UI.
    pub runtime: Arc<NodeRuntime>,
    /// HTTP port this node serves the API and UI on.
    pub gateway_port: u16,
    /// Setup-level configuration (role, connection key, bootstrap nodes).
    pub auth: AuthState,
    /// In-flight slice transfers and the offers waiting for an answer.
    pub transfers: crate::transfer::Transfers,
}

impl axum::extract::FromRef<GatewayState> for AuthState {
    fn from_ref(state: &GatewayState) -> Self {
        state.auth.clone()
    }
}

impl GatewayState {
    /// Model label the pipeline planner matches on right now.
    pub async fn active_model(&self) -> String {
        self.runtime.model_label(&self.default_model).await
    }

    /// Where this node writes slices, keeps received files, and the only
    /// directory a peer may pull from. Read from the live configuration, so a
    /// change on the screen takes effect without a restart.
    pub async fn shard_dir(&self) -> std::path::PathBuf {
        self.auth.read().await.shard_dir_path()
    }

    /// Directories of whole models this node scans.
    pub async fn model_dirs(&self) -> Vec<std::path::PathBuf> {
        self.auth.read().await.model_dir_paths()
    }

    /// Largest prompt this node's inference server can take, in tokens.
    ///
    /// llama.cpp aborts the *whole process* on
    /// `GGML_ASSERT(n_tokens_all <= cparams.n_batch)`, so an oversized prompt
    /// does not fail one request — it kills the peer and every request in
    /// flight on it. Coding harnesses send multi-thousand-token system prompts,
    /// which makes that trivially reachable from the public API. Refusing early
    /// keeps the node up; the caller gets a message naming the flags to raise.
    pub async fn prompt_token_budget(&self) -> Option<u32> {
        Some(prompt_budget(&self.runtime.config().await?))
    }

    /// Alive peers plus this node, so the local assignment participates in
    /// planning exactly like a remote one.
    pub async fn all_peers(&self) -> Vec<PeerInfo> {
        let mut peers = self.network.alive_peers().await;
        // This machine is always part of the cluster, model or no model. Hiding
        // it when there is no assignment made the whole machine list disappear
        // on a node configured through the web UI.
        {
            let local = self.local_assignment.read().await.clone();
            peers.push(PeerInfo {
                node_id: self.network.node_id.clone(),
                pubkey: self.network.pubkey.clone(),
                addr: format!("127.0.0.1:{}", self.network.bind_addr.port()),
                model: local.as_ref().map(|l| l.model.clone()).unwrap_or_default(),
                start_layer: local.as_ref().map(|l| l.start_layer).unwrap_or(0),
                end_layer: local.as_ref().map(|l| l.end_layer).unwrap_or(0),
                total_layers: local.as_ref().map(|l| l.total_layers).unwrap_or(0),
                memory_mb: local.as_ref().map(|l| l.memory_used_mb).unwrap_or(0.0),
                last_seen: chrono::Utc::now(),
                ollama_port: self.network.ollama_port,
                inference_port: self.network.inference_port,
                gateway_port: Some(self.gateway_port),
                role: self.network.role.clone(),
                hostname: self.network.hostname.clone(),
                connection_verified: false,
                disk_total_mb: 0,
                disk_free_mb: 0,
                api_keys_active: 0,
            });
        }
        peers
    }
}

/// Prompt tokens a peer running `cfg` can accept.
///
/// The KV cache is split evenly across `max_sessions` slots, so the window one
/// request sees is `n_ctx / max_sessions` — this divisor used to be hardcoded
/// to 8, which silently cut the configured context to an eighth and produced
/// "contexto insuficiente" no matter how high `--n-ctx` was set.
///
/// `n_batch` deliberately does **not** appear here. It did while the prompt
/// crossed as a single `llama_batch`; since [`crate::prefill`] advances the
/// prompt in chunks, `n_batch` bounds one *chunk* — see
/// [`prompt_chunk_tokens`]. Leaving it in this formula capped every prompt at
/// 512 tokens no matter how large `--n-ctx` was, which is what a harness sending
/// a real conversation hits immediately.
pub fn prompt_budget(cfg: &crate::runtime::NodeConfig) -> u32 {
    let per_session_ctx = cfg.n_ctx / cfg.max_sessions.max(1);
    // Room has to be left for the tokens we are about to generate.
    per_session_ctx.saturating_sub(cfg.max_new_tokens)
}

/// Tokens per prefill chunk — this is what `n_batch` actually constrains.
///
/// A chunk is decoded as one batch on the peer, so a configured chunk larger
/// than `n_batch` would abort llama.cpp exactly the way whole prompts used to.
pub fn prompt_chunk_tokens(cfg: &crate::runtime::NodeConfig) -> u32 {
    cfg.prefill_chunk_tokens
        .max(1)
        .min(cfg.n_batch.max(1))
}

// ── Peer-to-peer pipeline protocol ───────────────────────────────────────────

/// Sent to each peer's /v1/infer endpoint.
#[derive(Debug, Serialize)]
struct PeerInferRequest<'a> {
    model: &'a str,
    /// Unique per generation; inference server uses it to track KV cache per session.
    session_id: &'a str,
    /// Raw prompt text — only set for the FIRST peer in the pipeline (first step).
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<&'a str>,
    /// Token IDs — set for subsequent steps in the generation loop.
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<&'a [u32]>,
    /// Base64-encoded float32 hidden-state tensor.
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden_state: Option<&'a str>,
    /// Shape [n_tokens, hidden_dim] of hidden_state.
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden_shape: Option<[usize; 2]>,
    start_layer: u32,
    end_layer: u32,
    /// True only for the last peer in the pipeline.
    is_last: bool,
    /// Whether to clear KV cache.
    reset: bool,
    /// Activation scale factor.
    scale: f32,
}

/// Response from each peer's /v1/infer endpoint.
#[derive(Debug, Deserialize)]
struct PeerInferResponse {
    /// Populated by non-last peers.
    hidden_state: Option<String>,
    hidden_shape: Option<[usize; 2]>,
    /// Populated by the last peer.
    text: Option<String>,
    token_id: Option<u32>,
    done: bool,
}

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GenerateRequest {
    pub model: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Serialize)]
pub struct GenerateResponse {
    pub response: String,
    /// "pipeline:<hop-chain>" | "local" | "peer:<node_id>" | "error"
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct PeersResponse {
    pub peers: Vec<PeerSummary>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct PeerSummary {
    pub node_id: String,
    pub addr: String,
    pub model: String,
    pub layers: String,
    pub ollama_url: Option<String>,
    pub inference_url: Option<String>,
    pub load: u32,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub ollama_available: bool,
    pub peer_count: usize,
    pub pipeline_peers: usize,
    pub default_model: String,
}

// ── Streaming generation ─────────────────────────────────────────────────────

/// One event from a running generation.
#[derive(Debug, Clone)]
pub enum GenEvent {
    /// A newly decoded token's text.
    Token(String),
    /// Generation finished (possibly early — see `partial`).
    Done(GenSummary),
    /// Generation produced nothing usable.
    Failed(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct GenSummary {
    /// "pipeline:<hop-chain>", with " (partial)" when it stopped early.
    pub source: String,
    pub tokens_generated: u32,
    pub tokens_per_sec: f64,
    pub duration_ms: u64,
    /// True when a peer failed mid-generation but some text was salvaged.
    pub partial: bool,
}

/// Execute a single forward pass through the pipeline.
async fn execute_pipeline_step(
    plan: &crate::pipeline::PipelinePlan,
    model: &str,
    session_id: &str,
    prompt: Option<&str>,
    tokens: Option<&[u32]>,
    reset: bool,
    scale: f32,
    http: &reqwest::Client,
    loads: &LoadTracker,
) -> Result<StepResult> {
    let n = plan.steps.len();
    let mut hidden_state: Option<String> = None;
    let mut hidden_shape: Option<[usize; 2]> = None;

    for (i, step) in plan.steps.iter().enumerate() {
        let is_first = i == 0;
        let is_last = i == n - 1;

        let _guard = loads.acquire(&step.node_id).await;

        let req = PeerInferRequest {
            model,
            session_id,
            prompt: if is_first { prompt } else { None },
            tokens: if is_first { tokens } else { None },
            hidden_state: hidden_state.as_deref(),
            hidden_shape,
            start_layer: step.start_layer,
            end_layer: step.end_layer,
            is_last,
            reset,
            scale: if is_first { 1.0 } else { scale },
        };

        let url = format!("{}/v1/infer", step.inference_url);
        let resp = http
            .post(&url)
            .json(&req)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("peer {} unreachable: {}", &step.node_id[..8], e))?;

        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("peer {} returned {}: {}", &step.node_id[..8], code, body));
        }

        let peer_resp: PeerInferResponse = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("peer {} bad JSON: {}", &step.node_id[..8], e))?;

        if is_last {
            return Ok(StepResult {
                text: peer_resp.text.unwrap_or_default(),
                token_id: peer_resp.token_id.unwrap_or(0),
                done: peer_resp.done,
            });
        }

        hidden_state = peer_resp.hidden_state;
        hidden_shape = peer_resp.hidden_shape;
    }

    Err(anyhow::anyhow!("pipeline completed without a final response"))
}

#[derive(Debug)]
struct StepResult {
    text: String,
    token_id: u32,
    done: bool,
}

/// Run one generation, emitting each token as it is decoded.
///
/// This is the single generation path in the gateway: `/api/generate` collects
/// the events into one response, the OpenAI endpoints forward them as SSE. The
/// per-token loop and the peer protocol are unchanged — one round trip through
/// every peer yields exactly one token.
pub async fn generate_stream(
    state: GatewayState,
    model: String,
    prompt: String,
    max_tokens: u32,
    tx: mpsc::Sender<GenEvent>,
) {
    // Size the prompt up before any peer sees it — see `prompt_token_budget`.
    if let Some(cfg) = state.runtime.config().await {
        let budget = prompt_budget(&cfg);
        let estimated = estimate_tokens(&prompt);
        if estimated > budget {
            let _ = tx
                .send(GenEvent::Failed(format!(
                    "Prompt grande demais: ~{estimated} tokens estimados contra um limite de \
                     {budget}. O limite é n_ctx/{} ({}) menos max_new_tokens ({}). O prompt vai \
                     em pedaços de {} tokens, então n_batch ({}) não limita o prompt. Aumente \
                     n_ctx em Configurar → ajustes avançados, reduza max_new_tokens, ou reduza \
                     as requisições simultâneas.",
                    cfg.max_sessions.max(1),
                    cfg.n_ctx / cfg.max_sessions.max(1),
                    cfg.max_new_tokens,
                    prompt_chunk_tokens(&cfg),
                    cfg.n_batch,
                )))
                .await;
            return;
        }
    }

    let peers = state.all_peers().await;

    // Total layers for THIS model, as reported by a peer that serves it.
    let total_layers = peers
        .iter()
        .find(|p| p.model == model)
        .map(|p| p.total_layers)
        .unwrap_or(32);

    // Each request gets a unique session_id passed to every inference server
    // call, so parallel requests keep isolated KV cache state.
    let session_id = Uuid::new_v4().to_string();

    let Some(plan) = plan_pipeline(&peers, &model, total_layers, &state.loads).await else {
        let _ = tx
            .send(GenEvent::Failed(format!(
                "Nenhum pipeline válido para o modelo '{model}' (verifique a cobertura de camadas 0..{total_layers})"
            )))
            .await;
        return;
    };

    info!(
        "🔀 Pipeline planejado: {} hops [{}] session={} max_tokens={}",
        plan.steps.len(),
        plan.hop_chain(),
        &session_id[..8],
        max_tokens,
    );

    let gen_start = Instant::now();
    let mut tokens_generated: u32 = 0;
    let max_tokens = max_tokens.max(1);

    // Chunk size and wire precision come from the node's configuration, so
    // both are operator knobs rather than constants baked into the protocol.
    let (chunk_tokens, dtype) = {
        let cfg = state.runtime.config().await;
        (
            cfg.as_ref().map(|c| c.prefill_chunk_tokens).unwrap_or(crate::prefill::DEFAULT_CHUNK_TOKENS),
            crate::wire::WireDtype::F16,
        )
    };

    // Whatever ends this generation — EOS, the token limit, the client hanging
    // up, or a peer erroring — every peer has to get its KV slot back. Only the
    // sampling peer used to free anything, and only on EOS, so the first peer
    // leaked a slot per request and died after eight of them.
    async {
    // ── Prefill: the prompt goes in chunk by chunk, and the last peer hands
    // back the first token once the whole prompt is in.
    let first = match crate::prefill::run(
        &plan, &session_id, &prompt, chunk_tokens, dtype, &state.http,
    )
    .await
    {
        Ok(o) => {
            info!(
                "📥 prompt de {} tokens em {} pedaço(s) · {:.1}s até a primeira palavra",
                o.prompt_tokens,
                o.chunks,
                gen_start.elapsed().as_secs_f64(),
            );
            o
        }
        Err(e) => {
            let _ = tx.send(GenEvent::Failed(format!("Prefill falhou: {e}"))).await;
            return;
        }
    };

    let mut next_token = first.token_id;
    tokens_generated += 1;
    if !first.text.is_empty() && tx.send(GenEvent::Token(first.text)).await.is_err() {
        info!("cliente desconectou logo após o prompt");
        return;
    }
    if first.done {
        let elapsed = gen_start.elapsed();
        let _ = tx
            .send(GenEvent::Done(GenSummary {
                source: format!("pipeline:{}", plan.hop_chain()),
                tokens_generated,
                tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
                duration_ms: elapsed.as_millis() as u64,
                partial: false,
            }))
            .await;
        return;
    }

    for i in 0..max_tokens {
        let res = crate::prefill::step(&plan, &session_id, next_token, dtype, &state.http).await;

        match res {
            Ok((token_id, text, done)) => {
                next_token = token_id;
                let step = StepResult { text, token_id, done };
                tokens_generated += 1;
                if !step.text.is_empty() && tx.send(GenEvent::Token(step.text)).await.is_err() {
                    // Client hung up; stop burning GPU time on a dead request.
                    info!("cliente desconectou após {tokens_generated} tokens");
                    return;
                }

                if step.done || i == max_tokens - 1 {
                    let elapsed = gen_start.elapsed();
                    info!(
                        "✅ Pipeline completo: {} tokens em {:.1}s ({:.1} tok/s)",
                        tokens_generated,
                        elapsed.as_secs_f64(),
                        rate(tokens_generated, elapsed.as_secs_f64())
                    );
                    let _ = tx
                        .send(GenEvent::Done(GenSummary {
                            source: format!("pipeline:{}", plan.hop_chain()),
                            tokens_generated,
                            tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
                            duration_ms: elapsed.as_millis() as u64,
                            partial: false,
                        }))
                        .await;
                    return;
                }

            }
            Err(e) => {
                warn!("Erro no passo {i} do pipeline: {e}");
                let elapsed = gen_start.elapsed();
                if tokens_generated > 0 {
                    // Some output is better than none — the original behaviour.
                    info!("devolvendo {tokens_generated} tokens parciais");
                    let _ = tx
                        .send(GenEvent::Done(GenSummary {
                            source: format!("pipeline:{} (partial)", plan.hop_chain()),
                            tokens_generated,
                            tokens_per_sec: rate(tokens_generated, elapsed.as_secs_f64()),
                            duration_ms: elapsed.as_millis() as u64,
                            partial: true,
                        }))
                        .await;
                } else {
                    let _ = tx
                        .send(GenEvent::Failed(format!("Pipeline falhou no passo {i}: {e}")))
                        .await;
                }
                return;
            }
        }
    }
    }
    .await;

    release_sessions(&plan, &session_id, &state.http).await;
}

/// Hand every peer in the plan its KV slot back. Best effort: a peer that is
/// already gone has nothing to release, and failing here must not turn a good
/// answer into an error.
async fn release_sessions(plan: &PipelinePlan, session_id: &str, http: &reqwest::Client) {
    for step in &plan.steps {
        let url = format!("{}/v1/session/release", step.inference_url);
        match http
            .post(&url)
            .json(&serde_json::json!({ "session_id": session_id }))
            .timeout(Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => warn!("release de sessão em {} respondeu {}", step.node_id, r.status()),
            Err(e) => warn!("release de sessão em {} falhou: {e}", step.node_id),
        }
    }
}

/// Pessimistic token count for a prompt, used only by the size guard.
fn estimate_tokens(prompt: &str) -> u32 {
    prompt.chars().count().div_ceil(CHARS_PER_TOKEN) as u32
}

fn rate(tokens: u32, secs: f64) -> f64 {
    if secs > 0.0 {
        tokens as f64 / secs
    } else {
        0.0
    }
}

/// Start a generation and write down what it did, without the generator having
/// to know who asked for it.
///
/// Identity lives with the HTTP route (it is in the `Authorization` header) and
/// the pipeline lives with the generator, so neither one alone can fill in the
/// Logs screen. Forwarding the events through here joins the two without
/// putting a caller argument through every early return in `generate_stream` —
/// which is the hot inference path and not worth destabilising for a log line.
#[allow(clippy::too_many_arguments)]
pub fn spawn_logged_generation(
    state: GatewayState,
    route: &'static str,
    caller: crate::reqlog::Caller,
    model: String,
    prompt: String,
    max_tokens: u32,
    tx: mpsc::Sender<GenEvent>,
) {
    let entry_node_id = state.network.node_id.clone();
    let prompt_tokens = estimate_tokens(&prompt);
    let logged_model = model.clone();

    let (inner_tx, mut inner_rx) = mpsc::channel::<GenEvent>(64);
    tokio::spawn(generate_stream(state, model, prompt, max_tokens, inner_tx));

    tokio::spawn(async move {
        let started = Instant::now();
        let mut generated: u32 = 0;
        // `None` means the client hung up before the generation ended, which is
        // its own outcome and worth seeing in the log.
        let mut outcome: Option<Result<GenSummary, String>> = None;

        while let Some(event) = inner_rx.recv().await {
            let terminal = match &event {
                GenEvent::Token(_) => {
                    generated += 1;
                    None
                }
                GenEvent::Done(summary) => Some(Ok(summary.clone())),
                GenEvent::Failed(e) => Some(Err(e.clone())),
            };
            if tx.send(event).await.is_err() {
                break;
            }
            if terminal.is_some() {
                outcome = terminal;
                break;
            }
        }

        let (source, hops, completion_tokens, duration_ms, status, error) = match outcome {
            Some(Ok(s)) => (
                s.source.clone(),
                s.source
                    .strip_prefix("pipeline:")
                    .unwrap_or_default()
                    .split('→')
                    .filter(|h| !h.is_empty())
                    .map(|h| h.trim_end_matches(" (partial)").to_string())
                    .collect(),
                Some(s.tokens_generated),
                Some(s.duration_ms),
                if s.partial { "partial" } else { "ok" },
                None,
            ),
            Some(Err(e)) => (
                "none".to_string(),
                Vec::new(),
                None,
                Some(started.elapsed().as_millis() as u64),
                "error",
                Some(e),
            ),
            None => (
                "none".to_string(),
                Vec::new(),
                Some(generated),
                Some(started.elapsed().as_millis() as u64),
                "abandoned",
                Some("o cliente desconectou antes do fim".to_string()),
            ),
        };

        crate::reqlog::record(crate::reqlog::Entry {
            id: Uuid::new_v4().simple().to_string(),
            at: chrono::Utc::now().to_rfc3339(),
            route: route.to_string(),
            model: logged_model,
            token_label: caller.token_label,
            token_hint: caller.token_hint,
            token_node_id: caller.token_node_id,
            entry_node_id,
            hops,
            source,
            prompt_tokens: Some(prompt_tokens),
            completion_tokens,
            duration_ms,
            status,
            error,
        })
        .await;
    });
}

/// Run a generation to completion and return the whole text.
pub async fn generate_blocking(
    state: GatewayState,
    route: &'static str,
    caller: crate::reqlog::Caller,
    model: String,
    prompt: String,
    max_tokens: u32,
) -> std::result::Result<(String, GenSummary), String> {
    let (tx, mut rx) = mpsc::channel(64);
    spawn_logged_generation(state, route, caller, model, prompt, max_tokens, tx);

    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            GenEvent::Token(t) => text.push_str(&t),
            GenEvent::Done(summary) => return Ok((text, summary)),
            GenEvent::Failed(e) => return Err(e),
        }
    }
    Err("geração terminou sem resposta".to_string())
}

// ── Server startup ────────────────────────────────────────────────────────────

pub async fn start_gateway(state: GatewayState, port: u16) -> Result<()> {
    let admin = admin::admin_routes().with_state(state.clone());
    let app = Router::new()
        .route("/api/generate", post(handle_generate))
        .route("/generate", post(handle_generate)) // alias
        .route("/peers", get(handle_peers))
        .route("/health", get(handle_health))
        .route("/handshake", post(session::handle_handshake))
        .merge(crate::openai::routes())
        .merge(crate::webui::routes())
        .merge(crate::deploy::routes())
        .merge(crate::transfer::routes())
        .nest("/", admin)
        // Genuinely last: this one ends in a `/*path` catch-all that hands
        // unknown paths to the client router, so anything registered after it
        // would never be reached.
        .merge(crate::webapp::routes())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("[::]:{}", port)).await?;
    info!(
        "🌐 Interface e API em http://localhost:{port}  \
         — UI /  ·  OpenAI /v1/chat/completions  ·  POST /api/generate  ·  GET /peers /health"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Route handlers ────────────────────────────────────────────────────────────

/// Accept raw bytes so callers don't need `Content-Type: application/json`.
async fn handle_generate(State(state): State<GatewayState>, body: Bytes) -> impl IntoResponse {
    let req: GenerateRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(GenerateResponse {
                    response: format!("Invalid JSON: {}", e),
                    source: "error".to_string(),
                    tokens_generated: 0,
                    tokens_per_sec: 0.0,
                    duration_ms: 0,
                }),
            )
                .into_response();
        }
    };

    let model = match req.model.as_deref() {
        Some(m) => m.to_string(),
        None => state.active_model().await,
    };

    info!("📨 /api/generate model={} prompt_len={}", model, req.prompt.len());

    // `/api/generate` has never been behind auth, so there is no credential to
    // attribute the request to — the log says exactly that instead of guessing.
    match generate_blocking(
        state,
        "/api/generate",
        crate::reqlog::Caller::anonymous(),
        model,
        req.prompt,
        DEFAULT_MAX_TOKENS,
    )
    .await
    {
        Ok((text, summary)) => (
            StatusCode::OK,
            Json(GenerateResponse {
                response: text,
                source: summary.source,
                tokens_generated: summary.tokens_generated,
                tokens_per_sec: summary.tokens_per_sec,
                duration_ms: summary.duration_ms,
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(GenerateResponse {
                response: format!(
                    "Inference failed. Error: {e}. Start peers with --inference-port."
                ),
                source: "none".to_string(),
                tokens_generated: 0,
                tokens_per_sec: 0.0,
                duration_ms: 0,
            }),
        )
            .into_response(),
    }
}

async fn handle_peers(State(state): State<GatewayState>) -> Json<PeersResponse> {
    let peers = state.all_peers().await;

    let mut summaries = Vec::new();
    for p in &peers {
        let load = state.loads.load(&p.node_id).await;
        summaries.push(PeerSummary {
            node_id: p.node_id[..p.node_id.len().min(12)].to_string(),
            addr: p.addr.clone(),
            model: p.model.clone(),
            layers: p.layer_range(),
            ollama_url: p.ollama_url(),
            inference_url: p.inference_url(),
            load,
        });
    }
    let count = summaries.len();
    Json(PeersResponse { peers: summaries, count })
}

async fn handle_health(State(state): State<GatewayState>) -> Json<HealthResponse> {
    let ollama_available = state.ollama.is_available().await;
    let peers = state.all_peers().await;

    let peer_count = peers.len();
    let pipeline_peers = peers.iter().filter(|p| p.inference_port.is_some()).count();
    Json(HealthResponse {
        status: "ok",
        ollama_available,
        peer_count,
        pipeline_peers,
        default_model: state.active_model().await,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_pessimistic() {
        // 3 chars per token errs high, which is what a crash guard needs.
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens(&"a".repeat(3000)), 1000);
    }

    #[test]
    fn token_estimate_counts_characters_not_bytes() {
        // Accented Portuguese must not inflate the count through UTF-8 bytes.
        assert_eq!(estimate_tokens("ção"), 1);
    }

    #[test]
    fn rate_is_zero_for_an_instant_generation() {
        assert_eq!(rate(10, 0.0), 0.0);
        assert!((rate(10, 2.0) - 5.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::runtime::NodeConfig;

    fn cfg(n_ctx: u32, n_batch: u32, max_new: u32, max_sessions: u32) -> NodeConfig {
        NodeConfig {
            model_path: String::new(),
            model_label: String::new(),
            start_layer: 0,
            end_layer: 0,
            total_layers: 0,
            peer_count: 1,
            peer_index: 0,
            primary: true,
            inference_port: 8181,
            n_gpu_layers: 99,
            n_ctx,
            n_batch,
            max_sessions,
            prefill_chunk_tokens: crate::prefill::DEFAULT_CHUNK_TOKENS,
            max_new_tokens: max_new,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.15,
            repeat_last_n: 64,
            no_thinking: true,
            single_peer: false,
            no_embeddings: false,
        }
    }

    /// The whole point of the fix: one session means the configured context is
    /// the window, instead of an eighth of it.
    #[test]
    fn a_single_session_gets_the_whole_context() {
        let c = cfg(262_144, 262_144, 1024, 1);
        assert_eq!(prompt_budget(&c), 262_144 - 1024);
    }

    #[test]
    fn concurrency_divides_the_window() {
        let c = cfg(262_144, 262_144, 1024, 8);
        assert_eq!(prompt_budget(&c), 262_144 / 8 - 1024);
    }

    /// The bug a harness hits on its first real conversation: with the prompt
    /// travelling in chunks, a small `n_batch` must not cap the prompt. The Pi
    /// sent ~23k tokens into a 32k window and was refused against a limit of
    /// 512, which was `n_batch`.
    #[test]
    fn n_batch_no_longer_caps_the_prompt() {
        let c = cfg(32_768, 512, 1024, 1);
        assert_eq!(prompt_budget(&c), 32_768 - 1024);
    }

    /// `n_batch` did not stop mattering — it moved to the chunk, which is what
    /// llama.cpp actually decodes in one go.
    #[test]
    fn n_batch_caps_the_chunk_instead() {
        let c = cfg(32_768, 512, 1024, 1);
        assert_eq!(prompt_chunk_tokens(&c), 512);

        let wide = cfg(32_768, 4096, 1024, 1);
        assert_eq!(prompt_chunk_tokens(&wide), crate::prefill::DEFAULT_CHUNK_TOKENS);
    }

    #[test]
    fn zero_sessions_does_not_divide_by_zero() {
        let c = cfg(8192, 8192, 256, 0);
        assert_eq!(prompt_budget(&c), 8192 - 256);
    }
}
