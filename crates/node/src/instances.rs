//! Instances: more POM nodes on this machine, spawned and supervised by this
//! node (the host), so one machine can serve several deployments at once, each
//! on its own GPUs (`docs/superpowers/specs/2026-11-18-gpu-instances-design.md`).
//!
//! An instance is an ordinary member of the network: its own home under the
//! host's (`<home>/instances/<n>/`), its own key and `node_id`, its own ports,
//! the host's network settings, trust chain and connection key. The rest of
//! the network sees one more peer, so the planner, the pipeline and the deploy
//! do not change. What makes it an instance:
//!
//! - its announce carries an [`InstanceCertificate`] the host signed, naming
//!   the host and the instance (`P2PMessage::Announce.instance`);
//! - an admin signs it a roster vouch on its own when the certificate verifies
//!   and the host is a member (`webui::membership::sign_vouches`), with the
//!   host's role (an admin host's instance is a worker). A banned host's
//!   instances are not signed and age out with the vouch window.
//!
//! The host keeps the list in `pom.db` (`instances`) and starts them again at
//! its own start. The engines get the GPUs from the deployment (`gpus`), never
//! from a process-wide selection, so every instance sees every GPU.

use anyhow::{Context, Result};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What the host signs: this instance is mine.
const CERT_CONTEXT: &str = "pom-instance/v1";

/// The file in an instance's home that holds its certificate.
pub const CERT_FILENAME: &str = "instance.json";

/// The `pom.db` key of the host's list of instances.
const INDEX_KEY: &str = "instances";

/// Instance `n` takes the host's ports plus `n * PORT_STEP`: P2P, gateway and,
/// derived from them as for any node, the inference HTTP (gateway + 101), the
/// inference and gateway QUIC (P2P + 1000, + 2000) and the endpoint
/// (P2P + 10000). 20 keeps clear of the neighbours a machine usually runs.
pub const PORT_STEP: u16 = 20;

/// The most instances one host runs.
pub const MAX_INSTANCES: u32 = 8;

/// The host's statement that a node is one of its instances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceCertificate {
    pub host_node_id: String,
    /// The host's public key, hex: `SHA256(host_pubkey) == host_node_id`.
    pub host_pubkey: String,
    pub instance_node_id: String,
    pub issued_at: i64,
    /// Ed25519 by the host's node key over [`InstanceCertificate::message`], hex.
    pub signature: String,
}

impl InstanceCertificate {
    fn message(host_node_id: &str, instance_node_id: &str, issued_at: i64) -> String {
        format!("{CERT_CONTEXT}|{host_node_id}|{instance_node_id}|{issued_at}")
    }

    pub fn sign(host: &SigningKey, instance_node_id: &str, issued_at: i64) -> Self {
        let host_pubkey = host.verifying_key();
        let host_node_id = crate::identity::NodeId::from_pubkey(&host_pubkey).0;
        let signature = host.sign(Self::message(&host_node_id, instance_node_id, issued_at).as_bytes());
        Self {
            host_node_id,
            host_pubkey: hex::encode(host_pubkey.as_bytes()),
            instance_node_id: instance_node_id.to_string(),
            issued_at,
            signature: hex::encode(signature.to_bytes()),
        }
    }

    /// Whether the host's key signed this certificate for `node_id`, and the
    /// host's id is that key's.
    pub fn verifies_for(&self, node_id: &str) -> bool {
        if self.instance_node_id != node_id || self.host_node_id == node_id {
            return false;
        }
        let Some(pubkey) = hex::decode(&self.host_pubkey)
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .and_then(|bytes| VerifyingKey::from_bytes(&bytes).ok())
        else {
            return false;
        };
        if crate::identity::NodeId::from_pubkey(&pubkey).0 != self.host_node_id {
            return false;
        }
        let Some(signature) = hex::decode(&self.signature)
            .ok()
            .and_then(|bytes| <[u8; 64]>::try_from(bytes).ok())
            .map(|bytes| ed25519_dalek::Signature::from_bytes(&bytes))
        else {
            return false;
        };
        pubkey
            .verify(
                Self::message(&self.host_node_id, &self.instance_node_id, self.issued_at).as_bytes(),
                &signature,
            )
            .is_ok()
    }
}

/// This node's own certificate, when it is an instance: `<home>/instance.json`.
pub fn own_certificate() -> Option<InstanceCertificate> {
    let text = std::fs::read_to_string(crate::config::project_root().join(CERT_FILENAME)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The machine this process runs on, as a node id: the host's when this node
/// is an instance, else its own. Set once at start (`set_local_machine`).
static LOCAL_MACHINE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn set_local_machine(node_id: &str, certificate: Option<&InstanceCertificate>) {
    let machine = certificate
        .filter(|cert| cert.verifies_for(node_id))
        .map(|cert| cert.host_node_id.clone())
        .unwrap_or_else(|| node_id.to_string());
    let _ = LOCAL_MACHINE.set(machine);
}

/// The machine a peer runs on: its host's id when its certificate verifies,
/// else its own id.
pub fn machine_of(node_id: &str, certificate: Option<&InstanceCertificate>) -> String {
    certificate
        .filter(|cert| cert.verifies_for(node_id))
        .map(|cert| cert.host_node_id.clone())
        .unwrap_or_else(|| node_id.to_string())
}

/// Whether a peer runs on this machine: the host and its instances, or two
/// instances of one host. They reach each other on the loopback, and the RTT
/// between them is zero.
pub fn same_machine(node_id: &str, certificate: Option<&InstanceCertificate>) -> bool {
    LOCAL_MACHINE
        .get()
        .is_some_and(|machine| *machine == machine_of(node_id, certificate))
}

/// One instance in the host's list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRecord {
    /// 1, 2, ...: the folder name and the port offset.
    pub n: u32,
    pub node_id: String,
    pub p2p_port: u16,
    pub gateway_port: u16,
    pub created_at: i64,
}

pub fn home_of(n: u32) -> PathBuf {
    crate::config::project_root().join("instances").join(n.to_string())
}

pub fn records(database: &crate::db::PomDb) -> Vec<InstanceRecord> {
    database
        .local_setting_json(INDEX_KEY)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_records(database: &crate::db::PomDb, records: &[InstanceRecord]) -> Result<()> {
    database.save_local_setting(INDEX_KEY, records)
}

/// The ports of instance `n` of a host on `p2p`/`gateway`.
pub fn ports_of(p2p: u16, gateway: u16, n: u32) -> Option<(u16, u16)> {
    let offset = u16::try_from(n).ok()?.checked_mul(PORT_STEP)?;
    // The endpoint is P2P + 10000 and the inference HTTP gateway + 101.
    let p2p = p2p.checked_add(offset).filter(|port| port.checked_add(10_000).is_some())?;
    let gateway = gateway.checked_add(offset).filter(|port| port.checked_add(101).is_some())?;
    Some((p2p, gateway))
}

/// The instance's `pom.json`: the host's, with its own ports, name and
/// profile, the host as its first bootstrap and no relay of its own.
fn instance_config(
    host: &crate::config::Config,
    host_name: &str,
    n: u32,
    p2p: u16,
    gateway: u16,
) -> crate::config::Config {
    let mut cfg = host.clone();
    cfg.saved_at = None;
    cfg.p2p_port = p2p;
    cfg.gateway_port = gateway;
    cfg.inference_port = Some(gateway + 101);
    cfg.profile = crate::profile::NodeProfile::Worker;
    cfg.validator = false;
    cfg.setup_pending = false;
    cfg.local_auto_discovery = false;
    cfg.gpus = None;
    cfg.node_name = Some(format!("{host_name} · instance {n}"));
    let mut bootstrap = vec![format!("127.0.0.1:{}", host.p2p_port)];
    for node in &host.bootstrap_nodes {
        if !bootstrap.contains(node) {
            bootstrap.push(node.clone());
        }
    }
    cfg.bootstrap_nodes = bootstrap;
    // The host's pinned address names the machine; the instance is on the
    // same machine at its own port.
    cfg.advertise_addr = host.advertise_addr.as_deref().and_then(|pinned| {
        let host_part = pinned.rsplit_once(':').map(|(host, _)| host).unwrap_or(pinned);
        (!host_part.is_empty()).then(|| format!("{host_part}:{p2p}"))
    });
    cfg.relay = Default::default();
    cfg.api_keys = Vec::new();
    cfg
}

/// Top-level entries of the host's home an instance never shares: its own
/// identity, config, database, journal and logs.
fn private_entry(name: &str) -> bool {
    name == "instances"
        || name == crate::config::admin_trust_filename()
        || name.starts_with(&format!(".{}-", crate::brand::slug()))
        || name.starts_with(&crate::brand::config_filename())
        || name.starts_with(crate::db::DATABASE_FILENAME)
        || name.ends_with(".log")
        || name.ends_with(".last-slice.json")
        || name == CERT_FILENAME
        || name == "kv-cache"
        || name == "slots"
}

/// Inside a plugin's data folder: what belongs to the running engine.
fn private_plugin_entry(name: &str) -> bool {
    name == "run" || name.ends_with("-sessions") || name == "sessions" || name.ends_with(".log")
}

/// Links the host's shared files into the instance's home: model folders,
/// UI, plugin libraries, and each plugin's data folder one level deep, with a
/// `run/` of its own (two engines must not share one).
fn mirror_home(host_home: &Path, home: &Path, plugin_codes: &[String]) -> Result<()> {
    for entry in std::fs::read_dir(host_home).context("reading the host's home")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if private_entry(&name) {
            continue;
        }
        let target = home.join(&name);
        if target.symlink_metadata().is_ok() {
            continue;
        }
        let is_plugin_data = plugin_codes.contains(&name) && entry.path().is_dir();
        if is_plugin_data {
            std::fs::create_dir_all(&target)?;
            for inner in std::fs::read_dir(entry.path())? {
                let inner = inner?;
                let inner_name = inner.file_name().to_string_lossy().into_owned();
                if private_plugin_entry(&inner_name) {
                    continue;
                }
                let link = target.join(&inner_name);
                if link.symlink_metadata().is_err() {
                    symlink(&inner.path(), &link)?;
                }
            }
        } else {
            symlink(&entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(from: &Path, to: &Path) -> Result<()> {
    std::os::unix::fs::symlink(from, to)
        .with_context(|| format!("linking {} -> {}", to.display(), from.display()))
}

#[cfg(not(unix))]
fn symlink(_from: &Path, _to: &Path) -> Result<()> {
    anyhow::bail!("instances need symbolic links, which this system does not offer")
}

/// The plugin settings of the host's database, so the instance runs the same
/// installed plugins (licences, installs, preferences, UI manifests).
fn copy_plugin_settings(host: &crate::db::PomDb, instance: &crate::db::PomDb) -> Result<()> {
    for key in host.local_setting_keys_with_prefix("plugin_")? {
        if let Some(text) = host.local_setting_json(&key)? {
            let value: serde_json::Value = serde_json::from_str(&text)?;
            instance.save_local_setting(&key, &value)?;
        }
    }
    Ok(())
}

/// Creates instance `n`: its home, key, certificate, config, trust chain and
/// plugin settings. Returns its record. The process is started by [`start`].
pub fn create(
    host_cfg: &crate::config::Config,
    host_name: &str,
    host_key: &SigningKey,
    database: &crate::db::PomDb,
    plugin_codes: &[String],
) -> Result<InstanceRecord> {
    let mut list = records(database);
    let n = (1..=MAX_INSTANCES)
        .find(|n| !list.iter().any(|record| record.n == *n))
        .ok_or_else(|| anyhow::anyhow!("this machine already runs {MAX_INSTANCES} instances"))?;
    let (p2p, gateway) = ports_of(host_cfg.p2p_port, host_cfg.gateway_port, n)
        .ok_or_else(|| anyhow::anyhow!("no free port range for instance {n}"))?;
    let home = home_of(n);
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    let identity = crate::identity::Identity::load_or_create(&home.join(crate::identity::key_filename()));
    let node_id = identity.node_id.0.clone();
    let now = chrono::Utc::now().timestamp();
    let cert = InstanceCertificate::sign(host_key, &node_id, now);
    std::fs::write(home.join(CERT_FILENAME), serde_json::to_vec_pretty(&cert)?)?;
    let cfg = instance_config(host_cfg, host_name, n, p2p, gateway);
    cfg.write_to(&home.join(crate::brand::config_filename()))?;
    let trust = crate::config::admin_trust_path();
    if trust.exists() {
        std::fs::copy(&trust, home.join(crate::config::admin_trust_filename()))
            .context("copying the trust chain")?;
    }
    let host_home = crate::config::project_root();
    mirror_home(&host_home, &home, plugin_codes)?;
    let instance_db = crate::db::PomDb::open(home.join(crate::db::DATABASE_FILENAME))?;
    copy_plugin_settings(database, &instance_db)?;
    let record = InstanceRecord {
        n,
        node_id,
        p2p_port: p2p,
        gateway_port: gateway,
        created_at: now,
    };
    list.push(record.clone());
    save_records(database, &list)?;
    Ok(record)
}

/// The running processes, by instance number.
static RUNNING: Mutex<Option<HashMap<u32, tokio::sync::watch::Sender<bool>>>> = Mutex::new(None);

/// Environment variables the instance must not inherit: they would point it
/// at the host's config, key, database or ports.
fn inherited(name: &str) -> bool {
    let prefix = format!("{}_", crate::brand::slug().to_ascii_uppercase());
    let Some(suffix) = name.strip_prefix(&prefix) else {
        return true;
    };
    matches!(
        suffix,
        "UI_DIST"
            | "PLUGIN_HOST"
            | "DEV_MODE"
            | "DEV"
            | "NO_TLS"
            | "INSECURE_ADMIN_UI"
            | "RELEASE_VERSION"
            | "BUILD_COMMIT"
            | "VIRTUAL_SLICES"
            | "HEADLESS"
            | "GITHUB_TOKEN"
    )
}

/// Starts (or restarts, when it exits) the process of `record`, until
/// [`stop`]. Its output goes to `<instance home>/instance.log`.
pub fn start(record: &InstanceRecord) {
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    {
        let mut running = RUNNING.lock().expect("instances lock");
        let running = running.get_or_insert_with(HashMap::new);
        if running.contains_key(&record.n) {
            return;
        }
        running.insert(record.n, stop_tx);
    }
    let n = record.n;
    tokio::spawn(async move {
        let home = home_of(n);
        loop {
            let exe = match std::env::current_exe() {
                Ok(exe) => exe,
                Err(error) => {
                    tracing::error!("instance {n}: cannot find the pom binary: {error}");
                    return;
                }
            };
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(home.join("instance.log"));
            let mut command = tokio::process::Command::new(exe);
            command.current_dir(&home).env_clear().kill_on_drop(true);
            for (name, value) in std::env::vars_os() {
                if inherited(&name.to_string_lossy()) {
                    command.env(name, value);
                }
            }
            command.env(crate::brand::env_var("HOME"), &home);
            if let Ok(log) = log {
                if let Ok(err) = log.try_clone() {
                    command.stderr(err);
                }
                command.stdout(log);
            }
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    tracing::error!("instance {n}: could not start: {error}");
                    return;
                }
            };
            tracing::info!("instance {n} started (pid {:?})", child.id());
            tokio::select! {
                status = child.wait() => {
                    tracing::warn!("instance {n} exited ({status:?}); starting it again in 5 s");
                }
                _ = stop_rx.changed() => {
                    let _ = child.kill().await;
                    tracing::info!("instance {n} stopped");
                    return;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                _ = stop_rx.changed() => return,
            }
        }
    });
}

/// Stops instance `n`'s process.
pub fn stop(n: u32) {
    if let Some(stop) = RUNNING
        .lock()
        .expect("instances lock")
        .as_mut()
        .and_then(|running| running.remove(&n))
    {
        let _ = stop.send(true);
    }
}

/// Whether instance `n`'s process is supervised now.
pub fn running(n: u32) -> bool {
    RUNNING
        .lock()
        .expect("instances lock")
        .as_ref()
        .is_some_and(|running| running.contains_key(&n))
}

/// Removes instance `n`: stops it and deletes its home and record.
pub fn remove(database: &crate::db::PomDb, n: u32) -> Result<bool> {
    let mut list = records(database);
    let before = list.len();
    list.retain(|record| record.n != n);
    if list.len() == before {
        return Ok(false);
    }
    stop(n);
    save_records(database, &list)?;
    let home = home_of(n);
    if home.exists() {
        std::fs::remove_dir_all(&home).with_context(|| format!("removing {}", home.display()))?;
    }
    Ok(true)
}

/// Starts every instance of the list (the host's start).
pub fn start_all(database: &crate::db::PomDb) {
    for record in records(database) {
        start(&record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_certificate_verifies_only_for_its_instance_and_its_host() {
        let host = SigningKey::from_bytes(&[7u8; 32]);
        let cert = InstanceCertificate::sign(&host, "instance-a", 100);
        assert!(cert.verifies_for("instance-a"));
        assert!(!cert.verifies_for("instance-b"));
        let mut forged = cert.clone();
        forged.instance_node_id = "instance-b".into();
        assert!(!forged.verifies_for("instance-b"), "the signature names instance-a");
        let other = SigningKey::from_bytes(&[8u8; 32]);
        let mut wrong_host = cert.clone();
        wrong_host.host_pubkey = hex::encode(other.verifying_key().as_bytes());
        assert!(!wrong_host.verifies_for("instance-a"), "the key is not the host id's");
    }

    #[test]
    fn instance_ports_step_from_the_hosts() {
        assert_eq!(ports_of(42777, 8080, 1), Some((42797, 8100)));
        assert_eq!(ports_of(42777, 8080, 2), Some((42817, 8120)));
        assert_eq!(ports_of(60000, 8080, 1), None, "the endpoint port would overflow");
    }

    #[test]
    fn the_instance_keeps_the_hosts_network_but_not_its_identity_or_ports() {
        let mut host: crate::config::Config =
            serde_json::from_str(r#"{"profile": "admin", "connection_key": "k", "gateway_port": 8080, "p2p_port": 42777}"#)
                .expect("a minimal config");
        host.p2p_port = 42777;
        host.gateway_port = 8080;
        host.advertise_addr = Some("100.71.127.127:42777".into());
        host.bootstrap_nodes = vec!["10.0.0.9:42777".into()];
        host.gpus = Some(vec![0]);
        let cfg = instance_config(&host, "ksia", 1, 42797, 8100);
        assert_eq!(cfg.p2p_port, 42797);
        assert_eq!(cfg.gateway_port, 8100);
        assert_eq!(cfg.inference_port, Some(8201));
        assert_eq!(cfg.advertise_addr.as_deref(), Some("100.71.127.127:42797"));
        assert_eq!(cfg.bootstrap_nodes, vec!["127.0.0.1:42777", "10.0.0.9:42777"]);
        assert_eq!(cfg.node_name.as_deref(), Some("ksia · instance 1"));
        assert_eq!(cfg.gpus, None);
        assert_eq!(cfg.connection_key, host.connection_key);
    }

    #[test]
    fn the_instance_inherits_no_variable_that_names_the_hosts_files_or_ports() {
        let slug = crate::brand::slug().to_ascii_uppercase();
        assert!(inherited("PATH"));
        assert!(inherited(&format!("{slug}_UI_DIST")));
        assert!(!inherited(&format!("{slug}_HOME")));
        assert!(!inherited(&format!("{slug}_ADVERTISE_ADDR")));
        assert!(!inherited(&format!("{slug}_SLOT_SAVE_PATH")));
    }
}

/// The `/api/ui/instances` routes. Each takes an optional `node_id`: another
/// machine's id sends the call to that machine (`forward_to_node`), so the
/// Deploy Studio adds an instance on whichever host it shows.
pub(crate) mod http {
    use super::*;
    use crate::gateway::GatewayState;
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::Json;
    use serde_json::json;

    #[derive(Debug, Default, Deserialize)]
    pub struct Target {
        #[serde(default)]
        pub node_id: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    pub struct RemoveRequest {
        /// The instance by its number, or by its `node_id` (`instance_node_id`).
        #[serde(default)]
        pub n: Option<u32>,
        #[serde(default)]
        pub instance_node_id: Option<String>,
        /// The host, when it is another machine.
        #[serde(default)]
        pub node_id: Option<String>,
    }

    fn elsewhere<'a>(state: &GatewayState, node_id: &'a Option<String>) -> Option<&'a str> {
        node_id
            .as_deref()
            .filter(|id| !id.is_empty() && *id != state.network.node_id)
    }

    fn refused(code: &str, message: impl Into<String>) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": { "code": code, "message": message.into() } })),
        )
            .into_response()
    }

    async fn listing(state: &GatewayState) -> serde_json::Value {
        let alive: std::collections::HashSet<String> = state
            .network
            .alive_peers()
            .await
            .into_iter()
            .map(|peer| peer.node_id)
            .collect();
        let instances: Vec<serde_json::Value> = records(&state.database)
            .into_iter()
            .map(|record| {
                json!({
                    "n": record.n,
                    "node_id": record.node_id,
                    "p2p_port": record.p2p_port,
                    "gateway_port": record.gateway_port,
                    "created_at": record.created_at,
                    "running": running(record.n),
                    "in_network": alive.contains(&record.node_id),
                })
            })
            .collect();
        json!({
            "host_node_id": state.network.node_id,
            "instance_of": state.network.instance_certificate().map(|cert| cert.host_node_id),
            "instances": instances,
        })
    }

    pub async fn list(
        _auth: crate::auth::ConnectionAuth,
        State(state): State<GatewayState>,
        Query(target): Query<Target>,
    ) -> Response {
        if let Some(node_id) = elsewhere(&state, &target.node_id) {
            return crate::model_provider_backend::forward_to_node(
                &state,
                node_id,
                reqwest::Method::GET,
                "/api/ui/instances",
                None,
            )
            .await;
        }
        Json(listing(&state).await).into_response()
    }

    fn plugin_codes() -> Vec<String> {
        std::fs::read_dir(crate::config::project_root().join("plugins"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn add(
        _auth: crate::auth::ConnectionAuth,
        State(state): State<GatewayState>,
        Json(target): Json<Target>,
    ) -> Response {
        if let Some(node_id) = elsewhere(&state, &target.node_id) {
            return crate::model_provider_backend::forward_to_node(
                &state,
                node_id,
                reqwest::Method::POST,
                "/api/ui/instances",
                Some(json!({})),
            )
            .await;
        }
        if state.network.instance_certificate().is_some() {
            return refused("instance_of_instance", "an instance does not run instances of its own");
        }
        let cfg = state.auth.read().await.clone();
        let host_name = state.network.hostname();
        let key = state.authority_signing_key.clone();
        let database = state.database.clone();
        let created = tokio::task::spawn_blocking(move || {
            create(&cfg, &host_name, &key, &database, &plugin_codes())
        })
        .await;
        match created {
            Ok(Ok(record)) => {
                start(&record);
                tracing::info!(
                    "instance {} created ({}), ports {}/{}",
                    record.n,
                    crate::network::short(&record.node_id),
                    record.p2p_port,
                    record.gateway_port
                );
                Json(listing(&state).await).into_response()
            }
            Ok(Err(error)) => refused("instance_not_created", format!("{error:#}")),
            Err(error) => refused("instance_not_created", error.to_string()),
        }
    }

    pub async fn remove_route(
        _auth: crate::auth::ConnectionAuth,
        State(state): State<GatewayState>,
        Json(request): Json<RemoveRequest>,
    ) -> Response {
        if let Some(node_id) = elsewhere(&state, &request.node_id) {
            return crate::model_provider_backend::forward_to_node(
                &state,
                node_id,
                reqwest::Method::POST,
                "/api/ui/instances/remove",
                Some(json!({ "n": request.n, "instance_node_id": request.instance_node_id })),
            )
            .await;
        }
        let n = request.n.or_else(|| {
            let wanted = request.instance_node_id.as_deref()?;
            records(&state.database)
                .into_iter()
                .find(|record| record.node_id == wanted)
                .map(|record| record.n)
        });
        let Some(n) = n else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": { "code": "instance_not_found", "message": "no such instance" } })),
            )
                .into_response();
        };
        let database = state.database.clone();
        match tokio::task::spawn_blocking(move || remove(&database, n)).await {
            Ok(Ok(true)) => Json(listing(&state).await).into_response(),
            Ok(Ok(false)) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": { "code": "instance_not_found", "message": "no such instance" } })),
            )
                .into_response(),
            Ok(Err(error)) => refused("instance_not_removed", format!("{error:#}")),
            Err(error) => refused("instance_not_removed", error.to_string()),
        }
    }
}
