//! Contrato mínimo para preparar a ativação Enterprise entre peers.
//!
//! A chave de licença nunca aparece nestes pacotes. O admin coleta provas
//! assinadas de cada peer e usa os fingerprints para uma ativação em lote no
//! servidor. O artefato é distribuído depois pelo transporte autenticado já
//! existente do cluster.

#[cfg(test)]
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::Signer;
#[cfg(test)]
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use enterprise_protocol::Fingerprint;
use serde::{Deserialize, Serialize};

pub const PEER_PROTOCOL: &str = "pom-enterprise-peer/v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PeerFingerprintChallenge {
    pub protocol: String,
    pub nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PeerFingerprintProof {
    pub protocol: String,
    pub nonce: String,
    pub node_id: String,
    pub fingerprint_scheme: String,
    pub fingerprint: String,
    pub installation_id_hash: String,
    pub platform: String,
    pub arch: String,
    pub pom_version: String,
    pub public_key: String,
    pub signature: String,
}

fn signing_bytes(proof: &PeerFingerprintProof) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "protocol": proof.protocol,
        "nonce": proof.nonce,
        "node_id": proof.node_id,
        "fingerprint_scheme": proof.fingerprint_scheme,
        "fingerprint": proof.fingerprint,
        "installation_id_hash": proof.installation_id_hash,
        "platform": proof.platform,
        "arch": proof.arch,
        "pom_version": proof.pom_version,
        "public_key": proof.public_key,
    }))
    .expect("Enterprise proof must serialize")
}

pub(crate) fn answer_challenge(
    challenge: &PeerFingerprintChallenge,
    fingerprint: &Fingerprint,
    pom_version: &str,
    signing_key: &ed25519_dalek::SigningKey,
) -> PeerFingerprintProof {
    let mut proof = PeerFingerprintProof {
        protocol: PEER_PROTOCOL.into(),
        nonce: challenge.nonce.clone(),
        node_id: fingerprint.node_id.clone(),
        fingerprint_scheme: fingerprint.scheme.clone(),
        fingerprint: fingerprint.hash.clone(),
        installation_id_hash: fingerprint.installation_id_hash.clone(),
        platform: fingerprint.platform.clone(),
        arch: fingerprint.arch.clone(),
        pom_version: pom_version.into(),
        public_key: hex::encode(signing_key.verifying_key().as_bytes()),
        signature: String::new(),
    };
    proof.signature = URL_SAFE_NO_PAD.encode(signing_key.sign(&signing_bytes(&proof)).to_bytes());
    proof
}

#[cfg(test)]
fn verify_proof(challenge: &PeerFingerprintChallenge, proof: &PeerFingerprintProof) -> Result<()> {
    if proof.protocol != PEER_PROTOCOL || proof.nonce != challenge.nonce {
        anyhow::bail!("Enterprise proof outside the current challenge");
    }
    let public_key = hex::decode(&proof.public_key).context("invalid Enterprise public key")?;
    let public_key: [u8; 32] = public_key
        .try_into()
        .map_err(|_| anyhow::anyhow!("Enterprise public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key).context("invalid public key")?;
    let signature = URL_SAFE_NO_PAD
        .decode(&proof.signature)
        .context("invalid Enterprise signature")?;
    let signature = Signature::from_slice(&signature).context("invalid signature size")?;
    verifying_key
        .verify(&signing_bytes(proof), &signature)
        .context("Enterprise fingerprint signature does not match")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn peer_proof_is_bound_to_nonce_and_has_no_license_key() {
        let key = ed25519_dalek::SigningKey::generate(&mut OsRng);
        let fingerprint = Fingerprint::from_components(
            "linux",
            "x86_64",
            "node-a",
            "machine",
            "board",
            "installation",
        );
        let challenge = PeerFingerprintChallenge {
            protocol: PEER_PROTOCOL.into(),
            nonce: "nonce-1".into(),
        };
        let proof = answer_challenge(&challenge, &fingerprint, "0.1.0", &key);

        verify_proof(&challenge, &proof).expect("valid proof");
        let json = serde_json::to_string(&proof).expect("JSON");
        assert!(!json.contains("license_key"));
        assert!(!json.contains("installation-secret"));
    }

    #[test]
    fn proof_replay_with_another_nonce_is_rejected() {
        let key = ed25519_dalek::SigningKey::generate(&mut OsRng);
        let fingerprint = Fingerprint::from_components(
            "linux",
            "x86_64",
            "node-a",
            "machine",
            "board",
            "installation",
        );
        let challenge = PeerFingerprintChallenge {
            protocol: PEER_PROTOCOL.into(),
            nonce: "nonce-1".into(),
        };
        let proof = answer_challenge(&challenge, &fingerprint, "0.1.0", &key);
        let replay = PeerFingerprintChallenge {
            protocol: PEER_PROTOCOL.into(),
            nonce: "nonce-2".into(),
        };
        assert!(verify_proof(&replay, &proof).is_err());
    }
}
