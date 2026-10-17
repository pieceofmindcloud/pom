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

/// The node's identity file, derived from the brand: `.pom-node.key`.
///
/// It is the node's **only** key: from it come the `node_id`
/// (`SHA256(pubkey)`), the iroh endpoint id, the pkarr record key (`pk:`), the
/// trust chain's authority signature and the TLS certificate. An IP address
/// never identifies a node; it is only resolved at dial time.
pub fn key_filename() -> String {
    format!(".{}-node.key", crate::brand::slug())
}

/// Resolves the key file path.
///
/// `$POM_NODE_KEY` overrides the default so Docker deployments and
/// multi-instance setups can pin a specific file without touching the
/// project directory.  Falls back to `<project_root>/.pom-node.key`.
pub fn key_path() -> std::path::PathBuf {
    if let Ok(p) = crate::brand::env("NODE_KEY") {
        return std::path::PathBuf::from(p);
    }
    crate::config::project_root().join(key_filename())
}

impl Identity {
    pub fn generate() -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        let node_id = NodeId::from_pubkey(&signing_key.verifying_key());
        Identity {
            signing_key,
            node_id,
        }
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
                    return Identity {
                        signing_key,
                        node_id,
                    };
                }
            }
            tracing::warn!("{} unreadable — generating a new identity", path.display());
        }

        let identity = Self::generate();
        if let Err(e) = std::fs::write(path, hex::encode(identity.signing_key.to_bytes())) {
            tracing::warn!(
                "could not write {}: {e} — the node_id will change on the next start",
                path.display()
            );
        } else {
            // On Windows there is no equivalent of this without touching ACLs,
            // and the ACL inherited from a user profile directory already
            // gives no read access to other accounts. We do not pretend to
            // tighten what we do not tighten: whoever keeps the key in a
            // shared folder is exposed, and that deserves the same warning
            // that applies on Unix.
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

    /// The same key as the iroh endpoint's secret key.
    pub fn secret_key(&self) -> crate::p2p::SecretKey {
        crate::p2p::secret_key(&self.signing_key)
    }

    /// The endpoint id: the public key itself, in hex - the same string as
    /// [`Self::public_key_hex`]. It is what the trust chain records as the
    /// transport identity (`AuthorityPrincipal::peer_id`).
    pub fn peer_id(&self) -> String {
        self.secret_key().public().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("pom-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(key_filename());

        let first = Identity::load_or_create(&path);
        let second = Identity::load_or_create(&path);

        assert_eq!(
            first.node_id.0, second.node_id.0,
            "the node_id must be the same after restarting"
        );
        assert_eq!(first.public_key_hex(), second.public_key_hex());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A single key: the `node_id`, the endpoint id and the pkarr key name
    /// the same public key.
    #[test]
    fn node_id_peer_id_and_pkarr_key_come_from_one_key() {
        let identity = Identity::generate();
        let pubkey = identity.signing_key.verifying_key();
        assert_eq!(identity.node_id.0, NodeId::from_pubkey(&pubkey).0);
        assert_eq!(identity.peer_id(), identity.public_key_hex());
        let pkarr = crate::pkarr_node::PkarrNode::from_signing_key(&identity.signing_key).unwrap();
        assert_eq!(
            pkarr.public_key_uri(),
            crate::pkarr_node::PkarrNode::public_key_uri_from_hex(&identity.public_key_hex())
                .unwrap()
        );
    }

    #[test]
    fn a_corrupt_key_file_does_not_stop_the_node() {
        let dir = std::env::temp_dir().join(format!("pom-id-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(key_filename());
        std::fs::write(&path, "isto nao e uma chave").unwrap();

        let id = Identity::load_or_create(&path);
        assert_eq!(id.node_id.0.len(), 64);
        std::fs::remove_dir_all(&dir).ok();
    }
}
