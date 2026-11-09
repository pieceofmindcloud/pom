//! GPU work metering, the logic shared by the node and the metering plugin.
//!
//! Spec: `docs/superpowers/specs/2026-11-08-gpu-work-metering-design.md`.
//!
//! - [`cycle`]: payout cycles numbered from a fixed epoch.
//! - [`chain`]: running SHA-256 link digests over the frames a hop handled.
//! - [`token`]: compact Ed25519 tokens (`header.claims.signature`).
//! - [`receipt`]: the work receipt a hop signs and the bundle a gateway
//!   countersigns, with the checks a gateway runs before countersigning.
//! - [`merkle`]: the tree over TOPLOC chunk proofs and over accepted receipts.
//! - [`toploc`]: TOPLOC proofs (port of Prime Intellect's MIT implementation).
//! - [`pay`]: layer-positions, the pay rule and the payout formula.
//! - [`sample`]: HMAC selection of challenges and audit samples.
//!
//! Nothing here touches the network, the engine or a database.

pub mod chain;
pub mod cycle;
pub mod merkle;
pub mod pay;
pub mod receipt;
pub mod sample;
pub mod settings;
pub mod token;
pub mod toploc;

/// Node ids are `hex(SHA256(raw public key bytes))`, the rule of the node
/// identity (`crates/node/src/network/identity.rs`). `None` for a key that is
/// not 32 bytes of hex.
pub fn node_id_of_pubkey_hex(pubkey_hex: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let raw = hex::decode(pubkey_hex).ok()?;
    (raw.len() == 32).then(|| hex::encode(Sha256::digest(&raw)))
}
