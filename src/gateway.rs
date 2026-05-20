use crate::model::LayerAssignment;
use crate::network::{Network, PeerInfo};
use crate::ollama::OllamaClient;
use crate::pipeline::{plan_pipeline, LoadTracker};
use anyhow::Result;
use std::time::Instant;
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
use tracing::{info, warn};

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
    /// Serializes pipeline executions: inference server nodes hold shared KV cache
    /// state across tokens, so only one conversation can run at a time.
    pub pipeline_sem: Arc<tokio::sync::Semaphore>,
}

// ── Peer-to-peer pipeline protocol ───────────────────────────────────────────

/// Sent to each peer's /v1/infer endpoint.
#[derive(Debug, Serialize)]
struct PeerInferRequest<'a> {
    model: &'a str,
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

// ── Server startup ────────────────────────────────────────────────────────────

pub async fn start_gateway(state: GatewayState, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/api/generate", post(handle_generate))
        .route("/generate", post(handle_generate)) // alias
        .route("/peers", get(handle_peers))
        .route("/health", get(handle_health))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!(
        "🌐 Gateway HTTP on http://localhost:{port}\
         — POST /api/generate  GET /peers  GET /health"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

// ── Pipeline execution ────────────────────────────────────────────────────────

#[derive(Debug)]
struct StepResult {
    text: String,
    token_id: u32,
    done: bool,
}

/// Execute a single forward pass through the pipeline.
async fn execute_pipeline_step(
    plan: &crate::pipeline::PipelinePlan,
    model: &str,
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

// ── Route handlers ────────────────────────────────────────────────────────────

/// Accept raw bytes so callers don't need `Content-Type: application/json`.
async fn handle_generate(
    State(state): State<GatewayState>,
    body: Bytes,
) -> impl IntoResponse {
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

    let model = req
        .model
        .as_deref()
        .unwrap_or(&state.default_model)
        .to_string();

    info!("📨 /api/generate model={} prompt_len={}", model, req.prompt.len());

    let mut peers = state.network.alive_peers().await;
    
    // Include local node if it has an assignment and inference port
    if let Some(local_asgn) = state.local_assignment.read().await.clone() {
        if local_asgn.model == model {
            peers.push(PeerInfo {
                node_id: state.network.node_id.clone(),
                pubkey: state.network.pubkey.clone(),
                addr: format!("127.0.0.1:{}", state.network.bind_addr.port()),
                model: local_asgn.model,
                start_layer: local_asgn.start_layer,
                end_layer: local_asgn.end_layer,
                total_layers: local_asgn.total_layers,
                memory_mb: local_asgn.memory_used_mb,
                last_seen: chrono::Utc::now(),
                ollama_port: state.network.ollama_port,
                inference_port: state.network.inference_port,
            });
        }
    }

    // Find total layers for THIS model from peers that have it
    let total_layers = peers.iter()
        .find(|p| p.model == model)
        .map(|p| p.total_layers)
        .unwrap_or(32);

    // ── 1. Try distributed pipeline (peers with inference_port) ──────────────
    // Acquire the pipeline semaphore for the ENTIRE generation loop.
    // Inference server nodes share KV cache state across per-token calls, so
    // concurrent conversations would corrupt each other's sequence state.
    let _pipeline_permit = state.pipeline_sem.acquire().await;

    let mut pipeline_error = None;
    if let Some(plan) = plan_pipeline(&peers, &model, total_layers, &state.loads).await {
        info!(
            "🔀 Pipeline planned: {} hops [{}]",
            plan.steps.len(),
            plan.hop_chain()
        );

        let gen_start = Instant::now();
        let mut full_text = String::new();
        let mut tokens_generated: u32 = 0;
        let mut current_prompt: Option<&str> = Some(&req.prompt);
        let mut current_tokens: Option<Vec<u32>> = None;
        let mut reset = true;
        let max_tokens = 512;

        for i in 0..max_tokens {
            let res = execute_pipeline_step(
                &plan,
                &model,
                current_prompt,
                current_tokens.as_deref(),
                reset,
                1.0,
                &state.http,
                &state.loads
            ).await;

            match res {
                Ok(step) => {
                    full_text.push_str(&step.text);
                    tokens_generated += 1;
                    if step.done || i == max_tokens - 1 {
                        let elapsed = gen_start.elapsed();
                        let duration_ms = elapsed.as_millis() as u64;
                        let tokens_per_sec = if elapsed.as_secs_f64() > 0.0 {
                            tokens_generated as f64 / elapsed.as_secs_f64()
                        } else {
                            0.0
                        };
                        info!("✅ Pipeline complete: {} tokens in {:.1}s ({:.1} tok/s)",
                            tokens_generated, elapsed.as_secs_f64(), tokens_per_sec);
                        return (
                            StatusCode::OK,
                            Json(GenerateResponse {
                                response: full_text,
                                source: format!("pipeline:{}", plan.hop_chain()),
                                tokens_generated,
                                tokens_per_sec,
                                duration_ms,
                            }),
                        )
                            .into_response();
                    }
                    current_prompt = None;
                    current_tokens = Some(vec![step.token_id]);
                    reset = false;
                }
                Err(e) => {
                    warn!("Pipeline step {} error: {}", i, e);
                    if !full_text.is_empty() {
                        let elapsed = gen_start.elapsed();
                        let duration_ms = elapsed.as_millis() as u64;
                        let tokens_per_sec = if elapsed.as_secs_f64() > 0.0 {
                            tokens_generated as f64 / elapsed.as_secs_f64()
                        } else {
                            0.0
                        };
                        info!("Returning {} tokens of partial output", tokens_generated);
                        return (
                            StatusCode::OK,
                            Json(GenerateResponse {
                                response: full_text,
                                source: format!("pipeline:{} (partial)", plan.hop_chain()),
                                tokens_generated,
                                tokens_per_sec,
                                duration_ms,
                            }),
                        ).into_response();
                    }
                    pipeline_error = Some(format!("Pipeline failed at step {}: {}", i, e));
                    break;
                }
            }
        }
    } else {
        pipeline_error = Some(format!("No valid pipeline plan for model '{}' (check layer coverage)", model));
    }

    // ── 2. Fallback: local Ollama ─────────────────────────────────────────────
    // ... (commented out by user)

    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(GenerateResponse {
            response: format!(
                "Inference failed. Error: {}. Start peers with --inference-port.",
                pipeline_error.unwrap_or_else(|| "Unknown error".to_string())
            ),
            source: "none".to_string(),
            tokens_generated: 0,
            tokens_per_sec: 0.0,
            duration_ms: 0,
        }),
    )
        .into_response()
}

async fn handle_peers(State(state): State<GatewayState>) -> Json<PeersResponse> {
    let mut peers = state.network.alive_peers().await;
    
    // Include local node
    if let Some(local_asgn) = state.local_assignment.read().await.clone() {
        peers.push(PeerInfo {
            node_id: state.network.node_id.clone(),
            pubkey: state.network.pubkey.clone(),
            addr: format!("127.0.0.1:{}", state.network.bind_addr.port()),
            model: local_asgn.model,
            start_layer: local_asgn.start_layer,
            end_layer: local_asgn.end_layer,
            total_layers: local_asgn.total_layers,
            memory_mb: local_asgn.memory_used_mb,
            last_seen: chrono::Utc::now(),
            ollama_port: state.network.ollama_port,
            inference_port: state.network.inference_port,
        });
    }

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
    let mut peers = state.network.alive_peers().await;
    
    // Include local node
    if let Some(local_asgn) = state.local_assignment.read().await.clone() {
        peers.push(PeerInfo {
            node_id: state.network.node_id.clone(),
            pubkey: state.network.pubkey.clone(),
            addr: format!("127.0.0.1:{}", state.network.bind_addr.port()),
            model: local_asgn.model,
            start_layer: local_asgn.start_layer,
            end_layer: local_asgn.end_layer,
            total_layers: local_asgn.total_layers,
            memory_mb: local_asgn.memory_used_mb,
            last_seen: chrono::Utc::now(),
            ollama_port: state.network.ollama_port,
            inference_port: state.network.inference_port,
        });
    }

    let peer_count = peers.len();
    let pipeline_peers = peers.iter().filter(|p| p.inference_port.is_some()).count();
    Json(HealthResponse {
        status: "ok",
        ollama_available,
        peer_count,
        pipeline_peers,
        default_model: state.default_model.clone(),
    })
}
