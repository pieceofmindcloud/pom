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

pub fn admin_routes() -> Router<crate::gateway::GatewayState> {
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
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> impl IntoResponse {
    let offers = state.transfers.offers.read().await.clone();
    Json(json!({ "offers": offers }))
}

/// Every node this machine knows about: the peers actually on the network plus
/// any blocked node that is not currently connected. Returning only the blocked
/// list left the "Nós conectados" panel permanently empty.
async fn list_nodes(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> impl IntoResponse {
    let blocked = state.auth.read().await.blocked_nodes.clone();
    let self_id = state.network.node_id.clone();

    let mut nodes: Vec<serde_json::Value> = state
        .all_peers()
        .await
        .iter()
        .map(|p| {
            json!({
                "node_id": p.node_id,
                "short_id": &p.node_id[..p.node_id.len().min(12)],
                "hostname": p.hostname,
                "addr": p.addr,
                "role": p.role,
                "model": p.model,
                "start_layer": p.start_layer,
                "end_layer": p.end_layer,
                "total_layers": p.total_layers,
                "is_self": p.node_id == self_id,
                "connected": true,
                "blocked": blocked.contains(&p.node_id),
                "verified": p.connection_verified,
            })
        })
        .collect();

    let connected: Vec<String> = nodes
        .iter()
        .filter_map(|n| n["node_id"].as_str().map(str::to_string))
        .collect();
    for id in blocked.iter().filter(|id| !connected.contains(id)) {
        nodes.push(json!({
            "node_id": id,
            "short_id": &id[..id.len().min(12)],
            "hostname": "",
            "addr": "",
            "is_self": false,
            "connected": false,
            "blocked": true,
            "verified": false,
        }));
    }

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
    /// Node this token is meant for. Empty means the token belongs to the
    /// cluster as a whole rather than to one machine.
    #[serde(default)]
    node_id: Option<String>,
}

/// Mint a token.
///
/// Tokens accumulate rather than replace each other: the operator hands one out
/// per node (and per harness), and the request log attributes each generation
/// back to the one that paid for it. Minting used to deactivate every existing
/// key, which made "a token per node" impossible — the second one silently
/// revoked the first.
async fn generate_api_key(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Json(req): Json<GenerateKeyRequest>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    let raw = format!("sk-{}", Uuid::new_v4());
    let key = ApiKey {
        key: raw,
        label: req.label.unwrap_or_else(|| "default".into()),
        node_id: req.node_id.unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::TransferOffer;

    /// A rota `/admin` devolve as mesmas ofertas de `GET /api/transfer/offers`,
    /// não uma lista vazia fixa: um consumidor da rota de administração tem que
    /// enxergar o que está esperando resposta.
    #[tokio::test]
    async fn transfer_offers_returns_the_real_offers() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;

        state
            .transfers
            .offers
            .write()
            .await
            .push(TransferOffer {
                id: "oferta-1".into(),
                from_node_id: "peer-b".into(),
                from_host: "ksia".into(),
                from_gateway: "http://127.0.0.1:8080".into(),
                remote_path: "/models/fatia_0_15.gguf".into(),
                grant: None,
                name: "fatia_0_15.gguf".into(),
                size_bytes: 4_096,
                md5: String::new(),
                start_layer: Some(0),
                end_layer: Some(15),
            });

        let response = transfer_offers(ConnectionAuth, State(state))
            .await
            .into_response();
        let body = crate::gateway::testkit::body_of(response).await;

        let offers = body["offers"].as_array().expect("campo offers");
        assert_eq!(offers.len(), 1, "a rota /admin não pode devolver vazio");
        assert_eq!(offers[0]["name"], "fatia_0_15.gguf");
        assert_eq!(offers[0]["from_host"], "ksia");
    }
}
