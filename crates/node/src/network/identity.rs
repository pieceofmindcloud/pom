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
/// Arquivo da identidade do nó, derivado da marca: `.pom-node.key`.
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

/// Where the pkarr keypair lives. Not the same key as [`key_filename`]: this
/// one names the node on the pkarr DHT, the other names it in the cluster.
/// Arquivo da chave pkarr, derivado da marca: `.pom.key`.
pub fn pkarr_key_filename() -> String {
    format!(".{}.key", crate::brand::slug())
}

/// Resolves the pkarr key file path, under the project root.
///
/// Era `Path::new(".pom.key")`, relativo ao cwd: lançar o binário de outro
/// diretório (um `ssh host /caminho/pom` roda em `$HOME`) criava uma
/// segunda identidade pkarr e o endereço publicado na DHT mudava. O project
/// root é a regra do resto do projeto (`models/`, `pom.json`, a chave do
/// nó) e vale aqui também.
pub fn pkarr_key_path() -> std::path::PathBuf {
    if let Ok(p) = crate::brand::env("PKARR_KEY") {
        return std::path::PathBuf::from(p);
    }
    crate::config::project_root().join(pkarr_key_filename())
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
            tracing::warn!("{} ilegível — gerando identidade nova", path.display());
        }

        let identity = Self::generate();
        if let Err(e) = std::fs::write(path, hex::encode(identity.signing_key.to_bytes())) {
            tracing::warn!(
                "não deu para gravar {}: {e} — o node_id vai mudar no próximo arranque",
                path.display()
            );
        } else {
            // No Windows não há equivalente disto sem mexer em ACL, e a ACL
            // herdada de um diretório do perfil do usuário já não dá leitura a
            // outras contas. Não fingimos apertar o que não apertamos: quem
            // guardar a chave numa pasta compartilhada está exposto, e isso
            // vale a mesma advertência que vale em Unix.
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

    /// A chave pkarr é a identidade do nó na DHT: ela não pode depender do
    /// diretório de onde alguém lançou o binário. Rodando de outro cwd, o
    /// caminho continua saindo do project root.
    #[test]
    fn the_pkarr_key_lives_under_the_project_root_not_the_cwd() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let cwd_antes = std::env::current_dir().unwrap();
        let home_antes = std::env::var("POM_PKARR_KEY").ok();
        std::env::remove_var("POM_PKARR_KEY");

        let raiz = std::env::temp_dir().join(format!("pom-root-{}", std::process::id()));
        let outro = std::env::temp_dir().join(format!("pom-cwd-{}", std::process::id()));
        std::fs::create_dir_all(raiz.join("models")).unwrap();
        std::fs::create_dir_all(&outro).unwrap();
        std::env::set_var("POM_HOME", &raiz);
        std::env::set_current_dir(&outro).unwrap();

        let caminho = pkarr_key_path();

        std::env::set_current_dir(&cwd_antes).unwrap();
        std::env::remove_var("POM_HOME");
        if let Some(v) = home_antes {
            std::env::set_var("POM_PKARR_KEY", v);
        }
        std::fs::remove_dir_all(&raiz).ok();
        std::fs::remove_dir_all(&outro).ok();

        assert_eq!(caminho, raiz.join(pkarr_key_filename()));
        assert!(
            !caminho.starts_with(&outro),
            "a chave pkarr não pode sair do cwd: {caminho:?}"
        );
    }

    #[test]
    fn identity_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("pom-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(key_filename());

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
        let dir = std::env::temp_dir().join(format!("pom-id-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(key_filename());
        std::fs::write(&path, "isto nao e uma chave").unwrap();

        let id = Identity::load_or_create(&path);
        assert_eq!(id.node_id.0.len(), 64);
        std::fs::remove_dir_all(&dir).ok();
    }
}
