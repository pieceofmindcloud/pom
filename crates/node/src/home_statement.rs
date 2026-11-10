//! Home statements: which network a visitor belongs to
//! (`docs/superpowers/specs/2026-11-09-visitor-charging-design.md`, 1).
//!
//! A visitor's requests are charged to its **home network**, the account
//! `net:<home principal key>` (`crate::payments::network_account`). The visitor
//! proves its home network with a compact token signed by that network's
//! principal, naming the visitor node and its key, the home network's cluster
//! id, the principal and its earlier keys (so a principal transfer keeps the
//! account), and an expiry. The host checks it against the caller the
//! transport proved.
//!
//! A node signs its own statement when it is the principal of its home network,
//! the common case (every machine starts with a network of its own,
//! `bootstrap_local_network`). A member of a larger home network gets one from
//! its principal (`POST /api/internal/home-statement`, [`spawn_keeper`]) while
//! it is home, valid a week ([`MEMBER_TTL_SECS`]), and keeps it in `pom.db` to
//! show while it visits another network, where its home principal is out of
//! reach. Back home it is renewed.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// A statement lasts a day; the node makes a new one at two thirds.
pub(crate) const TTL_SECS: i64 = 86_400;
/// A statement for a member lasts a week: it must outlive a visit.
pub(crate) const MEMBER_TTL_SECS: i64 = 7 * 86_400;
const STORED: &str = "home_statement";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HomeClaims {
    pub(crate) node: String,
    pub(crate) node_key: String,
    /// The home network's cluster id.
    pub(crate) net: String,
    pub(crate) principal: String,
    pub(crate) principal_key: String,
    /// Keys of the home network's earlier principals (its chain's
    /// `primary_history`), oldest first.
    #[serde(default)]
    pub(crate) history: Vec<String>,
    pub(crate) iat: i64,
    pub(crate) exp: i64,
}

fn header() -> String {
    format!(r#"{{"alg":"EdDSA","typ":"{}-home-network/v1"}}"#, crate::brand::slug())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The account the statement names.
pub(crate) fn account_of(claims: &HomeClaims) -> String {
    crate::payments::network_account(&claims.principal_key)
}

/// Signs a statement for `document`'s network, when `key` is its principal.
pub(crate) fn issue_for(
    document: &crate::admin_trust::AdminTrustDocument,
    key: &ed25519_dalek::SigningKey,
    node_id: &str,
    now: i64,
) -> Result<String> {
    let key_hex = hex::encode(key.verifying_key().as_bytes());
    if document.primary.pubkey != key_hex || document.primary.node_id != node_id {
        bail!("only the home network's principal signs its statement");
    }
    let claims = HomeClaims {
        node: node_id.to_string(),
        node_key: key_hex.clone(),
        net: document.anchor.cluster_id.clone(),
        principal: node_id.to_string(),
        principal_key: key_hex,
        history: document.primary_history.iter().map(|p| p.pubkey.clone()).collect(),
        iat: now,
        exp: now + TTL_SECS,
    };
    metering_core::token::sign(&header(), &claims, key)
}

/// Signs a statement for the member `node_id` (key `node_key`) of
/// `document`'s network, when `key` is its principal.
pub(crate) fn issue_for_member(
    document: &crate::admin_trust::AdminTrustDocument,
    key: &ed25519_dalek::SigningKey,
    node_id: &str,
    node_key: &str,
    now: i64,
) -> Result<String> {
    if document.primary.pubkey != hex::encode(key.verifying_key().as_bytes()) {
        bail!("only the home network's principal signs its statements");
    }
    let history = document.primary_history.iter().map(|p| p.pubkey.clone()).collect();
    sign_member(key, &document.anchor.cluster_id, history, node_id, node_key, now)
}

fn sign_member(
    key: &ed25519_dalek::SigningKey,
    net: &str,
    history: Vec<String>,
    node_id: &str,
    node_key: &str,
    now: i64,
) -> Result<String> {
    let key_hex = hex::encode(key.verifying_key().as_bytes());
    if metering_core::node_id_of_pubkey_hex(node_key).as_deref() != Some(node_id) {
        bail!("the member's key is not its node's");
    }
    let claims = HomeClaims {
        node: node_id.to_string(),
        node_key: node_key.to_string(),
        net: net.to_string(),
        principal: metering_core::node_id_of_pubkey_hex(&key_hex).unwrap_or_default(),
        principal_key: key_hex,
        history,
        iat: now,
        exp: now + MEMBER_TTL_SECS,
    };
    metering_core::token::sign(&header(), &claims, key)
}

/// Opens a statement presented by the caller `caller_node` (proved by the
/// transport): signed by the principal it names, for that caller, in date.
pub(crate) fn verify(token: &str, caller_node: &str, now: i64) -> Result<HomeClaims> {
    let claims: HomeClaims = metering_core::token::open(token, &header(), |c: &HomeClaims| c.principal_key.clone())?;
    if metering_core::node_id_of_pubkey_hex(&claims.principal_key).as_deref() != Some(claims.principal.as_str()) {
        bail!("the statement's principal is not its key's");
    }
    if metering_core::node_id_of_pubkey_hex(&claims.node_key).as_deref() != Some(claims.node.as_str()) {
        bail!("the statement's node is not its key's");
    }
    if claims.node != caller_node {
        bail!("the statement names another node");
    }
    if claims.exp < now {
        bail!("the statement expired");
    }
    Ok(claims)
}

static OWN: Mutex<Option<(String, i64)>> = Mutex::new(None);

/// This node's current statement, made again at two thirds of its life.
/// `None` when the node cannot sign one (it is not its home principal).
pub(crate) fn own(state: &crate::gateway::GatewayState) -> Option<String> {
    let now = now();
    if let Ok(slot) = OWN.lock() {
        if let Some((token, exp)) = slot.as_ref() {
            if *exp - now > TTL_SECS / 3 {
                return Some(token.clone());
            }
        }
    }
    // Visiting: the home network is in standby. Otherwise the live chain is home.
    let document = crate::webui::standby::standby_trust_document().unwrap_or_else(|| state.admin_trust.document());
    if let Ok(token) = issue_for(&document, &state.authority_signing_key, &state.network.node_id, now) {
        if let Ok(mut slot) = OWN.lock() {
            *slot = Some((token.clone(), now + TTL_SECS));
        }
        return Some(token);
    }
    // A member: the statement its principal gave it, while in date.
    stored(state).filter(|token| verify(token, &state.network.node_id, now).is_ok())
}

fn stored(state: &crate::gateway::GatewayState) -> Option<String> {
    let json = state.database.local_setting_json(STORED).ok()??;
    serde_json::from_str(&json).ok()
}

/// On the principal, at an admission request: the account the candidate's
/// home statement names, kept on its admission record. A statement for
/// another node, forged or expired names nothing.
pub(crate) fn note_admission(chat: &crate::network_chat::NetworkChat, node_id: &str, home: Option<&str>) {
    if let Some(claims) = home.and_then(|t| verify(t, node_id, now()).ok()) {
        chat.set_admission_account(node_id, account_of(&claims));
    }
}

/// `POST /api/internal/home-statement` (on the home principal): a statement
/// for the member that signed the call.
pub(crate) async fn internal_issue(
    auth: crate::auth::InternalNodeAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let refuse = |code: &str| {
        (axum::http::StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": code }))).into_response()
    };
    let Some(caller) = auth.principal().cloned() else {
        return refuse("signature_required");
    };
    let document = state.admin_trust.document();
    // An admin, or a worker its membership token proves. A visitor's home is
    // its own network, never this one.
    let worker = async {
        let peer = state.network.alive_peers().await.into_iter().find(|p| p.node_id == caller.node_id);
        match peer {
            Some(peer) => {
                state.network.is_member_key(&caller.node_id, &caller.pubkey).await
                    && !crate::gateway::peer_is_visitor(&state, &peer)
            }
            None => false,
        }
    };
    let member = crate::admin_trust::control_issuer_is_admin(&document, &caller) || worker.await;
    if !member {
        return refuse("not_a_member");
    }
    match issue_for_member(&document, &state.authority_signing_key, &caller.node_id, &caller.pubkey, now()) {
        Ok(token) => axum::Json(serde_json::json!({ "statement": token })).into_response(),
        Err(_) => refuse("not_the_principal"),
    }
}

/// Keeps a member's statement from its home principal while it is home:
/// checked every minute (the principal may not be in sight yet at start),
/// asked for only when missing or past half its life.
pub(crate) fn spawn_keeper(state: crate::gateway::GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            // Visiting: the home principal is out of reach.
            if crate::webui::standby::standby_trust_document().is_some() {
                continue;
            }
            let document = state.admin_trust.document();
            if document.primary.node_id == state.network.node_id {
                continue;
            }
            let fresh = stored(&state)
                .and_then(|t| verify(&t, &state.network.node_id, now()).ok())
                .is_some_and(|c| c.exp - now() > MEMBER_TTL_SECS / 2);
            if fresh {
                continue;
            }
            let Some(peer) = state.network.alive_peers().await.into_iter().find(|p| p.node_id == document.primary.node_id) else {
                continue;
            };
            let answer = state
                .control_post_json(&peer, "/api/internal/home-statement", &serde_json::json!({}), std::time::Duration::from_secs(5))
                .await;
            let body = match answer {
                Ok(answer) => answer.json::<serde_json::Value>().await.unwrap_or_default(),
                Err(error) => {
                    tracing::debug!("home statement: the principal did not answer: {error:#}");
                    continue;
                }
            };
            match body["statement"].as_str() {
                Some(token) if verify(token, &state.network.node_id, now()).is_ok() => {
                    let _ = state.database.save_local_setting(STORED, token);
                    tracing::info!("home statement: renewed from the home principal");
                }
                _ => tracing::warn!("home statement: the principal refused: {body}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    fn id(k: &ed25519_dalek::SigningKey) -> String {
        metering_core::node_id_of_pubkey_hex(&hex::encode(k.verifying_key().as_bytes())).unwrap()
    }

    fn statement(k: &ed25519_dalek::SigningKey, now: i64) -> String {
        let key_hex = hex::encode(k.verifying_key().as_bytes());
        let claims = HomeClaims {
            node: id(k),
            node_key: key_hex.clone(),
            net: "home".into(),
            principal: id(k),
            principal_key: key_hex,
            history: vec![],
            iat: now,
            exp: now + TTL_SECS,
        };
        metering_core::token::sign(&header(), &claims, k).unwrap()
    }

    #[test]
    fn a_single_machine_network_signs_its_own_statement() {
        let k = key(3);
        let token = statement(&k, 100);
        let claims = verify(&token, &id(&k), 200).unwrap();
        assert_eq!(account_of(&claims), format!("net:{}", hex::encode(k.verifying_key().as_bytes())));
    }

    #[test]
    fn a_statement_for_another_node_or_expired_is_refused() {
        let k = key(3);
        let token = statement(&k, 100);
        assert!(verify(&token, &id(&key(4)), 200).is_err());
        assert!(verify(&token, &id(&k), 100 + TTL_SECS + 1).is_err());
        let mut forged = token.clone();
        forged.push('A');
        assert!(verify(&forged, &id(&k), 200).is_err());
    }

    #[test]
    fn a_principal_signs_for_a_member_node() {
        let (principal, member) = (key(5), key(6));
        let claims = HomeClaims {
            node: id(&member),
            node_key: hex::encode(member.verifying_key().as_bytes()),
            net: "home".into(),
            principal: id(&principal),
            principal_key: hex::encode(principal.verifying_key().as_bytes()),
            history: vec![],
            iat: 0,
            exp: TTL_SECS,
        };
        let token = metering_core::token::sign(&header(), &claims, &principal).unwrap();
        let opened = verify(&token, &id(&member), 10).unwrap();
        assert_eq!(opened.principal, id(&principal));
        // What the principal issues for a member opens the same way.
        let member_key = hex::encode(member.verifying_key().as_bytes());
        let issued = sign_member(&principal, "home", vec![], &id(&member), &member_key, 10).unwrap();
        let opened = verify(&issued, &id(&member), 20).unwrap();
        assert_eq!((opened.exp, opened.principal.as_str()), (10 + MEMBER_TTL_SECS, id(&principal).as_str()));
        // A key that is not the node's is refused.
        assert!(sign_member(&principal, "home", vec![], &id(&principal), &member_key, 10).is_err());
        // The member's own key cannot sign as the principal.
        let forged = metering_core::token::sign(&header(), &claims, &member).unwrap();
        assert!(verify(&forged, &id(&member), 10).is_err());
    }
}
