// src/config.rs
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const CONFIG_FILENAME: &str = "rdma-p2p.json";

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
    #[serde(default)]
    pub api_keys: Vec<ApiKey>,
    #[serde(default)]
    pub blocked_nodes: Vec<String>,
}

fn default_gateway_port() -> u16 { 8080 }
fn default_p2p_port() -> u16 { 42777 }

/// Port the local inference server listens on when the config does not name one.
///
/// A node configured through the web UI never passes `--inference-port`, and
/// without a port `apply` refused every model with "este nó subiu sem
/// --inference-port" — the UI could configure nothing at all. Advertising it
/// up front matches what `run-p1.sh`/`run-p2.sh` already do: the port is
/// announced before the Python server exists, and the server is spawned when a
/// slice is actually assigned.
pub const DEFAULT_INFERENCE_PORT: u16 = 8181;

/// Directory `rdma-p2p.json`, `models/` and `venv/` are resolved against.
///
/// Everything used to be resolved against the working directory, so launching
/// the binary by absolute path from somewhere else (`ssh host '/path/to/rdma-p2p'`
/// runs in `$HOME`) silently read a different config and found no models at all —
/// the node then reported it had none of the shards sitting right next to it.
///
/// Resolution order, first hit wins:
///  1. `$RDMA_P2P_HOME`, for anyone who wants to be explicit;
///  2. the working directory, when it actually looks like the project;
///  3. the directory holding the executable, climbing out of `target/release`.
pub fn project_root() -> PathBuf {
    if let Ok(home) = std::env::var("RDMA_P2P_HOME") {
        return PathBuf::from(home);
    }

    let looks_like_project =
        |d: &std::path::Path| d.join(CONFIG_FILENAME).exists() || d.join("models").is_dir();

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if looks_like_project(&cwd) {
        return cwd;
    }

    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(PathBuf::from);
        // …/target/release/rdma-p2p → climb to the project directory.
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
        std::env::var("RDMA_P2P_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| project_root().join(CONFIG_FILENAME))
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
}

#[cfg(test)]
mod tests {
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
            api_keys: vec![],
            blocked_nodes: vec![],
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
            api_keys: vec![],
            blocked_nodes: vec![],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, Role::Node);
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
        };
        assert_eq!(cfg.active_api_key().unwrap().key, "sk-active");
    }
}
