//! Membership tokens: the role of a worker or a visitor, signed by an admin.
//!
//! The trust chain names the admins and nobody else. A machine that is not
//! an admin carries a short lived token instead, which any admin of the
//! chain signs for the machines the network admitted. Every node checks a
//! token against the chain alone - the signer must be an admin, the network
//! must be this one, the token must not be expired - so a node needs to know
//! nothing about the member it was issued to.
//!
//! The format is a compact JWT signed with EdDSA (Ed25519, the node keys).
//! It names the member's key, and a node accepts it only from that key: in an
//! announce of the node that holds it, or with a request that key signed. A
//! copy is worthless to anyone else.

use crate::admin_trust::{AdminTrustDocument, AuthorityPrincipal};
use crate::config::EnrollmentRole;
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

/// How long a token lasts. It is also the longest a removed member keeps
/// its role on a node that missed the revocation (`Revocation`): admins
/// stop renewing it, and the token runs out.
pub(crate) const TOKEN_TTL_SECS: i64 = 10 * 60;
/// When a member asks for a new token: two thirds into the old one.
pub(crate) const RENEW_AFTER_SECS: i64 = TOKEN_TTL_SECS * 2 / 3;
/// Clock difference tolerated between the issuer and whoever checks.
pub(crate) const CLOCK_LEEWAY_SECS: i64 = 30;
/// The only header a token may have: anything else is refused, so the
/// algorithm is never the token's choice.
const HEADER: &str = r#"{"alg":"EdDSA","typ":"JWT"}"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct MembershipClaims {
    /// The issuing admin's node and key.
    pub(crate) iss: String,
    pub(crate) iss_key: String,
    /// The member's node and key.
    pub(crate) sub: String,
    pub(crate) sub_key: String,
    /// The chain's cluster id: a token of one network is nothing in another.
    pub(crate) net: String,
    pub(crate) role: EnrollmentRole,
    pub(crate) iat: i64,
    pub(crate) exp: i64,
    pub(crate) jti: String,
}

/// Signs a token for `subject` with `role`. Only an admin of `document`
/// issues one, with its own key.
pub(crate) fn issue(
    signing_key: &SigningKey,
    issuer: &AuthorityPrincipal,
    document: &AdminTrustDocument,
    subject_node_id: &str,
    subject_pubkey: &str,
    role: EnrollmentRole,
    now: i64,
) -> Result<String> {
    if issuer.pubkey != hex::encode(signing_key.verifying_key().as_bytes()) {
        bail!("the signing key is not the issuer's");
    }
    if !crate::admin_trust::control_issuer_is_admin(document, issuer) {
        bail!("only an admin of the trust chain issues membership tokens");
    }
    if subject_node_id.is_empty() || subject_pubkey.is_empty() {
        bail!("a membership token names its member");
    }
    let claims = MembershipClaims {
        iss: issuer.node_id.clone(),
        iss_key: issuer.pubkey.clone(),
        sub: subject_node_id.to_string(),
        sub_key: subject_pubkey.to_string(),
        net: document.anchor.cluster_id.clone(),
        role,
        iat: now,
        exp: now + TOKEN_TTL_SECS,
        jti: uuid::Uuid::new_v4().simple().to_string(),
    };
    sign_compact(HEADER, &claims, signing_key)
}

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

/// The claims of `token` when it holds against `document` at `now`: signed
/// by an admin the chain names today, for this network, and in date.
pub(crate) fn verify(
    token: &str,
    document: &AdminTrustDocument,
    now: i64,
) -> Result<MembershipClaims> {
    let claims: MembershipClaims =
        open_compact(token, HEADER, |claims: &MembershipClaims| claims.iss_key.clone())?;
    if claims.net != document.anchor.cluster_id {
        bail!("the token belongs to another network");
    }
    if !crate::admin_trust::is_admin_key(document, &claims.iss, &claims.iss_key) {
        bail!("the token was not signed by an admin of the trust chain");
    }
    if claims.exp - claims.iat > TOKEN_TTL_SECS
        || claims.iat > now + CLOCK_LEEWAY_SECS
        || now > claims.exp + CLOCK_LEEWAY_SECS
    {
        bail!("the membership token is out of date");
    }
    Ok(claims)
}

/// The role `token` gives the node `node_id` with key `pubkey`, when it is
/// valid, was issued to that very node, and was issued after the last time
/// that key was revoked (`revoked_at`).
pub(crate) fn role_of(
    token: Option<&str>,
    node_id: &str,
    pubkey: &str,
    document: &AdminTrustDocument,
    revoked_at: Option<i64>,
    now: i64,
) -> Option<EnrollmentRole> {
    let claims = verify(token?, document, now).ok()?;
    if revoked_at.is_some_and(|revoked| claims.iat <= revoked) {
        return None;
    }
    (claims.sub == node_id && claims.sub_key == pubkey).then_some(claims.role)
}

/// The role a peer's announced token gives it, checked now against the
/// chain and its revocations.
pub(crate) fn peer_role(
    peer: &crate::network::PeerInfo,
    trust: &crate::admin_trust::AdminTrust,
) -> Option<EnrollmentRole> {
    member_role(
        trust,
        peer.membership.as_deref(),
        &peer.node_id,
        &peer.pubkey,
    )
}

/// `role_of` against `trust` as it stands now.
pub(crate) fn member_role(
    trust: &crate::admin_trust::AdminTrust,
    token: Option<&str>,
    node_id: &str,
    pubkey: &str,
) -> Option<EnrollmentRole> {
    role_of(
        token,
        node_id,
        pubkey,
        &trust.document(),
        trust.revoked_node_at(node_id),
        chrono::Utc::now().timestamp(),
    )
}

/// Domain label of a revocation signature.
const REVOCATION_LABEL: &[u8] = b"pom-revocation/v1\n";

/// A ban: an admin's signed statement that the member `sub` loses its
/// role. The node_id is the hash of the member's key, so it names the key
/// as well. Every token issued to that member up to `revoked_at` stops
/// holding, on every node that has the entry. Each entry is signed on its
/// own, so the lists of several admins merge as a plain union, and any
/// admin can ban while the principal is away.
///
/// It lifts only through a new approval: an admin issues again only for a
/// member approved after `revoked_at`, and that token is newer than it.
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
pub(crate) fn verify_revocation(revocation: &Revocation, document: &AdminTrustDocument) -> Result<()> {
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

/// Seconds `token` has left, read without checking it: only for deciding
/// when the holder renews its own.
pub(crate) fn expires_at(token: &str) -> Option<i64> {
    let body = token.split('.').nth(1)?;
    let claims: MembershipClaims =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).ok()?).ok()?;
    Some(claims.exp)
}

#[cfg(test)]
#[path = "membership_tests.rs"]
mod tests;
