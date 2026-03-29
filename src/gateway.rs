use crate::network::Network;
use crate::ollama::OllamaClient;
use crate::pipeline::{plan_pipeline, LoadTracker};
use anyhow::Result;
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
    /// Tracks in-flight inference requests per peer (for pipeline load balancing).
    pub loads: Arc<LoadTracker>,
    /// Shared HTTP client with long timeout for pipeline calls.
    pub http: Arc<reqwest::Client>,
}

// ── Peer-to-peer pipeline protocol ───────────────────────────────────────────

/// Sent to each peer's /v1/infer endpoint.
#[derive(Debug, Serialize)]
struct PeerInferRequest<'a> {
    model: &'a str,
    /// Raw prompt text — only set for the FIRST peer in the pipeline.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<&'a str>,
    /// Base64-encoded float32 hidden-state tensor — set for all peers except the first.
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden_state: Option<&'a str>,
    /// Shape [n_tokens, hidden_dim] of hidden_state.
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden_shape: Option<[usize; 2]>,
    start_layer: u32,
    end_layer: u32,
    /// True only for the last peer in the pipeline — it should decode and return text.
    is_last: bool,
}

/// Response from each peer's /v1/infer endpoint.
#[derive(Debug, Deserialize)]
struct PeerInferResponse {
    /// Populated by non-last peers.
    hidden_state: Option<String>,
    hidden_shape: Option<[usize; 2]>,
    /// Populated by the last peer.
    text: Option<String>,
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

/// Execute a planned pipeline: call each peer in sequence, threading the
/// hidden-state tensor through until the last peer returns decoded text.
async fn execute_pipeline(
    plan: &crate::pipeline::PipelinePlan,
    model: &str,
    prompt: &str,
    http: &reqwest::Client,
    loads: &LoadTracker,
) -> Result<String> {
    let n = plan.steps.len();
    // hidden_state and its shape are passed between peers
    let mut hidden_state: Option<String> = None;
    let mut hidden_shape: Option<[usize; 2]> = None;

    for (i, step) in plan.steps.iter().enumerate() {
        let is_first = i == 0;
        let is_last = i == n - 1;

        // Acquire load slot — dropped at end of iteration (or on error)
        let _guard = loads.acquire(&step.node_id).await;

        let req = PeerInferRequest {
            model,
            prompt: if is_first { Some(prompt) } else { None },
            hidden_state: hidden_state.as_deref(),
            hidden_shape,
            start_layer: step.start_layer,
            end_layer: step.end_layer,
            is_last,
        };

        let url = format!("{}/v1/infer", step.inference_url);
        info!(
            "  → peer {} layers {}-{} ({})",
            &step.node_id[..step.node_id.len().min(8)],
            step.start_layer,
            step.end_layer,
            if is_last { "final" } else { "hidden" }
        );

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
            return peer_resp
                .text
                .ok_or_else(|| anyhow::anyhow!("last peer returned no text"));
        }

        hidden_state = peer_resp.hidden_state;
        hidden_shape = peer_resp.hidden_shape;

        if hidden_state.is_none() {
            return Err(anyhow::anyhow!(
                "peer {} (layers {}-{}) returned no hidden_state",
                &step.node_id[..8],
                step.start_layer,
                step.end_layer,
            ));
        }
    }

    Err(anyhow::anyhow!("pipeline completed without a final text response"))
}

// ── Route handlers ────────────────────────────────────────────────────────────

/// Accept raw bytes so callers don't need `Content-Type: application/json`.
/// `curl -d '{"prompt":"hi"}'` works without any extra headers.
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

    let peers = state.network.alive_peers().await;
    let total_layers = peers.first().map(|p| p.total_layers).unwrap_or(32);

    // ── 1. Try distributed pipeline (peers with inference_port) ──────────────
    if let Some(plan) = plan_pipeline(&peers, total_layers, &state.loads).await {
        info!(
            "🔀 Pipeline planned: {} hops [{}]",
            plan.steps.len(),
            plan.hop_chain()
        );
        match execute_pipeline(&plan, &model, &req.prompt, &state.http, &state.loads).await {
            Ok(text) => {
                info!("✅ Pipeline complete via {}", plan.hop_chain());
                return (
                    StatusCode::OK,
                    Json(GenerateResponse {
                        response: text,
                        source: format!("pipeline:{}", plan.hop_chain()),
                    }),
                )
                    .into_response();
            }
            Err(e) => warn!("Pipeline failed ({}), falling back to Ollama", e),
        }
    }

    // ── 2. Fallback: local Ollama ─────────────────────────────────────────────
    if state.ollama.is_available().await {
        match state.ollama.generate(&model, &req.prompt).await {
            Ok(resp) => {
                info!("✅ Served locally via Ollama");
                return (
                    StatusCode::OK,
                    Json(GenerateResponse {
                        response: resp.response,
                        source: "local".to_string(),
                    }),
                )
                    .into_response();
            }
            Err(e) => warn!("Local Ollama failed: {}", e),
        }
    }

    // ── 3. Fallback: any peer Ollama ──────────────────────────────────────────
    for peer in peers.iter().filter(|p| p.model == model) {
        if let Some(url) = peer.ollama_url() {
            let remote = OllamaClient::new(&url);
            match remote.generate(&model, &req.prompt).await {
                Ok(resp) => {
                    info!("✅ Served by peer Ollama {}", &peer.node_id[..8]);
                    return (
                        StatusCode::OK,
                        Json(GenerateResponse {
                            response: resp.response,
                            source: format!("peer:{}", &peer.node_id[..8]),
                        }),
                    )
                        .into_response();
                }
                Err(e) => warn!("Peer Ollama {} failed: {}", &peer.node_id[..8], e),
            }
        }
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(GenerateResponse {
            response: format!(
                "No inference backend available for model '{}'. \
                 Start peers with --inference-port or --ollama-url.",
                model
            ),
            source: "none".to_string(),
        }),
    )
        .into_response()
}

async fn handle_peers(State(state): State<GatewayState>) -> Json<PeersResponse> {
    let peers = state.network.alive_peers().await;
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
    let peers = state.network.alive_peers().await;
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
