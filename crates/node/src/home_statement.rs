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
//! Phase 1: a node signs its own statement when it is the principal of its home
//! network, the common case (every machine starts with a network of its own,
//! `bootstrap_local_network`). A member of a larger home network would need
//! that network's principal while it visits, which only multi-network visiting
//! (`2026-11-06-multi-network-visitor-design.md`) keeps reachable.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// A statement lasts a day; the node makes a new one at two thirds.
pub(crate) const TTL_SECS: i64 = 86_400;

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
    let token = issue_for(&document, &state.authority_signing_key, &state.network.node_id, now).ok()?;
    if let Ok(mut slot) = OWN.lock() {
        *slot = Some((token.clone(), now + TTL_SECS));
    }
    Some(token)
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
        // The member's own key cannot sign as the principal.
        let forged = metering_core::token::sign(&header(), &claims, &member).unwrap();
        assert!(verify(&forged, &id(&member), 10).is_err());
    }
}
