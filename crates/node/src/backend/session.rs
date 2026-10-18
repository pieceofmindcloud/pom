//! `/handshake`: how a node proves to another that it holds the network's
//! connection key without ever sending it.
//!
//! The node that wants to admit a peer (the prober) sends a fresh random
//! challenge. The peer answers with an HMAC-SHA256 of that challenge keyed by
//! the connection key, bound to both node ids, and signs the answer with its
//! node key. The prober recomputes the HMAC with its own key and checks the
//! signature against the peer's announced key. The key never travels, an
//! answer cannot be made up without it, and an answer obtained by sending a
//! challenge to a member names that member, so it admits nobody else.

use crate::auth::AuthState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Domain separation for the proof and its signature.
const HANDSHAKE_CONTEXT: &[u8] = b"pom-handshake/v2";
pub(crate) const HANDSHAKE_CHALLENGE_BYTES: usize = 32;

/// What `/handshake` answers with: the node's config, for the keys it
/// accepts, and its node key, to sign the answer.
#[derive(Clone)]
pub struct HandshakeIdentity {
    pub auth: AuthState,
    pub signing_key: Arc<SigningKey>,
}

impl axum::extract::FromRef<crate::gateway::GatewayState> for HandshakeIdentity {
    fn from_ref(state: &crate::gateway::GatewayState) -> Self {
        Self {
            auth: state.auth.clone(),
            signing_key: state.authority_signing_key.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HandshakeRequest {
    /// The prober: the answer is bound to it.
    pub node_id: String,
    pub pubkey: String,
    /// Fresh random bytes, hex.
    pub challenge: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HandshakeResponse {
    /// The answering node.
    pub node_id: String,
    /// One proof per connection key the node accepts now: the current one
    /// and, during a rotation's grace, the one being replaced.
    pub proofs: Vec<String>,
    /// The answering node's signature over the challenge, both ids and the
    /// proofs.
    pub signature: String,
}

fn pubkey_bytes(pubkey_hex: &str) -> Option<[u8; 32]> {
    let Ok(bytes) = hex::decode(pubkey_hex) else {
        return None;
    };
    let Ok(bytes): Result<[u8; 32], _> = bytes.try_into() else {
        return None;
    };
    // A malformed ed25519 key derives no identity at all.
    VerifyingKey::from_bytes(&bytes).ok()?;
    Some(bytes)
}

/// Is the declared `node_id` really the one derived from this `pubkey`?
/// `node_id` is the peer's name in the cluster and the key of
/// `blocked_nodes`, defined as `SHA256(pubkey)`.
pub(crate) fn node_id_matches_pubkey(node_id: &str, pubkey_hex: &str) -> bool {
    let Some(bytes) = pubkey_bytes(pubkey_hex) else {
        return false;
    };
    hex::encode(Sha256::digest(bytes)).eq_ignore_ascii_case(node_id)
}

/// HMAC-SHA256 of the challenge and both ids, keyed by `connection_key`.
pub(crate) fn proof(connection_key: &str, challenge: &[u8], responder: &str, prober: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(connection_key.as_bytes())
        .expect("HMAC takes a key of any size");
    for part in [
        HANDSHAKE_CONTEXT,
        challenge,
        responder.to_ascii_lowercase().as_bytes(),
        prober.to_ascii_lowercase().as_bytes(),
    ] {
        mac.update(&(part.len() as u32).to_be_bytes());
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

fn signed_message(challenge: &[u8], responder: &str, prober: &str, proofs: &[String]) -> Vec<u8> {
    let mut message = HANDSHAKE_CONTEXT.to_vec();
    for part in [
        challenge,
        responder.to_ascii_lowercase().as_bytes(),
        prober.to_ascii_lowercase().as_bytes(),
    ] {
        message.extend_from_slice(&(part.len() as u32).to_be_bytes());
        message.extend_from_slice(part);
    }
    for proof in proofs {
        message.extend_from_slice(&(proof.len() as u32).to_be_bytes());
        message.extend_from_slice(proof.as_bytes());
    }
    message
}

/// On the prober: whether `answer` proves that the peer `responder` (with
/// `responder_pubkey`) holds `connection_key`, for the `challenge` this node
/// sent as `prober`.
pub(crate) fn answer_proves_key(
    answer: &HandshakeResponse,
    challenge: &[u8],
    responder: &str,
    responder_pubkey: &str,
    prober: &str,
    connection_key: &str,
) -> bool {
    if connection_key.is_empty() || !answer.node_id.eq_ignore_ascii_case(responder) {
        return false;
    }
    let Some(raw) = pubkey_bytes(responder_pubkey) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&raw) else {
        return false;
    };
    let Some(signature) = hex::decode(&answer.signature)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
    else {
        return false;
    };
    if key
        .verify(
            &signed_message(challenge, responder, prober, &answer.proofs),
            &signature,
        )
        .is_err()
    {
        return false;
    }
    let expected = proof(connection_key, challenge, responder, prober);
    answer.proofs.iter().any(|candidate| {
        hex::decode(candidate)
            .ok()
            .is_some_and(|bytes| crate::auth::constant_time_bytes_eq(&bytes, &expected))
    })
}

fn rejected(reason: &str) -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({ "reason": reason })),
    )
        .into_response()
}

pub async fn handle_handshake(
    State(identity): State<HandshakeIdentity>,
    Json(req): Json<HandshakeRequest>,
) -> axum::response::Response {
    let keys = {
        let cfg = identity.auth.read().await;
        if cfg.blocked_nodes.iter().any(|blocked| blocked.eq_ignore_ascii_case(&req.node_id)) {
            return rejected("blocked");
        }
        crate::auth::accepted_connection_keys(&cfg)
    };
    if keys.is_empty() {
        return rejected("no_key");
    }
    // Before anything else: a `node_id` not derived from this `pubkey` would
    // bind the answer to a name nobody can prove.
    if !node_id_matches_pubkey(&req.node_id, &req.pubkey) {
        return rejected("identity_mismatch");
    }
    let Some(challenge) = hex::decode(&req.challenge)
        .ok()
        .filter(|bytes| bytes.len() == HANDSHAKE_CHALLENGE_BYTES)
    else {
        return rejected("invalid_challenge");
    };
    let responder = hex::encode(Sha256::digest(identity.signing_key.verifying_key().as_bytes()));
    let proofs: Vec<String> = keys
        .iter()
        .map(|key| hex::encode(proof(key, &challenge, &responder, &req.node_id)))
        .collect();
    let signature = identity
        .signing_key
        .sign(&signed_message(&challenge, &responder, &req.node_id, &proofs));
    Json(HandshakeResponse {
        node_id: responder,
        proofs,
        signature: hex::encode(signature.to_bytes()),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use rand::{rngs::OsRng, RngCore};
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
            cache_ram_mib: None,
            connection_key: key.into(),
            gateway_port: 8080,
            admin_exposure: Default::default(),
            admin_access_token: None,
            admin_password_default: false,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            relay_peers: vec![],
            relay: Default::default(),
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
            first_run_guide: None,
        }))
    }

    fn responder(key: &str, blocked: Vec<String>) -> (HandshakeIdentity, crate::identity::Identity) {
        let identity = crate::identity::Identity::generate();
        (
            HandshakeIdentity {
                auth: make_state(key, blocked),
                signing_key: Arc::new(identity.signing_key.clone()),
            },
            identity,
        )
    }

    fn challenge() -> [u8; HANDSHAKE_CHALLENGE_BYTES] {
        let mut bytes = [0u8; HANDSHAKE_CHALLENGE_BYTES];
        OsRng.fill_bytes(&mut bytes);
        bytes
    }

    async fn ask(
        state: HandshakeIdentity,
        prober: &crate::identity::Identity,
        challenge: &[u8],
    ) -> (StatusCode, serde_json::Value) {
        use axum::body::to_bytes;
        let response = handle_handshake(
            State(state),
            Json(HandshakeRequest {
                node_id: prober.node_id.0.clone(),
                pubkey: prober.public_key_hex(),
                challenge: hex::encode(challenge),
            }),
        )
        .await;
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn a_member_proves_the_key_without_sending_it() {
        let (state, member) = responder("secret", vec![]);
        let prober = crate::identity::Identity::generate();
        let challenge = challenge();
        let (status, body) = ask(state, &prober, &challenge).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.to_string().contains("secret"));
        let answer: HandshakeResponse = serde_json::from_value(body).unwrap();
        let check = |key: &str| {
            answer_proves_key(
                &answer,
                &challenge,
                &member.node_id.0,
                &member.public_key_hex(),
                &prober.node_id.0,
                key,
            )
        };
        assert!(check("secret"));
        assert!(!check("another-key"));
        assert!(!check(""));
    }

    #[tokio::test]
    async fn an_answer_proves_nothing_for_another_challenge_prober_or_node() {
        let (state, member) = responder("secret", vec![]);
        let prober = crate::identity::Identity::generate();
        let challenge = challenge();
        let (_, body) = ask(state, &prober, &challenge).await;
        let answer: HandshakeResponse = serde_json::from_value(body).unwrap();
        let other = crate::identity::Identity::generate();
        // Another challenge: a replayed answer.
        assert!(!answer_proves_key(&answer, &self::challenge(), &member.node_id.0, &member.public_key_hex(), &prober.node_id.0, "secret"));
        // Another prober: an answer collected by someone else.
        assert!(!answer_proves_key(&answer, &challenge, &member.node_id.0, &member.public_key_hex(), &other.node_id.0, "secret"));
        // Another node presenting the member's answer as its own.
        assert!(!answer_proves_key(&answer, &challenge, &other.node_id.0, &other.public_key_hex(), &prober.node_id.0, "secret"));
    }

    #[tokio::test]
    async fn a_tampered_proof_breaks_the_signature() {
        let (state, member) = responder("secret", vec![]);
        let prober = crate::identity::Identity::generate();
        let challenge = challenge();
        let (_, body) = ask(state, &prober, &challenge).await;
        let mut answer: HandshakeResponse = serde_json::from_value(body).unwrap();
        // A forger who knows the key but not the member's node key.
        answer.proofs = vec![hex::encode(proof("secret", &challenge, &member.node_id.0, &prober.node_id.0)), "00".into()];
        assert!(!answer_proves_key(&answer, &challenge, &member.node_id.0, &member.public_key_hex(), &prober.node_id.0, "secret"));
    }

    #[tokio::test]
    async fn a_pubkey_that_does_not_derive_the_node_id_is_refused() {
        let (state, _) = responder("secret", vec![]);
        let prober = crate::identity::Identity::generate();
        let other = crate::identity::Identity::generate();
        let response = handle_handshake(
            State(state),
            Json(HandshakeRequest {
                node_id: other.node_id.0.clone(),
                pubkey: prober.public_key_hex(),
                challenge: hex::encode(challenge()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_blocked_prober_gets_no_proof() {
        let prober = crate::identity::Identity::generate();
        let (state, _) = responder("secret", vec![prober.node_id.0.clone()]);
        let (status, body) = ask(state, &prober, &challenge()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "blocked");
    }

    #[tokio::test]
    async fn a_node_without_a_key_answers_nothing() {
        let (state, _) = responder("", vec![]);
        let prober = crate::identity::Identity::generate();
        let (status, body) = ask(state, &prober, &challenge()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "no_key");
    }

    #[tokio::test]
    async fn a_short_challenge_is_refused() {
        let (state, _) = responder("secret", vec![]);
        let prober = crate::identity::Identity::generate();
        let (status, body) = ask(state, &prober, &[1, 2, 3]).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "invalid_challenge");
    }
}
