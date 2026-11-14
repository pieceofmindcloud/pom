//! Relay passes: how a visitor's relay admits whoever its network lets in,
//! without learning who.
//!
//! A visitor that relays for its network (`RelayGate::anonymous`) cannot admit
//! by membership: the endpoints that reach it are ephemeral and name no node.
//! It admits by a pass instead - a compact token like the membership token
//! (`crate::membership`), signed by an admin of the chain, naming the network,
//! the relay URL it opens and an expiry, and nothing about its holder. The
//! relay checks it against its own copy of the chain and learns only that an
//! admin let someone in. Admins hand passes out with the membership token
//! renewal (`webui::membership`), a fresh one per relay and ephemeral
//! endpoint.
//!
//! A pass binds to the first endpoint that uses it (`PassLedger`): the relay
//! connection reconnects with the same pass, so it may come back from that
//! endpoint, but never from another one. The ledger also caps the relay
//! connections pass holders keep open (`MAX_LIVE_PER_PASS`, `MAX_LIVE`), so
//! a pass, or the visitors of a network together, cannot take over the
//! relay of a member's machine.

use crate::admin_trust::{AdminTrustDocument, AuthorityPrincipal};
use crate::membership::{open_compact, sign_compact, CLOCK_LEEWAY_SECS, TOKEN_TTL_SECS};
use anyhow::{bail, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The header of a pass: a membership token's header names another type, so
/// neither kind of token can be presented as the other.
const HEADER: &str = r#"{"alg":"EdDSA","typ":"pom-relay-pass"}"#;

/// A pass lasts as long as a membership token, and is renewed with it.
pub(crate) const PASS_TTL_SECS: i64 = TOKEN_TTL_SECS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RelayPassClaims {
    /// The issuing admin's node and key.
    pub(crate) iss: String,
    pub(crate) iss_key: String,
    /// The chain's cluster id.
    pub(crate) net: String,
    /// The relay URL the pass opens, as the relay offers it.
    pub(crate) relay: String,
    pub(crate) iat: i64,
    pub(crate) exp: i64,
    pub(crate) jti: String,
}

/// Signs a pass for `relay`. Only an admin of `document` issues one.
pub(crate) fn issue(
    signing_key: &SigningKey,
    issuer: &AuthorityPrincipal,
    document: &AdminTrustDocument,
    relay: &str,
    now: i64,
) -> Result<String> {
    if issuer.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
        bail!("the signing key is not the issuer's");
    }
    if !crate::admin_trust::control_issuer_is_admin(document, issuer) {
        bail!("only an admin of the trust chain issues relay passes");
    }
    let claims = RelayPassClaims {
        iss: issuer.node_id.clone(),
        iss_key: issuer.pubkey.clone(),
        net: document.anchor.cluster_id.clone(),
        relay: relay.to_string(),
        iat: now,
        exp: now + PASS_TTL_SECS,
        jti: uuid::Uuid::new_v4().simple().to_string(),
    };
    sign_compact(HEADER, &claims, signing_key)
}

/// The claims of `pass` when it opens `relay` on the network of `document`
/// at `now`: signed by an admin the chain names today, and in date.
pub(crate) fn verify(
    pass: &str,
    document: &AdminTrustDocument,
    relay: &str,
    now: i64,
) -> Result<RelayPassClaims> {
    let claims: RelayPassClaims = open_compact(pass, HEADER, |claims: &RelayPassClaims| {
        claims.iss_key.clone()
    })?;
    if claims.net != document.anchor.cluster_id {
        bail!("the pass belongs to another network");
    }
    if !crate::admin_trust::is_admin_key(document, &claims.iss, &claims.iss_key) {
        bail!("the pass was not signed by an admin of the trust chain");
    }
    if claims.relay != relay {
        bail!("the pass opens another relay");
    }
    if claims.exp - claims.iat > PASS_TTL_SECS
        || claims.iat > now + CLOCK_LEEWAY_SECS
        || now > claims.exp + CLOCK_LEEWAY_SECS
    {
        bail!("the pass is out of date");
    }
    Ok(claims)
}

/// Relay connections one pass keeps open at once: the live one and a
/// reconnect racing the close of the old one.
pub(crate) const MAX_LIVE_PER_PASS: usize = 2;

/// Relay connections all pass holders keep open at once on one relay.
pub(crate) const MAX_LIVE: usize = 64;

/// The passes a relay has seen, each bound to the first endpoint that used
/// it, kept until it expires, and the connections they hold open.
#[derive(Debug, Default)]
pub(crate) struct PassLedger {
    seen: HashMap<String, (String, i64)>,
    /// Open connection -> the `jti` of the pass that opened it.
    live: HashMap<String, String>,
}

impl PassLedger {
    /// Whether `endpoint` may open `connection` with the pass `claims` at
    /// `now`: a pass seen before is only good for the endpoint that used it
    /// first, and only while it and the relay are under their caps of open
    /// connections.
    pub(crate) fn admit(
        &mut self,
        claims: &RelayPassClaims,
        endpoint: &str,
        connection: &str,
        now: i64,
    ) -> bool {
        self.seen
            .retain(|_, (_, exp)| now <= *exp + CLOCK_LEEWAY_SECS);
        if self
            .seen
            .get(&claims.jti)
            .is_some_and(|(first, _)| first != endpoint)
        {
            return false;
        }
        let by_this_pass = self.live.values().filter(|jti| **jti == claims.jti).count();
        if self.live.len() >= MAX_LIVE || by_this_pass >= MAX_LIVE_PER_PASS {
            return false;
        }
        self.seen
            .entry(claims.jti.clone())
            .or_insert_with(|| (endpoint.to_string(), claims.exp));
        self.live.insert(connection.to_string(), claims.jti.clone());
        true
    }

    /// `connection` closed; a connection no pass opened is not in here.
    pub(crate) fn release(&mut self, connection: &str) {
        self.live.remove(connection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_trust::AdminTrust;

    const NOW: i64 = 1_800_000_000;
    const RELAY: &str = "http://198.51.100.7:52000/";

    fn admin() -> (AdminTrust, SigningKey, AuthorityPrincipal) {
        let key = SigningKey::from_bytes(&[3; 32]);
        let principal = AuthorityPrincipal::from_signing_key("primary", "peer-primary", &key);
        let trust =
            AdminTrust::bootstrap_primary_in_memory("test-cluster", principal.clone(), &key)
                .unwrap();
        (trust, key, principal)
    }

    #[test]
    fn an_admins_pass_opens_its_relay_and_names_no_holder() {
        let (trust, key, principal) = admin();
        let pass = issue(&key, &principal, &trust.document(), RELAY, NOW).unwrap();
        let claims = verify(&pass, &trust.document(), RELAY, NOW + 60).unwrap();
        assert_eq!(claims.relay, RELAY);
        // Nothing in it points at whoever holds it.
        let body = serde_json::to_value(&claims).unwrap();
        assert_eq!(
            body.as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["exp", "iat", "iss", "iss_key", "jti", "net", "relay"]
        );
    }

    #[test]
    fn a_pass_for_another_relay_network_or_time_is_refused() {
        let (trust, key, principal) = admin();
        let pass = issue(&key, &principal, &trust.document(), RELAY, NOW).unwrap();
        assert!(verify(&pass, &trust.document(), "http://203.0.113.9:52000/", NOW).is_err());
        assert!(verify(&pass, &trust.document(), RELAY, NOW + PASS_TTL_SECS + 120).is_err());

        let other_key = SigningKey::from_bytes(&[9; 32]);
        let other = AuthorityPrincipal::from_signing_key("other", "peer-other", &other_key);
        let other_trust =
            AdminTrust::bootstrap_primary_in_memory("other-cluster", other.clone(), &other_key)
                .unwrap();
        assert!(verify(&pass, &other_trust.document(), RELAY, NOW).is_err());
        // A non-admin cannot issue one.
        assert!(issue(&other_key, &other, &trust.document(), RELAY, NOW).is_err());
    }

    #[test]
    fn a_pass_and_a_membership_token_cannot_stand_in_for_each_other() {
        let (trust, key, principal) = admin();
        let token = crate::membership::issue(
            &key,
            &principal,
            &trust.document(),
            "member",
            "aa",
            crate::config::EnrollmentRole::Visitor,
            NOW,
        )
        .unwrap();
        assert!(verify(&token, &trust.document(), RELAY, NOW).is_err());
        let pass = issue(&key, &principal, &trust.document(), RELAY, NOW).unwrap();
        assert!(crate::membership::verify(&pass, &trust.document(), NOW).is_err());
    }

    #[test]
    fn a_pass_stays_with_the_first_endpoint_that_used_it() {
        let (trust, key, principal) = admin();
        let pass = issue(&key, &principal, &trust.document(), RELAY, NOW).unwrap();
        let claims = verify(&pass, &trust.document(), RELAY, NOW).unwrap();
        let mut ledger = PassLedger::default();
        assert!(ledger.admit(&claims, "ephemeral-a", "c1", NOW));
        // The same endpoint reconnecting is fine; another one is not.
        assert!(ledger.admit(&claims, "ephemeral-a", "c2", NOW + 5));
        assert!(!ledger.admit(&claims, "ephemeral-b", "c3", NOW + 5));
    }

    #[test]
    fn a_pass_and_the_relay_hold_only_so_many_connections_open() {
        let (trust, key, principal) = admin();
        let mut ledger = PassLedger::default();
        let pass = issue(&key, &principal, &trust.document(), RELAY, NOW).unwrap();
        let claims = verify(&pass, &trust.document(), RELAY, NOW).unwrap();
        for n in 0..MAX_LIVE_PER_PASS {
            assert!(ledger.admit(&claims, "ephemeral-a", &format!("own-{n}"), NOW));
        }
        assert!(!ledger.admit(&claims, "ephemeral-a", "own-extra", NOW));
        // A closed connection gives its place back.
        ledger.release("own-0");
        assert!(ledger.admit(&claims, "ephemeral-a", "own-again", NOW));

        // Many passes together stop at the relay's cap.
        for n in MAX_LIVE_PER_PASS..MAX_LIVE {
            let pass = issue(&key, &principal, &trust.document(), RELAY, NOW).unwrap();
            let claims = verify(&pass, &trust.document(), RELAY, NOW).unwrap();
            assert!(ledger.admit(&claims, &format!("ephemeral-{n}"), &format!("c{n}"), NOW));
        }
        let pass = issue(&key, &principal, &trust.document(), RELAY, NOW).unwrap();
        let claims = verify(&pass, &trust.document(), RELAY, NOW).unwrap();
        assert!(!ledger.admit(&claims, "ephemeral-late", "late", NOW));
        ledger.release("c10");
        assert!(ledger.admit(&claims, "ephemeral-late", "late", NOW));
    }
}
