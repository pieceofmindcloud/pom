use crate::auth::AuthState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct HandshakeRequest {
    pub node_id: String,
    pub pubkey: String,
    pub connection_key: String,
}

#[derive(Debug, Serialize)]
pub struct HandshakeResponse {
    pub approved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub async fn handle_handshake(
    State(config): State<AuthState>,
    Json(req): Json<HandshakeRequest>,
) -> impl IntoResponse {
    let cfg = config.read().await;

    if req.connection_key != cfg.connection_key {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "approved": false,
                "reason": "invalid_key"
            })),
        );
    }

    if cfg.blocked_nodes.contains(&req.node_id) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "approved": false,
                "reason": "blocked"
            })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "approved": true,
            "peer_id": req.node_id
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Role};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn make_state(key: &str, blocked: Vec<String>) -> AuthState {
        Arc::new(RwLock::new(Config {
            role: Role::Principal,
            node_name: None,
            connection_key: key.into(),
            gateway_port: 8080,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![],
            blocked_nodes: blocked,
            model_dirs: None,
            shard_dir: None,
        }))
    }

    fn make_request(key: &str) -> HandshakeRequest {
        HandshakeRequest {
            node_id: "node1".into(),
            pubkey: "pubkey1".into(),
            connection_key: key.into(),
        }
    }

    #[tokio::test]
    async fn test_handshake_valid_key() {
        let state = make_state("secret", vec![]);
        let req = make_request("secret");
        let cfg = state.read().await;
        assert_eq!(req.connection_key, cfg.connection_key);
        assert!(!cfg.blocked_nodes.contains(&req.node_id));
    }

    #[tokio::test]
    async fn test_handshake_wrong_key() {
        let state = make_state("secret", vec![]);
        let cfg = state.read().await;
        assert_ne!("wrong", cfg.connection_key);
    }

    #[tokio::test]
    async fn test_handshake_blocked_node() {
        let state = make_state("secret", vec!["blocked-node".into()]);
        let cfg = state.read().await;
        assert!(cfg.blocked_nodes.contains(&"blocked-node".to_string()));
    }
}
