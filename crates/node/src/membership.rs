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
/// its role: admins stop renewing it, and nothing else revokes it.
pub(crate) const TOKEN_TTL_SECS: i64 = 10 * 60;
/// When a member asks for a new token: two thirds into the old one.
pub(crate) const RENEW_AFTER_SECS: i64 = TOKEN_TTL_SECS * 2 / 3;
/// Clock difference tolerated between the issuer and whoever checks.
const CLOCK_LEEWAY_SECS: i64 = 30;
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
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(HEADER),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?)
    );
    let signature = signing_key.sign(signed.as_bytes());
    Ok(format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// The claims of `token` when it holds against `document` at `now`: signed
/// by an admin the chain names today, for this network, and in date.
pub(crate) fn verify(
    token: &str,
    document: &AdminTrustDocument,
    now: i64,
) -> Result<MembershipClaims> {
    let mut parts = token.split('.');
    let (Some(header), Some(body), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("a membership token has three parts");
    };
    if URL_SAFE_NO_PAD.decode(header).ok().as_deref() != Some(HEADER.as_bytes()) {
        bail!("unsupported membership token header");
    }
    let claims: MembershipClaims =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).context("invalid token body")?)
            .context("invalid token claims")?;
    if claims.net != document.anchor.cluster_id {
        bail!("the token belongs to another network");
    }
    if !crate::admin_trust::is_admin_key(document, &claims.iss, &claims.iss_key) {
        bail!("the token was not signed by an admin of the trust chain");
    }
    let raw: [u8; 32] = hex::decode(&claims.iss_key)
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
    key.verify(format!("{header}.{body}").as_bytes(), &signature)
        .context("the token signature does not match")?;
    if claims.exp - claims.iat > TOKEN_TTL_SECS
        || claims.iat > now + CLOCK_LEEWAY_SECS
        || now > claims.exp + CLOCK_LEEWAY_SECS
    {
        bail!("the membership token is out of date");
    }
    Ok(claims)
}

/// The role `token` gives the node `node_id` with key `pubkey`, when it is
/// valid and was issued to that very node.
pub(crate) fn role_of(
    token: Option<&str>,
    node_id: &str,
    pubkey: &str,
    document: &AdminTrustDocument,
    now: i64,
) -> Option<EnrollmentRole> {
    let claims = verify(token?, document, now).ok()?;
    (claims.sub == node_id && claims.sub_key == pubkey).then_some(claims.role)
}

/// The role a peer's announced token gives it, checked now.
pub(crate) fn peer_role(
    peer: &crate::network::PeerInfo,
    document: &AdminTrustDocument,
) -> Option<EnrollmentRole> {
    role_of(
        peer.membership.as_deref(),
        &peer.node_id,
        &peer.pubkey,
        document,
        chrono::Utc::now().timestamp(),
    )
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
