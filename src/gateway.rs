use crate::model::LayerAssignment;
use crate::network::{Network, PeerInfo};
use crate::ollama::OllamaClient;
use crate::pipeline::{plan_pipeline, LoadTracker};
use crate::runtime::NodeRuntime;
use anyhow::Result;
use std::time::Instant;
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

/// `_MAX_SESSIONS` in `inference_server.py`: the context is divided into this
/// many concurrent session slots, so one request only gets `n_ctx / 8` tokens.
const INFERENCE_SESSION_SLOTS: u32 = 8;

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
}

impl GatewayState {
    /// Model label the pipeline planner matches on right now.
    pub async fn active_model(&self) -> String {
        self.runtime.model_label(&self.default_model).await
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
        let cfg = self.runtime.config().await?;
        let per_session_ctx = cfg.n_ctx / INFERENCE_SESSION_SLOTS.max(1);
        // Room has to be left for the tokens we are about to generate.
        let usable_ctx = per_session_ctx.saturating_sub(cfg.max_new_tokens);
        Some(cfg.n_batch.min(usable_ctx))
    }

    /// Alive peers plus this node, so the local assignment participates in
    /// planning exactly like a remote one.
    pub async fn all_peers(&self) -> Vec<PeerInfo> {
        let mut peers = self.network.alive_peers().await;
        if let Some(local) = self.local_assignment.read().await.clone() {
            peers.push(PeerInfo {
                node_id: self.network.node_id.clone(),
                pubkey: self.network.pubkey.clone(),
                addr: format!("127.0.0.1:{}", self.network.bind_addr.port()),
                model: local.model,
                start_layer: local.start_layer,
                end_layer: local.end_layer,
                total_layers: local.total_layers,
                memory_mb: local.memory_used_mb,
                last_seen: chrono::Utc::now(),
                ollama_port: self.network.ollama_port,
                inference_port: self.network.inference_port,
                gateway_port: Some(self.gateway_port),
                role: String::new(),
                hostname: String::new(),
                connection_verified: false,
                disk_total_mb: 0,
                disk_free_mb: 0,
                api_keys_active: 0,
            });
        }
        peers
    }
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
    if let Some(budget) = state.prompt_token_budget().await {
        let estimated = estimate_tokens(&prompt);
        if estimated > budget {
            let _ = tx
                .send(GenEvent::Failed(format!(
                    "Prompt grande demais: ~{estimated} tokens estimados contra um limite de \
                     {budget}. O limite é o menor entre --n-batch e (--n-ctx / {INFERENCE_SESSION_SLOTS} \
                     sessões) menos --max-new-tokens. Suba --n-ctx e --n-batch nos peers, ou \
                     encurte o prompt."
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
    let mut current_prompt: Option<&str> = Some(&prompt);
    let mut current_tokens: Option<Vec<u32>> = None;
    let mut reset = true;
    let max_tokens = max_tokens.max(1);

    for i in 0..max_tokens {
        let res = execute_pipeline_step(
            &plan,
            &model,
            &session_id,
            current_prompt,
            current_tokens.as_deref(),
            reset,
            1.0,
            &state.http,
            &state.loads,
        )
        .await;

        match res {
            Ok(step) => {
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

                current_prompt = None;
                current_tokens = Some(vec![step.token_id]);
                reset = false;
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

/// Run a generation to completion and return the whole text.
pub async fn generate_blocking(
    state: GatewayState,
    model: String,
    prompt: String,
    max_tokens: u32,
) -> std::result::Result<(String, GenSummary), String> {
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(generate_stream(state, model, prompt, max_tokens, tx));

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
    let app = Router::new()
        .route("/api/generate", post(handle_generate))
        .route("/generate", post(handle_generate)) // alias
        .route("/peers", get(handle_peers))
        .route("/health", get(handle_health))
        .merge(crate::openai::routes())
        .merge(crate::webui::routes())
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

    match generate_blocking(state, model, req.prompt, DEFAULT_MAX_TOKENS).await {
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
