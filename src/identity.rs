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

/// Where the node's long-lived keypair lives, under the project root.
pub const KEY_FILENAME: &str = ".rdma-p2p-node.key";

impl Identity {
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        let node_id = NodeId::from_pubkey(&signing_key.verifying_key());
        Identity { signing_key, node_id }
    }

    /// Load this node's keypair, creating it on first run.
    ///
    /// The identity has to survive restarts: `node_id` is how the cluster names
    /// a machine. Regenerating it made every restart look like a brand-new peer
    /// — the old entry lingered until it timed out, so the list showed the same
    /// machine twice — and it silently voided `blocked_nodes`, which is keyed by
    /// node_id.
    pub fn load_or_create(path: &std::path::Path) -> Self {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(bytes) = hex::decode(text.trim()) {
                if let Ok(seed) = <[u8; 32]>::try_from(bytes.as_slice()) {
                    let signing_key = SigningKey::from_bytes(&seed);
                    let node_id = NodeId::from_pubkey(&signing_key.verifying_key());
                    return Identity { signing_key, node_id };
                }
            }
            tracing::warn!("{} ilegível — gerando identidade nova", path.display());
        }

        let identity = Self::generate();
        if let Err(e) = std::fs::write(path, hex::encode(identity.signing_key.to_bytes())) {
            tracing::warn!("não deu para gravar {}: {e} — o node_id vai mudar no próximo arranque", path.display());
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
        }
        identity
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("rdma-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(KEY_FILENAME);

        let first = Identity::load_or_create(&path);
        let second = Identity::load_or_create(&path);

        assert_eq!(
            first.node_id.0, second.node_id.0,
            "o node_id tem de ser o mesmo depois de reiniciar"
        );
        assert_eq!(first.public_key_hex(), second.public_key_hex());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_key_file_does_not_stop_the_node() {
        let dir = std::env::temp_dir().join(format!("rdma-id-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(KEY_FILENAME);
        std::fs::write(&path, "isto nao e uma chave").unwrap();

        let id = Identity::load_or_create(&path);
        assert_eq!(id.node_id.0.len(), 64);
        std::fs::remove_dir_all(&dir).ok();
    }
}
