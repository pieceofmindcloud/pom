use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.0[..12])
    }
}

impl NodeId {
    pub fn from_pubkey(pubkey: &VerifyingKey) -> Self {
        let hash = Sha256::digest(pubkey.as_bytes());
        NodeId(hex::encode(hash))
    }
}

pub struct Identity {
    pub signing_key: SigningKey,
    pub node_id: NodeId,
}

impl Identity {
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        let node_id = NodeId::from_pubkey(&signing_key.verifying_key());
        Identity { signing_key, node_id }
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().as_bytes())
    }
}
