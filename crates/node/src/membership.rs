//! Membership of a network beyond the chain's own admins: bans and roster
//! vouches, both signed by any admin of the chain and checked by every node
//! against the chain alone.
//!
//! The trust chain names the admins and nobody else. A machine that is not
//! an admin holds its role through the roster: the admission lists, and the
//! **vouches** - the admins' signatures of those entries - that the keepers
//! carry to every node (`AdminTrust::vouched_role`). A removed member is cut
//! by a **revocation**: an admin's signed ban that any admin can issue and
//! every node honors. The membership token this replaced was retired
//! 2026-11-15 (phase C3 of the admin-hub routing,
//! `docs/superpowers/specs/2026-11-14-admin-hub-routing-design.md`, decision
//! 5: no compatibility bridge).
//!
//! The compact-JWT form below stays: `relay_pass` signs its passes with it,
//! under a header of its own, so the two kinds of token cannot stand in for
//! each other.

use crate::admin_trust::{AdminTrustDocument, AuthorityPrincipal};
use crate::config::EnrollmentRole;
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Clock difference tolerated between the issuer and whoever checks.
pub(crate) const CLOCK_LEEWAY_SECS: i64 = 30;

/// `header.claims.signature`, each part base64url without padding, signed by
/// `signing_key` over the first two. Shared with `relay_pass`, whose header
/// differs, so neither kind of token can pass for the other.
pub(crate) fn sign_compact<T: Serialize>(
    header: &str,
    claims: &T,
    signing_key: &SigningKey,
) -> Result<String> {
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?)
    );
    let signature = signing_key.sign(signed.as_bytes());
    Ok(format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// The claims of a compact token with exactly `header`, and a check of its
/// signature by the key `issuer_key` (hex) the claims name. The caller
/// checks what the claims mean, after this.
pub(crate) fn open_compact<T: serde::de::DeserializeOwned>(
    token: &str,
    header: &str,
    issuer_key: impl FnOnce(&T) -> String,
) -> Result<T> {
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("a token has three parts");
    };
    if URL_SAFE_NO_PAD.decode(head).ok().as_deref() != Some(header.as_bytes()) {
        bail!("unsupported token header");
    }
    let claims: T =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).context("invalid token body")?)
            .context("invalid token claims")?;
    let raw: [u8; 32] = hex::decode(issuer_key(&claims))
        .context("invalid issuer key")?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid issuer key size"))?;
    let key = VerifyingKey::from_bytes(&raw).context("invalid issuer key")?;
    let signature = Signature::from_slice(
        &URL_SAFE_NO_PAD
            .decode(signature)
            .context("invalid token signature")?,
    )
    .context("invalid token signature")?;
    key.verify(format!("{head}.{body}").as_bytes(), &signature)
        .context("the token signature does not match")?;
    Ok(claims)
}

/// Domain label of a revocation signature.
const REVOCATION_LABEL: &[u8] = b"pom-revocation/v1\n";

/// A ban: an admin's signed statement that the member `sub` loses its
/// role. The node_id is the hash of the member's key, so it names the key
/// as well. Every approval older than `revoked_at` stops holding on every
/// node that has the entry - the roster's record and the vouches that
/// repeat it. Each entry is signed on its own, so the lists of several
/// admins merge as a plain union, and any admin can ban while the principal
/// is away.
///
/// It lifts only through a new approval: a vouch is signed for a member
/// approved after `revoked_at`, and its `admitted_at` is newer than it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Revocation {
    pub(crate) net: String,
    pub(crate) sub: String,
    pub(crate) revoked_at: i64,
    pub(crate) iss: String,
    pub(crate) iss_key: String,
    pub(crate) signature: String,
}

fn revocation_bytes(revocation: &Revocation) -> Vec<u8> {
    let mut bytes = REVOCATION_LABEL.to_vec();
    bytes.extend(
        serde_json::to_vec(&(
            &revocation.net,
            &revocation.sub,
            revocation.revoked_at,
            &revocation.iss,
            &revocation.iss_key,
        ))
        .unwrap_or_default(),
    );
    bytes
}

/// Signs a revocation of `subject`. Only an admin revokes, and an admin is
/// not revoked this way: only the principal removes one, through the chain.
pub(crate) fn revoke(
    signing_key: &SigningKey,
    issuer: &AuthorityPrincipal,
    document: &AdminTrustDocument,
    subject_node_id: &str,
    now: i64,
) -> Result<Revocation> {
    if issuer.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
        bail!("the signing key is not the issuer's");
    }
    let mut revocation = Revocation {
        net: document.anchor.cluster_id.clone(),
        sub: subject_node_id.to_string(),
        revoked_at: now,
        iss: issuer.node_id.clone(),
        iss_key: issuer.pubkey.clone(),
        signature: String::new(),
    };
    revocation.signature = hex::encode(signing_key.sign(&revocation_bytes(&revocation)).to_bytes());
    verify_revocation(&revocation, document)?;
    Ok(revocation)
}

/// Whether `revocation` holds against `document`: this network, signed by
/// an admin the chain names today, and not aimed at an admin.
pub(crate) fn verify_revocation(
    revocation: &Revocation,
    document: &AdminTrustDocument,
) -> Result<()> {
    if revocation.net != document.anchor.cluster_id {
        bail!("the revocation belongs to another network");
    }
    if revocation.sub.is_empty() {
        bail!("a revocation names its member");
    }
    if !crate::admin_trust::is_admin_key(document, &revocation.iss, &revocation.iss_key) {
        bail!("the revocation was not signed by an admin of the trust chain");
    }
    if std::iter::once(&document.primary)
        .chain(document.grants.iter().map(|grant| &grant.subject))
        .any(|admin| {
            admin.node_id == revocation.sub
                && crate::admin_trust::control_issuer_is_admin(document, admin)
        })
    {
        bail!("an admin is not revoked this way");
    }
    let raw: [u8; 32] = hex::decode(&revocation.iss_key)
        .context("invalid issuer key")?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid issuer key size"))?;
    let key = VerifyingKey::from_bytes(&raw).context("invalid issuer key")?;
    let signature = Signature::from_slice(
        &hex::decode(&revocation.signature).context("invalid revocation signature")?,
    )
    .context("invalid revocation signature")?;
    key.verify(&revocation_bytes(revocation), &signature)
        .context("the revocation signature does not match")
}

/// How long a roster vouch holds on a node without hearing it renewed. The
/// admins re-sign theirs every keeper tick and push a new approval at once,
/// so this only covers a stretch where the exchange could not run: hours -
/// a vouch outlives the minutes-long token it replaced because the ban,
/// not the calendar, is what takes a role away.
pub(crate) const VOUCH_FRESH_SECS: i64 = 4 * 60 * 60;

/// Domain label of a vouch signature.
const VOUCH_LABEL: &[u8] = b"pom-roster-vouch/v1\n";

/// A roster vouch: an admin's signed statement of the role one member was
/// admitted with - the roster entry, signed. The phase C form of the
/// `docs/superpowers/specs/2026-11-14-admin-hub-routing-design.md`: unlike
/// a token it is not carried by the member and does not move per request;
/// it travels the network through the vouch keepers, every node keeps the
/// newest per member, and every node checks each entry against the chain
/// alone. `sub` is the member's `node_id`, which is the hash of its key:
/// the vouch names the key as well. `admitted_at` is the approval's
/// decision time, cut by the member's last ban exactly as an approval is;
/// `issued_at` is when this admin last signed it, the freshness the keepers
/// renew.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Vouch {
    pub(crate) net: String,
    pub(crate) sub: String,
    pub(crate) role: EnrollmentRole,
    pub(crate) admitted_at: i64,
    pub(crate) issued_at: i64,
    pub(crate) iss: String,
    pub(crate) iss_key: String,
    pub(crate) signature: String,
}

fn vouch_bytes(vouch: &Vouch) -> Vec<u8> {
    let mut bytes = VOUCH_LABEL.to_vec();
    bytes.extend(
        serde_json::to_vec(&(
            &vouch.net,
            &vouch.sub,
            &vouch.role,
            vouch.admitted_at,
            vouch.issued_at,
            &vouch.iss,
            &vouch.iss_key,
        ))
        .unwrap_or_default(),
    );
    bytes
}

/// Signs the vouch of `subject` with `role`, admitted at `admitted_at`. Only
/// an admin of `document` signs one, with its own key, and never for a
/// member of the chain's own list: an admin's role is the chain's.
pub(crate) fn sign_vouch(
    signing_key: &SigningKey,
    issuer: &AuthorityPrincipal,
    document: &AdminTrustDocument,
    subject_node_id: &str,
    role: EnrollmentRole,
    admitted_at: i64,
    now: i64,
) -> Result<Vouch> {
    if issuer.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
        bail!("the signing key is not the issuer's");
    }
    if !crate::admin_trust::control_issuer_is_admin(document, issuer) {
        bail!("only an admin of the trust chain vouches a member");
    }
    if subject_node_id.is_empty() {
        bail!("a vouch names its member");
    }
    if std::iter::once(&document.primary)
        .chain(document.grants.iter().map(|grant| &grant.subject))
        .any(|admin| {
            admin.node_id == subject_node_id
                && crate::admin_trust::control_issuer_is_admin(document, admin)
        })
    {
        bail!("an admin is not vouched: its role is the chain's");
    }
    let mut vouch = Vouch {
        net: document.anchor.cluster_id.clone(),
        sub: subject_node_id.to_string(),
        role,
        admitted_at,
        issued_at: now,
        iss: issuer.node_id.clone(),
        iss_key: issuer.pubkey.clone(),
        signature: String::new(),
    };
    vouch.signature = hex::encode(signing_key.sign(&vouch_bytes(&vouch)).to_bytes());
    verify_vouch(&vouch, document, now)?;
    Ok(vouch)
}

/// Whether `vouch` holds against `document` now: this network, signed by an
/// admin the chain names today, not aimed at one, and renewed within the
/// vouch window. The ban cut is not checked here - it belongs to the ledger
/// lookup (`AdminTrust::vouched_role`), like a revocation cuts a roster
/// record.
pub(crate) fn verify_vouch(vouch: &Vouch, document: &AdminTrustDocument, now: i64) -> Result<()> {
    if vouch.net != document.anchor.cluster_id {
        bail!("the vouch belongs to another network");
    }
    if vouch.sub.is_empty() {
        bail!("a vouch names its member");
    }
    if now + CLOCK_LEEWAY_SECS < vouch.issued_at {
        bail!("the vouch was issued in the future");
    }
    if now - CLOCK_LEEWAY_SECS > vouch.issued_at + VOUCH_FRESH_SECS {
        bail!("the vouch is too old to hold: it stopped being renewed");
    }
    if !crate::admin_trust::is_admin_key(document, &vouch.iss, &vouch.iss_key) {
        bail!("the vouch was not signed by an admin of the trust chain");
    }
    if std::iter::once(&document.primary)
        .chain(document.grants.iter().map(|grant| &grant.subject))
        .any(|admin| {
            admin.node_id == vouch.sub
                && crate::admin_trust::control_issuer_is_admin(document, admin)
        })
    {
        bail!("an admin is not vouched");
    }
    let raw: [u8; 32] = hex::decode(&vouch.iss_key)
        .context("invalid issuer key")?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid issuer key size"))?;
    let key = VerifyingKey::from_bytes(&raw).context("invalid issuer key")?;
    let signature =
        Signature::from_slice(&hex::decode(&vouch.signature).context("invalid vouch signature")?)
            .context("invalid vouch signature")?;
    key.verify(&vouch_bytes(vouch), &signature)
        .context("the vouch signature does not match")
}

#[cfg(test)]
#[path = "membership_tests.rs"]
mod tests;
