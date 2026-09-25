use crate::auth::AuthState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use ed25519_dalek::{Signature, VerifyingKey};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

const HANDSHAKE_NONCE_BYTES: usize = 32;
const HANDSHAKE_CHALLENGE_TTL: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct PendingChallenge {
    verifier_id: usize,
    node_id: String,
    pubkey: [u8; 32],
    expires_at: Instant,
}

static CHALLENGES: OnceLock<Mutex<HashMap<[u8; HANDSHAKE_NONCE_BYTES], PendingChallenge>>> =
    OnceLock::new();

fn challenges() -> &'static Mutex<HashMap<[u8; HANDSHAKE_NONCE_BYTES], PendingChallenge>> {
    CHALLENGES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Debug, Deserialize)]
pub struct HandshakeRequest {
    pub node_id: String,
    pub pubkey: String,
    pub connection_key: String,
    pub nonce: Option<String>,
    pub signature: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

/// O `node_id` declarado é mesmo o que sai desta `pubkey`?
///
/// `node_id` é o nome do peer no cluster e a chave de `blocked_nodes`, e é
/// definido como `SHA256(pubkey)` (`identity::NodeId::from_pubkey`). Antes o
/// campo `pubkey` era lido e jogado fora, e qualquer requisição com a chave de
/// conexão podia declarar o `node_id` que quisesse - inclusive o de outra
/// máquina, para se passar por ela. Amarrar os dois campos custa uma hash e
/// fecha essa porta. O handshake também exige que o peer assine um nonce
/// gerado pelo verificador; só a chave privada correspondente pode concluir a
/// sessão.
///
fn pubkey_bytes(pubkey_hex: &str) -> Option<[u8; 32]> {
    let Ok(bytes) = hex::decode(pubkey_hex) else {
        return None;
    };
    let Ok(bytes): Result<[u8; 32], _> = bytes.try_into() else {
        return None;
    };
    // Uma chave ed25519 malformada não deriva identidade nenhuma.
    VerifyingKey::from_bytes(&bytes).ok()?;
    Some(bytes)
}

fn node_id_matches_pubkey(node_id: &str, pubkey_hex: &str) -> bool {
    let Some(bytes) = pubkey_bytes(pubkey_hex) else {
        return false;
    };
    hex::encode(Sha256::digest(bytes)).eq_ignore_ascii_case(node_id)
}

fn issue_challenge(verifier_id: usize, node_id: &str, pubkey: [u8; 32]) -> String {
    let now = Instant::now();
    let mut pending = challenges()
        .lock()
        .expect("handshake challenge lock poisoned");
    pending.retain(|_, challenge| challenge.expires_at > now);

    loop {
        let mut nonce = [0u8; HANDSHAKE_NONCE_BYTES];
        OsRng.fill_bytes(&mut nonce);
        if pending.contains_key(&nonce) {
            continue;
        }
        pending.insert(
            nonce,
            PendingChallenge {
                verifier_id,
                node_id: node_id.into(),
                pubkey,
                expires_at: now + HANDSHAKE_CHALLENGE_TTL,
            },
        );
        return hex::encode(nonce);
    }
}

fn take_challenge(
    verifier_id: usize,
    nonce_hex: &str,
) -> Result<([u8; HANDSHAKE_NONCE_BYTES], PendingChallenge), &'static str> {
    let Ok(bytes) = hex::decode(nonce_hex) else {
        return Err("invalid_nonce");
    };
    let Ok(nonce): Result<[u8; HANDSHAKE_NONCE_BYTES], _> = bytes.try_into() else {
        return Err("invalid_nonce");
    };

    let mut pending = challenges()
        .lock()
        .expect("handshake challenge lock poisoned");
    let Some(challenge) = pending.remove(&nonce) else {
        return Err("nonce_reused");
    };
    if challenge.expires_at <= Instant::now() {
        return Err("nonce_expired");
    }
    if challenge.verifier_id != verifier_id {
        return Err("challenge_mismatch");
    }
    Ok((nonce, challenge))
}

fn signature_matches(
    pubkey: [u8; 32],
    nonce: &[u8; HANDSHAKE_NONCE_BYTES],
    signature_hex: &str,
) -> bool {
    let Ok(signature_bytes) = hex::decode(signature_hex) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature_bytes) else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&pubkey) else {
        return false;
    };
    verifying_key.verify_strict(nonce, &signature).is_ok()
}

fn rejected(reason: &str) -> (StatusCode, Json<HandshakeResponse>) {
    (
        StatusCode::FORBIDDEN,
        Json(HandshakeResponse {
            approved: false,
            peer_id: None,
            reason: Some(reason.into()),
            nonce: None,
        }),
    )
}

pub async fn handle_handshake(
    State(config): State<AuthState>,
    Json(req): Json<HandshakeRequest>,
) -> impl IntoResponse {
    let verifier_id = std::sync::Arc::as_ptr(&config) as usize;
    let cfg = config.read().await;

    if cfg.connection_key.is_empty()
        || req.connection_key.is_empty()
        || cfg
            .connection_key
            .as_bytes()
            .ct_eq(req.connection_key.as_bytes())
            .unwrap_u8()
            != 1
    {
        return (
            StatusCode::FORBIDDEN,
            Json(HandshakeResponse {
                approved: false,
                peer_id: None,
                reason: Some("invalid_key".into()),
                nonce: None,
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
                nonce: None,
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
                nonce: None,
            }),
        );
    }

    match (req.nonce.as_deref(), req.signature.as_deref()) {
        (None, None) => {
            let pubkey = pubkey_bytes(&req.pubkey).expect("identity was validated above");
            let nonce = issue_challenge(verifier_id, &req.node_id, pubkey);
            return (
                StatusCode::FORBIDDEN,
                Json(HandshakeResponse {
                    approved: false,
                    peer_id: None,
                    reason: Some("challenge_required".into()),
                    nonce: Some(nonce),
                }),
            );
        }
        (None, Some(_)) => return rejected("missing_nonce"),
        (Some(_), None) => return rejected("missing_signature"),
        (Some(nonce_hex), Some(signature_hex)) => {
            let (nonce, challenge) = match take_challenge(verifier_id, nonce_hex) {
                Ok(challenge) => challenge,
                Err(reason) => return rejected(reason),
            };
            if !challenge.node_id.eq_ignore_ascii_case(&req.node_id)
                || challenge.pubkey
                    != pubkey_bytes(&req.pubkey).expect("identity was validated above")
            {
                return rejected("challenge_mismatch");
            }
            if !signature_matches(challenge.pubkey, &nonce, signature_hex) {
                return rejected("invalid_signature");
            }
        }
    }

    (
        StatusCode::OK,
        Json(HandshakeResponse {
            approved: true,
            peer_id: Some(req.node_id),
            reason: None,
            nonce: None,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use ed25519_dalek::Signer;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::RwLock;

    fn make_state(key: &str, blocked: Vec<String>) -> AuthState {
        Arc::new(RwLock::new(Config {
            gpus: None,
            thermal: Default::default(),
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            profile: crate::profile::NodeProfile::Admin,
            setup_pending: false,
            local_auto_discovery: false,
            tls_required: false,
            quic_enabled: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            cluster_logs_min_level: Default::default(),
            node_name: None,
            huggingface_token: None,
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
            connection_key: key.into(),
            gateway_port: 8080,
            admin_exposure: Default::default(),
            admin_access_token: None,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            relay_peers: vec![],
            advertise_addr: None,
            network: Default::default(),
            api_keys: vec![],
            blocked_nodes: blocked,
            model_dirs: None,
            shard_dir: None,
            workspace_root: None,
            shard_max_gb: crate::storage::DEFAULT_SHARD_MAX_GB,
            resources: Default::default(),
            setup_model: None,
        }))
    }

    fn make_request(key: &str) -> HandshakeRequest {
        let (node_id, pubkey) = identidade_de_verdade();
        HandshakeRequest {
            node_id,
            pubkey,
            connection_key: key.into(),
            nonce: None,
            signature: None,
        }
    }

    fn request_for(identity: &crate::identity::Identity, key: &str) -> HandshakeRequest {
        HandshakeRequest {
            node_id: identity.node_id.0.clone(),
            pubkey: identity.public_key_hex(),
            connection_key: key.into(),
            nonce: None,
            signature: None,
        }
    }

    fn signed_request(
        identity: &crate::identity::Identity,
        key: &str,
        nonce: &str,
    ) -> HandshakeRequest {
        let nonce_bytes = hex::decode(nonce).unwrap();
        HandshakeRequest {
            nonce: Some(nonce.into()),
            signature: Some(hex::encode(
                identity.signing_key.sign(&nonce_bytes).to_bytes(),
            )),
            ..request_for(identity, key)
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
                nonce: None,
                signature: None,
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
                nonce: None,
                signature: None,
            },
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_identidade_coerente_passa() {
        let state = make_state("secret", vec![]);
        let identity = crate::identity::Identity::generate();
        let challenge = request_for(&identity, "secret");
        let (challenge_status, challenge_body) = responder(state.clone(), challenge).await;
        assert_eq!(challenge_status, StatusCode::FORBIDDEN);
        assert_eq!(challenge_body["reason"], "challenge_required");
        let nonce = challenge_body["nonce"].as_str().unwrap();
        let esperado = identity.node_id.0.clone();

        let (status, body) = responder(state, signed_request(&identity, "secret", nonce)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["approved"], true);
        assert_eq!(body["peer_id"], esperado);
    }

    #[tokio::test]
    async fn assinatura_ausente_e_recusada() {
        let state = make_state("secret", vec![]);
        let identity = crate::identity::Identity::generate();
        let body = serde_json::json!({
            "node_id": identity.node_id.0.clone(),
            "pubkey": identity.public_key_hex(),
            "connection_key": "secret"
        });
        let app = axum::Router::new()
            .route("/handshake", axum::routing::post(handle_handshake))
            .with_state(state);
        use axum::body::{to_bytes, Body};
        use axum::http::Request;
        use tower::ServiceExt;
        let response = app
            .oneshot(
                Request::post("/handshake")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "challenge_required");
        assert!(body["nonce"].as_str().is_some());
    }

    #[tokio::test]
    async fn assinatura_ausente_depois_do_nonce_e_recusada() {
        let state = make_state("secret", vec![]);
        let identity = crate::identity::Identity::generate();
        let (_, challenge_body) = responder(state.clone(), request_for(&identity, "secret")).await;
        let nonce = challenge_body["nonce"].as_str().unwrap().to_string();
        let mut req = request_for(&identity, "secret");
        req.nonce = Some(nonce);

        let (status, body) = responder(state, req).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "missing_signature");
    }

    #[tokio::test]
    async fn assinatura_de_outra_chave_e_recusada() {
        let state = make_state("secret", vec![]);
        let identity = crate::identity::Identity::generate();
        let other = crate::identity::Identity::generate();
        let (_, challenge_body) = responder(state.clone(), request_for(&identity, "secret")).await;
        let nonce = challenge_body["nonce"].as_str().unwrap();

        let mut req = signed_request(&other, "secret", nonce);
        req.node_id = identity.node_id.0.clone();
        req.pubkey = identity.public_key_hex();
        let (status, body) = responder(state, req).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "invalid_signature");
    }

    #[tokio::test]
    async fn nonce_so_pode_ser_usado_uma_vez() {
        let state = make_state("secret", vec![]);
        let identity = crate::identity::Identity::generate();
        let (_, challenge_body) = responder(state.clone(), request_for(&identity, "secret")).await;
        let nonce = challenge_body["nonce"].as_str().unwrap().to_string();
        let req = signed_request(&identity, "secret", &nonce);

        let (first_status, _) = responder(state.clone(), req.clone()).await;
        let (second_status, body) = responder(state, req).await;

        assert_eq!(first_status, StatusCode::OK);
        assert_eq!(second_status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "nonce_reused");
    }

    #[tokio::test]
    async fn nonce_expirado_e_recusado() {
        let state = make_state("secret", vec![]);
        let identity = crate::identity::Identity::generate();
        let (_, challenge_body) = responder(state.clone(), request_for(&identity, "secret")).await;
        let nonce = challenge_body["nonce"].as_str().unwrap().to_string();
        let nonce_bytes: [u8; 32] = hex::decode(&nonce).unwrap().try_into().unwrap();
        challenges()
            .lock()
            .unwrap()
            .get_mut(&nonce_bytes)
            .unwrap()
            .expires_at = Instant::now() - Duration::from_secs(1);

        let (status, body) = responder(state, signed_request(&identity, "secret", &nonce)).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "nonce_expired");
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
    async fn chave_vazia_nunca_e_uma_credencial_de_handshake() {
        let state = make_state("", vec![]);
        let (status, body) = responder(state, make_request("")).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "invalid_key");
    }

    #[tokio::test]
    async fn test_handshake_blocked_node() {
        let state = make_state("secret", vec!["blocked-node".into()]);
        let cfg = state.read().await;
        assert!(cfg.blocked_nodes.contains(&"blocked-node".to_string()));
    }
}
