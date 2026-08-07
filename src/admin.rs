use crate::auth::{AuthState, ConnectionAuth};
use crate::config::ApiKey;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub fn admin_routes() -> Router<AuthState> {
    Router::new()
        .route("/admin/nodes", get(list_nodes))
        .route("/admin/nodes/{id}/block", post(block_node))
        .route("/admin/nodes/{id}/unblock", post(unblock_node))
        .route("/admin/nodes/{id}", delete(remove_node))
        .route("/admin/regenerate-key", post(regenerate_key))
        .route("/admin/api-keys", get(list_api_keys).post(generate_api_key))
        .route("/admin/api-keys/{key}", delete(revoke_api_key))
        .route("/admin/transfer-offers", get(transfer_offers))
}

async fn transfer_offers(
    State(_config): State<AuthState>,
) -> impl IntoResponse {
    Json(json!({ "offers": [] }))
}

async fn list_nodes(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
) -> impl IntoResponse {
    let cfg = config.read().await;
    let nodes: Vec<serde_json::Value> = cfg
        .blocked_nodes
        .iter()
        .map(|id| {
            json!({
                "node_id": id,
                "blocked": true,
                "verified": false,
            })
        })
        .collect();
    Json(nodes)
}

async fn block_node(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    if !cfg.blocked_nodes.contains(&node_id) {
        cfg.blocked_nodes.push(node_id.clone());
    }
    let _ = cfg.save();
    Json(json!({ "blocked": true, "node_id": node_id }))
}

async fn unblock_node(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    cfg.blocked_nodes.retain(|id| id != &node_id);
    let _ = cfg.save();
    Json(json!({ "blocked": false, "node_id": node_id }))
}

async fn remove_node(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    cfg.blocked_nodes.retain(|id| id != &node_id);
    let _ = cfg.save();
    Json(json!({ "removed": true, "node_id": node_id }))
}

async fn regenerate_key(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    let raw = Uuid::new_v4().to_string();
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    cfg.connection_key = hex::encode(hasher.finalize());
    let _ = cfg.save();
    Json(json!({ "connection_key": cfg.connection_key }))
}

async fn list_api_keys(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
) -> impl IntoResponse {
    let cfg = config.read().await;
    Json(cfg.api_keys.clone())
}

#[derive(Debug, Deserialize)]
struct GenerateKeyRequest {
    label: Option<String>,
}

async fn generate_api_key(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Json(req): Json<GenerateKeyRequest>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    let node_id = "principal".to_string();
    for k in &mut cfg.api_keys {
        k.active = false;
    }
    let raw = format!("sk-{}", Uuid::new_v4());
    let key = ApiKey {
        key: raw,
        label: req.label.unwrap_or_else(|| "default".into()),
        node_id,
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    cfg.api_keys.push(key.clone());
    let _ = cfg.save();
    (StatusCode::CREATED, Json(key))
}

async fn revoke_api_key(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    let existed = cfg.api_keys.iter().any(|k| k.key == key);
    cfg.api_keys.retain(|k| k.key != key);
    let _ = cfg.save();
    Json(json!({ "revoked": existed }))
}
