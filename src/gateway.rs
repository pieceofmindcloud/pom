use crate::network::Network;
use crate::ollama::OllamaClient;
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

#[derive(Clone)]
pub struct GatewayState {
    pub network: Arc<Network>,
    pub ollama: Arc<OllamaClient>,
    /// Default model for this gateway node (used when request omits `model`).
    pub default_model: String,
}

#[derive(Debug, Deserialize)]
pub struct GenerateRequest {
    /// Ollama model name. Falls back to the node's default model if omitted.
    pub model: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Serialize)]
pub struct GenerateResponse {
    pub response: String,
    /// "local" or the peer's node_id (first 12 chars)
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
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub ollama_available: bool,
    pub peer_count: usize,
    pub default_model: String,
}

pub async fn start_gateway(state: GatewayState, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/generate", post(handle_generate))
        .route("/peers", get(handle_peers))
        .route("/health", get(handle_health))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!(
        "🌐 Gateway HTTP server on http://localhost:{port}  \
        — POST /generate  GET /peers  GET /health"
    );
    axum::serve(listener, app).await?;
    Ok(())
}

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
                .into_response()
        }
    };

    let model = req
        .model
        .as_deref()
        .unwrap_or(&state.default_model)
        .to_string();

    info!("📨 /generate model={} prompt_len={}", model, req.prompt.len());

    // 1. Try local Ollama first
    if state.ollama.is_available().await {
        match state.ollama.generate(&model, &req.prompt).await {
            Ok(resp) => {
                info!("✅ Served locally");
                return (
                    StatusCode::OK,
                    Json(GenerateResponse {
                        response: resp.response,
                        source: "local".to_string(),
                    }),
                )
                    .into_response();
            }
            Err(e) => warn!("Local Ollama failed ({}), trying peers…", e),
        }
    }

    // 2. Try peers that advertise an Ollama port and serve the same model
    let peers = state.network.alive_peers().await;
    for peer in &peers {
        if peer.model != model {
            continue;
        }
        if let Some(url) = peer.ollama_url() {
            let remote = OllamaClient::new(&url);
            match remote.generate(&model, &req.prompt).await {
                Ok(resp) => {
                    info!("✅ Served by peer {}", &peer.node_id[..12]);
                    return (
                        StatusCode::OK,
                        Json(GenerateResponse {
                            response: resp.response,
                            source: peer.node_id[..12].to_string(),
                        }),
                    )
                        .into_response();
                }
                Err(e) => warn!("Peer {} failed: {}", &peer.node_id[..12], e),
            }
        }
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(GenerateResponse {
            response: format!(
                "No Ollama instance available for model '{}'. \
                 Run a node with --ollama-url and ensure the model is pulled.",
                model
            ),
            source: "none".to_string(),
        }),
    )
        .into_response()
}

async fn handle_peers(State(state): State<GatewayState>) -> Json<PeersResponse> {
    let peers = state.network.alive_peers().await;
    let summaries: Vec<PeerSummary> = peers
        .iter()
        .map(|p| PeerSummary {
            node_id: p.node_id[..12.min(p.node_id.len())].to_string(),
            addr: p.addr.clone(),
            model: p.model.clone(),
            layers: p.layer_range(),
            ollama_url: p.ollama_url(),
        })
        .collect();
    let count = summaries.len();
    Json(PeersResponse { peers: summaries, count })
}

async fn handle_health(State(state): State<GatewayState>) -> Json<HealthResponse> {
    let ollama_available = state.ollama.is_available().await;
    let peer_count = state.network.peer_count().await;
    Json(HealthResponse {
        status: "ok",
        ollama_available,
        peer_count,
        default_model: state.default_model.clone(),
    })
}
