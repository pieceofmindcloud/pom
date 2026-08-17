// src/config.rs
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Nome do arquivo de config: derivado da marca (`brand.json`), não escrito à
/// mão, para renomear o produto ser uma edição só.
fn config_filename() -> String {
    crate::brand::config_filename()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Principal,
    Node,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Principal => write!(f, "Principal"),
            Role::Node => write!(f, "Nó"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub key: String,
    pub label: String,
    pub node_id: String,
    #[serde(default)]
    pub active: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub role: Role,
    #[serde(default)]
    pub connection_key: String,
    #[serde(default = "default_gateway_port")]
    pub gateway_port: u16,
    #[serde(default = "default_p2p_port")]
    pub p2p_port: u16,
    #[serde(default)]
    pub inference_port: Option<u16>,
    #[serde(default)]
    pub bootstrap_nodes: Vec<String>,
    /// Address other nodes should keep for this one, e.g.
    /// `"[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777"` or just the bare IPv6.
    ///
    /// Empty means "let each peer read it off the packet", which is right on a
    /// LAN. On the public internet it is not: with privacy extensions the source
    /// address is a temporary IPv6 that rotates, so peers end up holding an
    /// address that has stopped answering. See `network::advertised_addr`.
    #[serde(default)]
    pub advertise_addr: Option<String>,
    #[serde(default)]
    pub api_keys: Vec<ApiKey>,
    #[serde(default)]
    pub blocked_nodes: Vec<String>,
    /// Directories holding **whole** models.
    ///
    /// `None` — the field missing from an old `pom.json`, or never edited —
    /// means "whatever LM Studio and Ollama have on this machine", exactly what
    /// the scan did before it was configurable. Once the operator saves the list
    /// it becomes law, and an empty list really means "look nowhere": the
    /// captain moving every GGUF to `/Volumes/sdcard/models` must not keep
    /// getting the old locations back.
    #[serde(default)]
    pub model_dirs: Option<Vec<String>>,
    /// Where slices produced on this machine are written and looked for.
    /// `None` keeps the historical `<project_root>/models`.
    #[serde(default)]
    pub shard_dir: Option<String>,
    /// Human-readable name for this node shown in the dashboard and peer lists.
    /// Falls back to the OS hostname when absent.
    #[serde(default)]
    pub node_name: Option<String>,
}

fn default_gateway_port() -> u16 {
    8080
}
fn default_p2p_port() -> u16 {
    42777
}

/// Put a peer address the operator typed into the form the resolver accepts.
///
/// The failure this exists for is an address with **no port**: the resolver
/// splits on the last colon, so a bare IPv6 hands it a group of hex digits
/// where a port belongs and answers `invalid port value`. The gossip loop then
/// repeats `Failed to send to …: invalid port value` forever, and the message
/// names neither the offending entry nor the fix — it comes from a layer below
/// anything the operator ever typed. So a bare address gets the default P2P
/// port, which is the only port it could have meant.
///
/// Missing brackets are handled too, though measurement says they are *not* the
/// culprit: `2804:d55::1:42777` resolves fine, because the split leaves a valid
/// IPv6 on the left. Normalising it anyway keeps what we store equal to what we
/// print, which is what makes a peer list comparable by eye.
pub fn normalize_peer_addr(raw: &str) -> String {
    use std::net::{IpAddr, SocketAddr};

    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    // Already valid — `[v6]:port`, `v4:port`.
    if s.parse::<SocketAddr>().is_ok() {
        return s.to_string();
    }
    // An address and nothing else: supply the port it must have meant.
    if let Ok(ip) = s.parse::<IpAddr>() {
        return SocketAddr::new(ip, default_p2p_port()).to_string();
    }
    // Unbracketed IPv6 with a port: resolvable as typed, stored canonically.
    if let Some((host, port)) = s.rsplit_once(':') {
        if host.parse::<std::net::Ipv6Addr>().is_ok() && port.parse::<u16>().is_ok() {
            return format!("[{host}]:{port}");
        }
    }
    // Hostnames resolve at send time; leave anything else untouched rather than
    // rejecting a name we simply do not understand here.
    s.to_string()
}

/// Port the local inference server listens on when the config does not name one.
///
/// A node configured through the web UI never passes `--inference-port`, and
/// without a port `apply` refused every model with "este nó subiu sem
/// --inference-port" — the UI could configure nothing at all. Advertising it
/// up front matches what `run-p1.sh`/`run-p2.sh` already do: the port is
/// announced before the Python server exists, and the server is spawned when a
/// slice is actually assigned.
pub const DEFAULT_INFERENCE_PORT: u16 = 8181;

/// Directory `pom.json`, `models/` and `venv/` are resolved against.
///
/// Everything used to be resolved against the working directory, so launching
/// the binary by absolute path from somewhere else (`ssh host '/path/to/pom'`
/// runs in `$HOME`) silently read a different config and found no models at all —
/// the node then reported it had none of the shards sitting right next to it.
///
/// Resolution order, first hit wins:
///  1. `$POM_HOME`, for anyone who wants to be explicit;
///  2. the working directory, when it actually looks like the project;
///  3. the directory holding the executable, climbing out of `target/release`.
pub fn project_root() -> PathBuf {
    if let Ok(home) = crate::brand::env("HOME") {
        return PathBuf::from(home);
    }

    let looks_like_project =
        |d: &std::path::Path| d.join(config_filename()).exists() || d.join("models").is_dir();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if looks_like_project(&cwd) {
        return cwd;
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(PathBuf::from);
        // …/target/release/pom → climb to the project directory.
        for _ in 0..3 {
            let Some(d) = dir else { break };
            if looks_like_project(&d) {
                return d;
            }
            dir = d.parent().map(PathBuf::from);
        }
    }

    cwd
}

impl Config {
    pub fn config_path() -> PathBuf {
        crate::brand::env("CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| project_root().join(config_filename()))
    }

    pub fn exists() -> bool {
        Self::config_path().exists()
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config at {:?}", path))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse config at {:?}", path))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write config at {:?}", path))
    }

    /// Inference port this node should use, falling back to the default.
    pub fn inference_port_or_default(&self) -> u16 {
        self.inference_port.unwrap_or(DEFAULT_INFERENCE_PORT)
    }

    pub fn active_api_key(&self) -> Option<&ApiKey> {
        self.api_keys.iter().find(|k| k.active)
    }

    /// Directories to scan for whole models, falling back to what LM Studio and
    /// Ollama put on this machine while the operator has not chosen.
    pub fn model_dir_paths(&self) -> Vec<PathBuf> {
        match &self.model_dirs {
            Some(dirs) => dirs.iter().map(PathBuf::from).collect(),
            None => crate::catalog::default_model_dirs(),
        }
    }

    /// True while the whole-model list is still the auto-detected one — the UI
    /// says so instead of pretending the operator picked these paths.
    pub fn model_dirs_are_default(&self) -> bool {
        self.model_dirs.is_none()
    }

    /// Destination of local slicing, and the only directory a peer may pull
    /// from (see `transfer::safe_model_path`).
    pub fn shard_dir_path(&self) -> PathBuf {
        match self.shard_dir.as_deref().map(str::trim) {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => default_shard_dir(),
        }
    }
}

/// `<project_root>/models` — where every slice has always landed.
pub fn default_shard_dir() -> PathBuf {
    project_root().join("models")
}

/// Where the built web interface (`apps/frontend/dist`) is read from at
/// runtime. `$POM_UI_DIST` overrides it — the runtime Docker image sets
/// it explicitly rather than relying on `project_root()` detection, and a dev
/// setup that keeps the bundle elsewhere can point straight at it.
pub fn ui_dist_dir() -> PathBuf {
    if let Ok(dir) = crate::brand::env("UI_DIST") {
        return PathBuf::from(dir);
    }
    if let Some(dir) = bundled_ui_dist() {
        return dir;
    }
    project_root().join("apps/frontend/dist")
}

/// A interface dentro de um bundle `.app` do macOS.
///
/// O bundle é o único jeito de um executável ter ícone no macOS
/// (`scripts/make-macos-app.sh`), e nele o binário vive em
/// `POM.app/Contents/MacOS/`. Dali, `project_root()` cai no cwd de quem
/// abriu - o Finder abre em `/`, e a tela responderia 500. A interface viaja
/// em `Contents/Resources/apps/frontend/dist`, que é o que esta função acha.
///
/// Vale em qualquer plataforma porque é só um teste de caminho; fora de um
/// bundle o diretório não existe e a função devolve `None`.
fn bundled_ui_dist() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let dir = macos_dir
        .parent()?
        .join("Resources")
        .join("apps/frontend/dist");
    dir.is_dir().then_some(dir)
}

#[cfg(test)]
mod tests {
    /// A peer address with no port is what produces `invalid port value` — the
    /// resolver splits on the last colon and finds hex where a port belongs.
    /// Measured, not assumed: an unbracketed IPv6 *with* a port resolves fine.
    #[test]
    fn peer_addr_supplies_the_missing_port() {
        assert_eq!(
            normalize_peer_addr("2804:d55:5579:1000:1c20:e551:6a95:9a0d:42777"),
            "[2804:d55:5579:1000:1c20:e551:6a95:9a0d]:42777"
        );
        // Already correct: untouched.
        assert_eq!(
            normalize_peer_addr(" [2804:d55::1]:42777 "),
            "[2804:d55::1]:42777"
        );
        // No port at all: the default P2P port is the only one it could mean.
        assert_eq!(normalize_peer_addr("2804:d55::1"), "[2804:d55::1]:42777");
        assert_eq!(normalize_peer_addr("192.168.1.5"), "192.168.1.5:42777");
        // IPv4 with port, and a hostname, both pass through.
        assert_eq!(
            normalize_peer_addr("192.168.1.5:42777"),
            "192.168.1.5:42777"
        );
        assert_eq!(normalize_peer_addr("ksia:42777"), "ksia:42777");
        assert_eq!(normalize_peer_addr("  "), "");
    }

    use super::*;

    #[test]
    fn test_roundtrip_principal_config() {
        let cfg = Config {
            role: Role::Principal,
            connection_key: "test-key".into(),
            gateway_port: 8080,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![],
            blocked_nodes: vec![],
            model_dirs: None,
            shard_dir: None,
            node_name: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, Role::Principal);
        assert_eq!(parsed.connection_key, "test-key");
    }

    #[test]
    fn test_roundtrip_node_config() {
        let cfg = Config {
            role: Role::Node,
            connection_key: "node-key".into(),
            gateway_port: 8090,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec!["192.168.1.1:42777".into()],
            advertise_addr: None,
            api_keys: vec![],
            blocked_nodes: vec![],
            model_dirs: None,
            shard_dir: None,
            node_name: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, Role::Node);
    }

    /// A `pom.json` written before the directories were configurable has
    /// to keep behaving exactly as it did: auto-detected libraries, slices in
    /// `<project_root>/models`.
    #[test]
    fn a_config_without_the_new_fields_keeps_todays_behaviour() {
        let old = r#"{"role":"principal","connection_key":"k","gateway_port":8080,
                      "p2p_port":42777,"bootstrap_nodes":[],"api_keys":[],"blocked_nodes":[]}"#;
        let cfg: Config = serde_json::from_str(old).unwrap();
        assert!(cfg.model_dirs_are_default());
        assert_eq!(cfg.model_dir_paths(), crate::catalog::default_model_dirs());
        assert_eq!(cfg.shard_dir_path(), default_shard_dir());
    }

    /// Saving the list is the operator taking over: an empty list means "look
    /// nowhere", not "go back to guessing".
    #[test]
    fn an_explicit_empty_list_is_not_the_default_list() {
        let cfg: Config = serde_json::from_str(
            r#"{"role":"node","model_dirs":[],"shard_dir":"/Volumes/sdcard/fatias"}"#,
        )
        .unwrap();
        assert!(!cfg.model_dirs_are_default());
        assert!(cfg.model_dir_paths().is_empty());
        assert_eq!(
            cfg.shard_dir_path(),
            PathBuf::from("/Volumes/sdcard/fatias")
        );
    }

    #[test]
    fn a_blank_shard_dir_falls_back_instead_of_writing_to_the_root() {
        let cfg: Config = serde_json::from_str(r#"{"role":"node","shard_dir":"   "}"#).unwrap();
        assert_eq!(cfg.shard_dir_path(), default_shard_dir());
    }

    #[test]
    fn test_active_api_key() {
        let cfg = Config {
            role: Role::Principal,
            connection_key: "k".into(),
            gateway_port: 8080,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![
                ApiKey {
                    key: "sk-old".into(),
                    label: "old".into(),
                    node_id: "n1".into(),
                    active: false,
                    created_at: "2026-01-01T00:00:00Z".into(),
                },
                ApiKey {
                    key: "sk-active".into(),
                    label: "active".into(),
                    node_id: "n1".into(),
                    active: true,
                    created_at: "2026-02-01T00:00:00Z".into(),
                },
            ],
            blocked_nodes: vec![],
            model_dirs: None,
            shard_dir: None,
            node_name: None,
        };
        assert_eq!(cfg.active_api_key().unwrap().key, "sk-active");
    }
}
