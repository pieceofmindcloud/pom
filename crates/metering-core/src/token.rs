//! Compact Ed25519 tokens: `header.claims.signature`, each part base64url
//! without padding, the signature over the first two. The same format as the
//! node's membership tokens; each kind has its own header, so one kind can
//! never pass for another.

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{de::DeserializeOwned, Serialize};

pub fn sign<T: Serialize>(header: &str, claims: &T, key: &SigningKey) -> Result<String> {
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?)
    );
    let signature = key.sign(signed.as_bytes());
    Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())))
}

/// Opens a token with exactly `header`, checking the signature by the key
/// (hex) that `signer_key` reads from the claims.
pub fn open<T: DeserializeOwned>(
    token: &str,
    header: &str,
    signer_key: impl FnOnce(&T) -> String,
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
    let claims: T = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).context("invalid body")?)
        .context("invalid claims")?;
    let raw: [u8; 32] = hex::decode(signer_key(&claims))
        .context("invalid signer key")?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid signer key size"))?;
    let key = VerifyingKey::from_bytes(&raw).context("invalid signer key")?;
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).context("invalid signature")?)
        .context("invalid signature")?;
    key.verify(format!("{head}.{body}").as_bytes(), &signature)
        .context("the token signature does not match")?;
    Ok(claims)
}
