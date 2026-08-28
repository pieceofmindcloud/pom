use crate::auth::AuthState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct HandshakeRequest {
    pub node_id: String,
    pub pubkey: String,
    pub connection_key: String,
}

/// O corpo que o handshake devolve. Foi um `json!` solto por engano, e um
/// tipo que ninguém constrói vira documentação que mente: o `skip` do `None`
/// é o que faz a recusa sair sem `peer_id` e o aceite sem `reason`.
#[derive(Debug, Serialize)]
pub struct HandshakeResponse {
    pub approved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// O `node_id` declarado é mesmo o que sai desta `pubkey`?
///
/// `node_id` é o nome do peer no cluster e a chave de `blocked_nodes`, e é
/// definido como `SHA256(pubkey)` (`identity::NodeId::from_pubkey`). Antes o
/// campo `pubkey` era lido e jogado fora, e qualquer requisição com a chave de
/// conexão podia declarar o `node_id` que quisesse - inclusive o de outra
/// máquina, para se passar por ela. Amarrar os dois campos custa uma hash e
/// fecha essa porta.
///
/// O que isto **não** prova é posse da chave privada: para isso o handshake
/// precisaria de um desafio assinado, que é mudança de protocolo.
fn node_id_matches_pubkey(node_id: &str, pubkey_hex: &str) -> bool {
    use ed25519_dalek::VerifyingKey;
    use sha2::{Digest, Sha256};

    let Ok(bytes) = hex::decode(pubkey_hex) else {
        return false;
    };
    let Ok(bytes): Result<[u8; 32], _> = bytes.try_into() else {
        return false;
    };
    // Uma chave ed25519 malformada não deriva identidade nenhuma.
    if VerifyingKey::from_bytes(&bytes).is_err() {
        return false;
    }
    hex::encode(Sha256::digest(bytes)).eq_ignore_ascii_case(node_id)
}

pub async fn handle_handshake(
    State(config): State<AuthState>,
    Json(req): Json<HandshakeRequest>,
) -> impl IntoResponse {
    let cfg = config.read().await;

    if req.connection_key != cfg.connection_key {
        return (
            StatusCode::FORBIDDEN,
            Json(HandshakeResponse {
                approved: false,
                peer_id: None,
                reason: Some("invalid_key".into()),
            }),
        );
    }

    // Antes de olhar a lista de bloqueio: um `node_id` que não sai desta
    // `pubkey` faria o bloqueio recair sobre um nome que ninguém pode provar.
    if !node_id_matches_pubkey(&req.node_id, &req.pubkey) {
        return (
            StatusCode::FORBIDDEN,
            Json(HandshakeResponse {
                approved: false,
                peer_id: None,
                reason: Some("identity_mismatch".into()),
            }),
        );
    }

    if cfg.blocked_nodes.contains(&req.node_id) {
        return (
            StatusCode::FORBIDDEN,
            Json(HandshakeResponse {
                approved: false,
                peer_id: None,
                reason: Some("blocked".into()),
            }),
        );
    }

    (
        StatusCode::OK,
        Json(HandshakeResponse {
            approved: true,
            peer_id: Some(req.node_id),
            reason: None,
        }),
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
            tool_call_grace_tokens: crate::config::DEFAULT_TOOL_CALL_GRACE_TOKENS,
            gpus: None,
            thermal: Default::default(),
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            role: Role::Principal,
            tls_required: false,
            node_name: None,
            huggingface_token: None,
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
            shard_max_gb: crate::storage::DEFAULT_SHARD_MAX_GB,
            max_output_tokens: crate::config::DEFAULT_MAX_OUTPUT_TOKENS,
        }))
    }

    fn make_request(key: &str) -> HandshakeRequest {
        let (node_id, pubkey) = identidade_de_verdade();
        HandshakeRequest {
            node_id,
            pubkey,
            connection_key: key.into(),
        }
    }

    /// Um par (`node_id`, `pubkey`) gerado como um nó real gera o seu.
    fn identidade_de_verdade() -> (String, String) {
        let id = crate::identity::Identity::generate();
        (id.node_id.0.clone(), id.public_key_hex())
    }

    async fn responder(state: AuthState, req: HandshakeRequest) -> (StatusCode, serde_json::Value) {
        use axum::body::to_bytes;
        let resp = handle_handshake(State(state), Json(req))
            .await
            .into_response();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// O handshake lia `pubkey` e nunca a conferia: com a chave de conexão em
    /// mãos, qualquer requisição podia declarar o `node_id` de outra máquina -
    /// e `node_id` é o nome do peer no cluster e a chave de `blocked_nodes`.
    /// Agora o `node_id` tem que ser o `SHA256` desta `pubkey`.
    #[tokio::test]
    async fn a_pubkey_que_nao_deriva_o_node_id_e_recusada() {
        let state = make_state("secret", vec![]);
        let (_, pubkey) = identidade_de_verdade();
        let (outro_id, _) = identidade_de_verdade();

        let (status, body) = responder(
            state,
            HandshakeRequest {
                node_id: outro_id,
                pubkey,
                connection_key: "secret".into(),
            },
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "identity_mismatch");
    }

    #[tokio::test]
    async fn uma_pubkey_malformada_e_recusada() {
        let state = make_state("secret", vec![]);
        let (node_id, _) = identidade_de_verdade();

        let (status, _) = responder(
            state,
            HandshakeRequest {
                node_id,
                pubkey: "pubkey1".into(),
                connection_key: "secret".into(),
            },
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_identidade_coerente_passa() {
        let state = make_state("secret", vec![]);
        let req = make_request("secret");
        let esperado = req.node_id.clone();

        let (status, body) = responder(state, req).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["approved"], true);
        assert_eq!(body["peer_id"], esperado);
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
