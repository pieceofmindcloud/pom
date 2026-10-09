//! The node's own network, kept aside while it is a member of someone else's.
//!
//! Every node has a network of its own, created at first launch
//! (`setup::bootstrap_local_network`). It is inherent to the machine and is
//! never dropped: joining another network puts it in **standby** - its trust
//! root and its network block of the config are written to one file - and
//! leaving that network brings it back exactly as it was, name, settings,
//! connection key and all.
//!
//! The file holds the primary network's connection key, so it is written
//! owner-only, next to the node key.

use super::*;

/// What makes the primary network what it is: its trust root and the part of
/// the config that belongs to a network rather than to the machine.
#[derive(Serialize, Deserialize)]
struct PrimarySnapshot {
    trust_document: String,
    profile: crate::profile::NodeProfile,
    connection_key: String,
    network: crate::config::NetworkSettings,
    #[serde(default)]
    bootstrap_nodes: Vec<String>,
    #[serde(default)]
    relay_peers: Vec<String>,
    #[serde(default)]
    advertise_addr: Option<String>,
    #[serde(default)]
    blocked_nodes: Vec<String>,
    #[serde(default)]
    tls_required: bool,
    #[serde(default)]
    quic_enabled: bool,
    #[serde(default)]
    cluster_logs_enabled: bool,
    #[serde(default)]
    network_chat_enabled: bool,
}

pub(crate) fn standby_path() -> std::path::PathBuf {
    crate::config::project_root().join(format!(".{}-primary-network.json", crate::brand::slug()))
}

/// Whether this node's own network is in standby, i.e. the node is currently
/// a member of another network.
pub(crate) fn in_standby() -> bool {
    standby_path().exists()
}

/// The name of the network in standby, for the interface.
pub(crate) fn standby_name() -> Option<String> {
    let text = std::fs::read_to_string(standby_path()).ok()?;
    let snapshot: PrimarySnapshot = serde_json::from_str(&text).ok()?;
    Some(snapshot.network.public_name)
}

/// Puts the node's own network in standby, when the network it is in now is
/// its own and none is in standby yet. Returns whether one was saved.
///
/// "Its own" is the network this node is the principal of. A node that is
/// only a member somewhere has nothing of its own to keep here, and when a
/// standby already exists the node is moving between two foreign networks:
/// the standby is left untouched.
pub(crate) fn save_primary(
    config: &crate::config::Config,
    document: &crate::admin_trust::AdminTrustDocument,
    own_pubkey: &str,
) -> anyhow::Result<bool> {
    save_primary_at(&standby_path(), config, document, own_pubkey)
}

fn save_primary_at(
    path: &std::path::Path,
    config: &crate::config::Config,
    document: &crate::admin_trust::AdminTrustDocument,
    own_pubkey: &str,
) -> anyhow::Result<bool> {
    if path.exists() || document.primary.pubkey != own_pubkey {
        return Ok(false);
    }
    let snapshot = PrimarySnapshot {
        trust_document: serde_json::to_string(document)?,
        profile: config.profile,
        connection_key: config.connection_key.clone(),
        network: config.network.clone(),
        bootstrap_nodes: config.bootstrap_nodes.clone(),
        relay_peers: config.relay_peers.clone(),
        advertise_addr: config.advertise_addr.clone(),
        blocked_nodes: config.blocked_nodes.clone(),
        tls_required: config.tls_required,
        quic_enabled: config.quic_enabled,
        cluster_logs_enabled: config.cluster_logs_enabled,
        network_chat_enabled: config.network_chat_enabled,
    };
    std::fs::write(path, serde_json::to_vec_pretty(&snapshot)?)
        .with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(true)
}

/// Brings the network in standby back into `config` and installs its trust
/// root, which must not be on disk (the caller removed the foreign one).
/// Returns `false` when nothing is in standby. The standby file is removed
/// only by [`discard`], after the caller saved the config: a failure in
/// between leaves the snapshot in place to try again.
pub(crate) fn restore_primary(config: &mut crate::config::Config) -> anyhow::Result<bool> {
    restore_primary_at(&standby_path(), &crate::config::admin_trust_path(), config)
}

fn restore_primary_at(
    path: &std::path::Path,
    trust_path: &std::path::Path,
    config: &mut crate::config::Config,
) -> anyhow::Result<bool> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let snapshot: PrimarySnapshot =
        serde_json::from_str(&text).with_context(|| format!("read {}", path.display()))?;
    let document = crate::admin_trust::parse_transported(&snapshot.trust_document)?;
    crate::admin_trust::AdminTrust::install_document(trust_path.to_path_buf(), document)?;
    config.setup_pending = false;
    config.profile = snapshot.profile;
    config.connection_key = snapshot.connection_key;
    config.network = snapshot.network;
    config.bootstrap_nodes = snapshot.bootstrap_nodes;
    config.relay_peers = snapshot.relay_peers;
    config.advertise_addr = snapshot.advertise_addr;
    config.blocked_nodes = snapshot.blocked_nodes;
    config.tls_required = snapshot.tls_required;
    config.quic_enabled = snapshot.quic_enabled;
    config.cluster_logs_enabled = snapshot.cluster_logs_enabled;
    config.network_chat_enabled = snapshot.network_chat_enabled;
    Ok(true)
}

/// Removes the standby file, once the restored config is safely on disk.
pub(crate) fn discard() {
    if let Err(error) = std::fs::remove_file(standby_path()) {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!("não foi possível remover a rede em espera: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: serde_json::Value) -> crate::config::Config {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn the_own_network_goes_to_standby_and_comes_back_as_it_was() {
        let dir = std::env::temp_dir().join(format!("pom-standby-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let standby = dir.join("standby.json");
        let trust_path = dir.join("trust.json");

        // This node's own network: it is the principal.
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let mut seed = signing_key.to_bytes();
        let peer_id = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed[..])
            .unwrap()
            .public()
            .to_peer_id()
            .to_string();
        let node_id = crate::identity::NodeId::from_pubkey(&signing_key.verifying_key()).0;
        let principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            node_id,
            peer_id,
            &signing_key,
        );
        let own_pubkey = principal.pubkey.clone();
        let own_trust = dir.join("own-trust.json");
        let trust = crate::admin_trust::AdminTrust::bootstrap_primary(
            &own_trust,
            "minha-rede",
            principal,
            &signing_key,
        )
        .unwrap();
        let document = trust.document();
        let own = config(serde_json::json!({
            "profile": "admin",
            "connection_key": "chave-da-minha-rede",
            "network": { "public_name": "Minha Rede", "visibility": "public", "public_stats": true },
            "blocked_nodes": ["alguem"],
            "network_chat_enabled": true,
        }));

        // Someone else's key is not this node's network: nothing is kept.
        assert!(!save_primary_at(&standby, &own, &document, "outra-chave").unwrap());
        assert!(save_primary_at(&standby, &own, &document, &own_pubkey).unwrap());
        // Moving on to a second foreign network must not overwrite it.
        let foreign = config(serde_json::json!({
            "profile": "node",
            "connection_key": "chave-alheia",
            "network": { "public_name": "Rede Alheia" },
        }));
        assert!(!save_primary_at(&standby, &foreign, &document, &own_pubkey).unwrap());

        // Leaving: the foreign key is gone and the own network is back whole.
        let mut leaving = foreign;
        leaving.prepare_for_setup();
        assert!(leaving.connection_key.is_empty());
        assert!(restore_primary_at(&standby, &trust_path, &mut leaving).unwrap());
        assert_eq!(leaving.profile, crate::profile::NodeProfile::Admin);
        assert_eq!(leaving.connection_key, "chave-da-minha-rede");
        assert_eq!(leaving.network.public_name, "Minha Rede");
        assert_eq!(leaving.network.visibility, crate::config::NetworkVisibility::Public);
        assert!(leaving.network.public_stats);
        assert_eq!(leaving.blocked_nodes, vec!["alguem".to_string()]);
        assert!(leaving.network_chat_enabled);
        assert!(!leaving.setup_pending);
        let restored = crate::admin_trust::AdminTrust::load_or_empty(trust_path.clone());
        assert_eq!(restored.document().primary.pubkey, own_pubkey);

        // With nothing in standby there is nothing to restore.
        let mut never = config(serde_json::json!({ "profile": "node" }));
        assert!(!restore_primary_at(&dir.join("absent.json"), &trust_path, &mut never).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }
}
