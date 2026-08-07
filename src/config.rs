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

impl Config {
    pub fn config_path() -> PathBuf {
        std::env::var("RDMA_P2P_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(CONFIG_FILENAME))
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
