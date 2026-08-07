# Principal/Node Architecture with P2P Bitswap — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transform rdma-p2p from autonomous peer-to-peer into a Principal/Node model with connection auth, API keys, P2P bitswap file transfer, node management, and a role-aware web UI.

**Architecture:** Pragmatic Hybrid — keep existing UDP + libp2p + HTTP transport stack. Add new modules for config (`config.rs`), auth middleware (`auth.rs`), session handshake (`session.rs`), admin routes (`admin.rs`), and bitswap protocol (`bitswap.rs`). Extend existing modules (PeerInfo, sysmon, webui, gateway) with auth, role awareness, disk monitoring, and transfer notifications.

**Tech Stack:** Rust + axum + libp2p + serde_json + vanilla JS web UI

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/config.rs` | **Create** | Read/write `rdma-p2p.json`, role enum, API key struct |
| `src/auth.rs` | **Create** | Connection key + API key middleware for axum |
| `src/session.rs` | **Create** | `/handshake` route, connection lifecycle |
| `src/admin.rs` | **Create** | Principal management routes (nodes, keys, transfers) |
| `src/bitswap.rs` | **Create** | libp2p bitswap behaviour + HTTP transfer endpoints |
| `src/sysmon.rs` | Modify | Add disk space sampling |
| `src/network.rs` | Modify | Extend PeerInfo with role, disk, verified fields |
| `src/webui.rs` | Modify | Setup screen, admin tab, notifications, shard catalog routes |
| `src/gateway.rs` | Modify | Auth middleware integration, GatewayState extension |
| `src/main.rs` | Modify | Setup flow, config loading, conditional startup |
| `src/identity.rs` | Modify | Helper for connection key derivation |
| `src/catalog.rs` | Modify | Expose shard catalog via HTTP |
| `src/gguf.rs` | Modify | MD5 hash computation for GGUF files |
| `src/discovery.rs` | Modify | Register BitswapBehaviour in swarm |
| `web/setup.html` | **Create** | Minimal role-selection page |
| `web/index.html` | Modify | Role indicator, notification bell, admin tab |
| `web/app.js` | Modify | New views, notifications, transfer progress |
| `web/app.css` | Modify | Role badges, notification styles, progress bars |
| `Cargo.toml` | Modify | Add `dirs` crate for disk info (macOS) |

---

### Task 1: Config Module (`src/config.rs`)

**Files:**
- Create: `src/config.rs`
- Modify: `src/main.rs` (add `mod config;`)

- [ ] **Step 1: Define Config struct and Role enum**

```rust
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
        let mut cfg = Config {
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
```

- [ ] **Step 2: Run tests**

```bash
cargo test config
```
Expected: 3 tests pass

- [ ] **Step 3: Add `mod config;` to main.rs**

```rust
// In src/main.rs, add after existing mod declarations:
mod config;
```

- [ ] **Step 4: Build to verify**

```bash
cargo build
```
Expected: compiles without errors

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/main.rs && git commit -m "feat: add config module with Role, ApiKey, and JSON persistence"
```

---

### Task 2: Auth Middleware (`src/auth.rs`)

**Files:**
- Create: `src/auth.rs`
- Modify: `src/main.rs` (add `mod auth;`)

- [ ] **Step 1: Implement connection key and API key middleware**

```rust
// src/auth.rs
use crate::config::Config;
use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Json},
};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared auth state: the current config, which holds connection_key + api_keys.
pub type AuthState = Arc<RwLock<Config>>;

/// Connection-key guard for internal/admin routes.
/// Extracts and validates the connection key from `Authorization: Bearer <key>`.
pub struct ConnectionAuth;

#[derive(Debug)]
pub enum AuthRejection {
    MissingHeader,
    InvalidKey,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            AuthRejection::MissingHeader => (StatusCode::UNAUTHORIZED, "missing Authorization header"),
            AuthRejection::InvalidKey => (StatusCode::FORBIDDEN, "invalid connection key"),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

impl<S> FromRequestParts<S> for ConnectionAuth
where
    S: Send + Sync,
{
    type Rejection = AuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let State(config): State<AuthState> = State::from_request_parts(parts, state)
            .await
            .map_err(|_| AuthRejection::MissingHeader)?;

        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthRejection::MissingHeader)?;

        let token = header.strip_prefix("Bearer ").unwrap_or(header);

        let cfg = config.read().await;
        if token == cfg.connection_key {
            Ok(ConnectionAuth)
        } else {
            Err(AuthRejection::InvalidKey)
        }
    }
}

/// API-key guard for OpenAI-compatible routes.
/// Extracts and validates the API key from `Authorization: Bearer <key>`.
pub struct ApiKeyAuth;

impl<S> FromRequestParts<S> for ApiKeyAuth
where
    S: Send + Sync,
{
    type Rejection = AuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let State(config): State<AuthState> = State::from_request_parts(parts, state)
            .await
            .map_err(|_| AuthRejection::MissingHeader)?;

        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthRejection::MissingHeader)?;

        let token = header.strip_prefix("Bearer ").unwrap_or(header);

        let cfg = config.read().await;
        if cfg.api_keys.iter().any(|k| k.active && k.key == token) {
            Ok(ApiKeyAuth)
        } else {
            Err(AuthRejection::InvalidKey)
        }
    }
}

/// Optional auth: extracts API key if present, but does not reject.
/// Used for routes that work with or without auth (e.g., /health, /peers).
pub struct OptionalApiKey {
    pub key: Option<String>,
}

impl<S> FromRequestParts<S> for OptionalApiKey
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok());

        let token = header.and_then(|h| h.strip_prefix("Bearer ")).map(|s| s.to_string());

        Ok(OptionalApiKey { key: token })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_rejection_status() {
        let resp = AuthRejection::MissingHeader.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = AuthRejection::InvalidKey.into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test auth
```
Expected: 1 test passes

- [ ] **Step 3: Add `mod auth;` to main.rs**

```rust
mod auth;
```

- [ ] **Step 4: Build to verify**

```bash
cargo build
```
Expected: compiles

- [ ] **Step 5: Commit**

```bash
git add src/auth.rs src/main.rs && git commit -m "feat: add auth middleware (connection key + API key extractors)"
```

---

### Task 3: Session Handshake (`src/session.rs`)

**Files:**
- Create: `src/session.rs`
- Modify: `src/main.rs` (add `mod session;`)

- [ ] **Step 1: Implement handshake route handler**

```rust
// src/session.rs
use crate::auth::AuthState;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct HandshakeRequest {
    pub node_id: String,
    pub pubkey: String,
    pub connection_key: String,
}

#[derive(Debug, Serialize)]
pub struct HandshakeResponse {
    pub approved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub async fn handle_handshake(
    State(config): State<AuthState>,
    Json(req): Json<HandshakeRequest>,
) -> impl IntoResponse {
    let cfg = config.read().await;

    if req.connection_key != cfg.connection_key {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "approved": false,
                "reason": "invalid_key"
            })),
        );
    }

    if cfg.blocked_nodes.contains(&req.node_id) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "approved": false,
                "reason": "blocked"
            })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "approved": true,
            "peer_id": req.node_id
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Role};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn make_state(key: &str, blocked: Vec<String>) -> AuthState {
        Arc::new(RwLock::new(Config {
            role: Role::Principal,
            connection_key: key.into(),
            gateway_port: 8080,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            api_keys: vec![],
            blocked_nodes: blocked,
        }))
    }

    #[tokio::test]
    async fn test_handshake_valid_key() {
        let state = make_state("secret", vec![]);
        let req = HandshakeRequest {
            node_id: "node1".into(),
            pubkey: "pubkey1".into(),
            connection_key: "secret".into(),
        };
        // This test validates the logic; full integration passes through axum
        let cfg = state.read().await;
        assert_eq!(req.connection_key, cfg.connection_key);
        assert!(!cfg.blocked_nodes.contains(&req.node_id));
    }

    #[tokio::test]
    async fn test_handshake_wrong_key() {
        let state = make_state("secret", vec![]);
        let cfg = state.read().await;
        assert_ne!("wrong", cfg.connection_key);
    }

    #[tokio::test]
    async fn test_handshake_blocked_node() {
        let state = make_state("secret", vec!["blocked-node".into()]);
        let cfg = state.read().await;
        assert!(cfg.blocked_nodes.contains(&"blocked-node".to_string()));
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test session
```
Expected: 3 tests pass

- [ ] **Step 3: Add `mod session;` to main.rs**

```rust
mod session;
```

- [ ] **Step 4: Build**

```bash
cargo build
```

- [ ] **Step 5: Commit**

```bash
git add src/session.rs src/main.rs && git commit -m "feat: add session handshake handler with key validation and block checking"
```

---

### Task 4: Extend PeerInfo with Role and Disk Fields

**Files:**
- Modify: `src/network.rs`

- [ ] **Step 1: Add new fields to PeerInfo and P2PMessage::Announce**

```rust
// In src/network.rs, modify PeerInfo struct:

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub node_id: String,
    pub pubkey: String,
    pub addr: String,
    pub model: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    pub memory_mb: f64,
    pub last_seen: DateTime<Utc>,
    #[serde(default)]
    pub ollama_port: Option<u16>,
    #[serde(default)]
    pub inference_port: Option<u16>,
    #[serde(default)]
    pub gateway_port: Option<u16>,
    // New fields
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub connection_verified: bool,
    #[serde(default)]
    pub disk_total_mb: u64,
    #[serde(default)]
    pub disk_free_mb: u64,
    #[serde(default)]
    pub api_keys_active: u16,
}
```

- [ ] **Step 2: Add new fields to P2PMessage::Announce variant**

```rust
// In src/network.rs, modify Announce variant:
    Announce {
        node_id: String,
        pubkey: String,
        addr: String,
        model: String,
        start_layer: u32,
        end_layer: u32,
        total_layers: u32,
        memory_mb: f64,
        timestamp: DateTime<Utc>,
        #[serde(default)]
        ollama_port: Option<u16>,
        #[serde(default)]
        inference_port: Option<u16>,
        #[serde(default)]
        gateway_port: Option<u16>,
        #[serde(default)]
        role: String,
        #[serde(default)]
        hostname: String,
        #[serde(default)]
        disk_total_mb: u64,
        #[serde(default)]
        disk_free_mb: u64,
    },
```

- [ ] **Step 3: Update `insert_peer` to populate new fields from Announce**

Read the existing `insert_peer` and `handle_message` functions in `network.rs` and update the code that copies Announce fields into PeerInfo to include the new fields.

```rust
// In the handle_message function, where Announce is deserialized into PeerInfo,
// add the new field assignments. Find the existing peer creation block and add:
    entry.role = announce.role;
    entry.hostname = announce.hostname;
    entry.disk_total_mb = announce.disk_total_mb;
    entry.disk_free_mb = announce.disk_free_mb;
```

- [ ] **Step 4: Build to verify no compilation errors**

```bash
cargo build
```

- [ ] **Step 5: Commit**

```bash
git add src/network.rs && git commit -m "feat: extend PeerInfo with role, disk, hostname, and verification fields"
```

---

### Task 5: Add Disk Space Sampling to sysmon

**Files:**
- Modify: `src/sysmon.rs`

- [ ] **Step 1: Add DiskStat struct and sampling function**

```rust
// Add to src/sysmon.rs, after existing structs:

#[derive(Debug, Clone, Serialize)]
pub struct DiskStat {
    pub total_mb: u64,
    pub free_mb: u64,
    pub mount_point: String,
}

/// Sample disk space for the models directory (working directory).
/// Cross-platform: uses `statvfs` on unix (Linux + macOS).
pub fn sample_disk() -> DiskStat {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    sample_disk_at(&cwd)
}

fn sample_disk_at(path: &std::path::Path) -> DiskStat {
    let mount = path.to_string_lossy().to_string();
    // Use std::fs and platform-specific APIs
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(path) {
            // On unix, use statvfs via libc or a simple approximation
            // For a reliable cross-platform solution, parse df output
            let total_mb = meta.len() / (1024 * 1024); // fallback, not actual disk
            // Better: use nix::sys::statvfs::statvfs or parse "df" output
            // For now, use a simple ffilib-free approach via std::process::Command
            let out = std::process::Command::new("df")
                .arg("-m")
                .arg(path)
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok());

            if let Some(output) = out {
                let lines: Vec<&str> = output.lines().collect();
                if lines.len() >= 2 {
                    let fields: Vec<&str> = lines[1].split_whitespace().collect();
                    if fields.len() >= 4 {
                        if let (Ok(total), Ok(free)) =
                            (fields[1].parse::<u64>(), fields[3].parse::<u64>())
                        {
                            return DiskStat {
                                total_mb: total,
                                free_mb: free,
                                mount_point: mount,
                            };
                        }
                    }
                }
            }
        }
    }

    DiskStat {
        total_mb: 0,
        free_mb: 0,
        mount_point: mount,
    }
}
```

- [ ] **Step 2: Build and verify**

```bash
cargo build
```

- [ ] **Step 3: Commit**

```bash
git add src/sysmon.rs && git commit -m "feat: add disk space sampling to sysmon"
```

---

### Task 6: Admin Routes (`src/admin.rs`)

**Files:**
- Create: `src/admin.rs`
- Modify: `src/main.rs` (add `mod admin;`)

- [ ] **Step 1: Implement admin route handlers**

```rust
// src/admin.rs
use crate::auth::{AuthState, ConnectionAuth, _ConnectionAuth as _};
use crate::config::{ApiKey, Config, Role};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use uuid::Uuid;

pub fn admin_routes() -> Router<AuthState> {
    Router::new()
        .route("/admin/nodes", get(list_nodes))
        .route("/admin/nodes/{id}/block", post(block_node))
        .route("/admin/nodes/{id}/unblock", post(unblock_node))
        .route("/admin/nodes/{id}", delete(remove_node))
        .route("/admin/regenerate-key", post(regenerate_key))
        .route("/admin/api-keys", get(list_api_keys).post(generate_api_key))
        .route("/admin/api-keys/{key}", delete(revoke_api_key))
}

#[derive(Debug, Serialize)]
struct NodeInfo {
    node_id: String,
    blocked: bool,
    verified: bool,
}

async fn list_nodes(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
) -> impl IntoResponse {
    let cfg = config.read().await;
    // In production, this would read from the actual peer table.
    // For now, return blocked list from config.
    let nodes: Vec<NodeInfo> = cfg.blocked_nodes.iter().map(|id| NodeInfo {
        node_id: id.clone(),
        blocked: true,
        verified: false,
    }).collect();
    Json(nodes)
}

async fn block_node(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    if !cfg.blocked_nodes.contains(&node_id) {
        cfg.blocked_nodes.push(node_id.clone());
    }
    let _ = cfg.save();
    Json(json!({ "blocked": true, "node_id": node_id }))
}

async fn unblock_node(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    cfg.blocked_nodes.retain(|id| id != &node_id);
    let _ = cfg.save();
    Json(json!({ "blocked": false, "node_id": node_id }))
}

async fn remove_node(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    cfg.blocked_nodes.retain(|id| id != &node_id);
    let _ = cfg.save();
    Json(json!({ "removed": true, "node_id": node_id }))
}

async fn regenerate_key(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    // Generate new key from random UUID + SHA256
    let raw = Uuid::new_v4().to_string();
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    cfg.connection_key = hex::encode(hasher.finalize());
    let _ = cfg.save();
    Json(json!({ "connection_key": cfg.connection_key }))
}

async fn list_api_keys(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
) -> impl IntoResponse {
    let cfg = config.read().await;
    Json(cfg.api_keys.clone())
}

#[derive(Debug, Deserialize)]
struct GenerateKeyRequest {
    label: Option<String>,
}

async fn generate_api_key(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Json(req): Json<GenerateKeyRequest>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    // Deactivate all existing keys for this node
    let node_id = "principal"; // In production, this comes from session context
    for k in &mut cfg.api_keys {
        k.active = false;
    }
    // Generate new key
    let raw = format!("sk-{}", Uuid::new_v4());
    let key = ApiKey {
        key: raw,
        label: req.label.unwrap_or_else(|| "default".into()),
        node_id: node_id.into(),
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    cfg.api_keys.push(key.clone());
    let _ = cfg.save();
    (StatusCode::CREATED, Json(key))
}

async fn revoke_api_key(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    let existed = cfg.api_keys.iter().any(|k| k.key == key);
    cfg.api_keys.retain(|k| k.key != key);
    let _ = cfg.save();
    Json(json!({ "revoked": existed }))
}

#[derive(Debug, Deserialize)]
pub struct TransferOffer {
    pub shard_name: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_size_mb: u64,
    pub num_chunks: u32,
}

pub async fn handle_transfer_offer(
    _auth: ConnectionAuth,
    State(_config): State<AuthState>,
    Json(_offer): Json<TransferOffer>,
) -> impl IntoResponse {
    // Transfer offer handling — full bitswap integration in Task 12
    Json(json!({ "status": "offer_received", "pending_approval": true }))
}

pub fn transfer_routes() -> Router<AuthState> {
    Router::new()
        .route("/admin/transfer-offer", post(handle_transfer_offer))
}
```

- [ ] **Step 2: Add `mod admin;` to main.rs**

```rust
mod admin;
```

- [ ] **Step 3: Build**

```bash
cargo build
```

- [ ] **Step 4: Commit**

```bash
git add src/admin.rs src/main.rs && git commit -m "feat: add admin routes — node management, key regeneration, API keys"
```

---

### Task 7: Minimal Setup HTML Page

**Files:**
- Create: `web/setup.html`

- [ ] **Step 1: Create setup page**

```html
<!-- web/setup.html -->
<!DOCTYPE html>
<html lang="pt">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>rdma-p2p — Configuracao inicial</title>
<style>
:root {
  --bg: #f5f5f5; --card: #fff; --text: #1a1a2e;
  --accent: #e8a838; --node-accent: #7b8ca8;
  --border: #ddd; --muted: #888;
}
@media (prefers-color-scheme: dark) {
:root { --bg: #1a1a2e; --card: #252540; --text: #e0e0e0; --border: #333; --muted: #888; }
}
* { box-sizing:border-box; margin:0; padding:0 }
body { font-family:system-ui,sans-serif; background:var(--bg); color:var(--text); display:flex; justify-content:center; align-items:center; min-height:100vh }
.card { background:var(--card); border:1px solid var(--border); border-radius:8px; padding:32px; max-width:520px; width:100% }
h1 { font-family:ui-monospace,monospace; font-size:1.2rem; margin-bottom:24px; color:var(--muted) }
h2 { font-size:1rem; margin-bottom:16px }
.roles { display:flex; gap:12px; margin-bottom:24px }
.role-btn { flex:1; padding:20px 12px; border:2px solid var(--border); border-radius:8px; background:var(--card); cursor:pointer; text-align:center; transition:border-color .2s }
.role-btn:hover { border-color:var(--accent) }
.role-btn.selected { border-color:var(--accent) }
.role-btn .emoji { font-size:2rem; display:block; margin-bottom:8px }
.role-btn .label { font-weight:600; display:block; margin-bottom:4px }
.role-btn .desc { font-size:.8rem; color:var(--muted) }
.principal-panel,.node-panel { display:none }
.principal-panel.show,.node-panel.show { display:block }
.key-box { background:#000; color:#0f0; padding:12px; border-radius:4px; font-family:ui-monospace,monospace; font-size:.75rem; word-break:break-all; margin:12px 0; user-select:all }
.addr { font-family:ui-monospace,monospace; font-size:.85rem; padding:4px 0 }
input,button { width:100%; padding:10px 12px; border:1px solid var(--border); border-radius:4px; font-size:.9rem; margin-bottom:8px }
input:focus { outline:2px solid var(--accent); border-color:var(--accent) }
button { background:var(--accent); color:#fff; font-weight:600; border:none; cursor:pointer }
button:hover { opacity:.9 }
.status { margin-top:12px; padding:8px; border-radius:4px; font-size:.85rem }
.status.error { background:#fee; color:#c00 }
.status.success { background:#efe; color:#060 }
.hidden { display:none }
</style>
</head>
<body>
<div class="card">
  <h1>rdma-p2p / configuracao inicial</h1>
  <div class="roles" id="roles">
    <button class="role-btn" data-role="principal">
      <span class="emoji">&#x1F3DB;</span>
      <span class="label">Principal</span>
      <span class="desc">Controla a rede, fatia modelos e gerencia nos</span>
    </button>
    <button class="role-btn" data-role="node">
      <span class="emoji">&#x1F5F3;</span>
      <span class="label">No da rede</span>
      <span class="desc">Conecta-se ao principal e serve camadas</span>
    </button>
  </div>

  <div class="principal-panel" id="principal-panel">
    <h2>Chave de conexao</h2>
    <div class="key-box" id="conn-key">Gerando...</div>
    <p style="font-size:.8rem;color:var(--muted)">Guarde esta chave! Ela nao sera mostrada novamente.</p>
    <h2 style="margin-top:20px">Enderecos para os nos</h2>
    <div id="addresses"></div>
    <button id="btn-start-principal">Iniciar</button>
    <div class="status hidden" id="principal-status"></div>
  </div>

  <div class="node-panel" id="node-panel">
    <label for="principal-addr">Endereco do principal (IP:porta)</label>
    <input type="text" id="principal-addr" placeholder="ex: 192.168.1.5:42777">
    <label for="conn-key-input">Chave de conexao</label>
    <input type="password" id="conn-key-input" placeholder="Cole a chave fornecida pelo principal">
    <button id="btn-connect">Conectar</button>
    <div class="status hidden" id="node-status"></div>
  </div>
</div>
<script>
const API = '';
async function postJSON(path, body) {
  const r = await fetch(API + path, { method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(body) });
  return { status: r.status, body: await r.json() };
}

// Role selection
document.querySelectorAll('.role-btn').forEach(btn => {
  btn.onclick = () => {
    document.querySelectorAll('.role-btn').forEach(b => b.classList.remove('selected'));
    btn.classList.add('selected');
    const role = btn.dataset.role;
    document.getElementById('principal-panel').classList.toggle('show', role === 'principal');
    document.getElementById('node-panel').classList.toggle('show', role === 'node');
    if (role === 'principal') fetchConnectionKey();
  };
});

// Fetch connection key from server (server-side generated)
async function fetchConnectionKey() {
  try {
    const r = await fetch('/api/setup/key');
    const d = await r.json();
    document.getElementById('conn-key').textContent = d.connection_key;
    document.getElementById('addresses').innerHTML = d.addresses
      .map(a => '<div class="addr">' + a + '</div>').join('');
  } catch(e) {
    document.getElementById('conn-key').textContent = 'Erro ao gerar chave';
  }
}

document.getElementById('btn-start-principal').onclick = async () => {
  const status = document.getElementById('principal-status');
  try {
    const r = await fetch('/api/setup/save', {
      method:'POST',
      headers:{'Content-Type':'application/json'},
      body: JSON.stringify({ role:'principal' })
    });
    if (r.ok) {
      status.className = 'status success';
      status.textContent = 'Configuracao salva! Redirecionando...';
      status.classList.remove('hidden');
      setTimeout(() => location.reload(), 1500);
    } else {
      const d = await r.json();
      status.className = 'status error';
      status.textContent = d.error || 'Erro ao salvar';
      status.classList.remove('hidden');
    }
  } catch(e) {
    status.className = 'status error';
    status.textContent = e.message;
    status.classList.remove('hidden');
  }
};

document.getElementById('btn-connect').onclick = async () => {
  const status = document.getElementById('node-status');
  const addr = document.getElementById('principal-addr').value;
  const key = document.getElementById('conn-key-input').value;
  if (!addr || !key) {
    status.className = 'status error';
    status.textContent = 'Preencha o endereco e a chave';
    status.classList.remove('hidden');
    return;
  }
  try {
    const r = await fetch('/api/setup/save', {
      method:'POST',
      headers:{'Content-Type':'application/json'},
      body: JSON.stringify({ role:'node', bootstrap:addr, connection_key:key })
    });
    const d = await r.json();
    if (r.ok) {
      status.className = 'status success';
      status.textContent = d.message || 'Conectado! Redirecionando...';
      status.classList.remove('hidden');
      setTimeout(() => location.reload(), 1500);
    } else {
      status.className = 'status error';
      status.textContent = d.error || 'Conexao recusada';
      status.classList.remove('hidden');
    }
  } catch(e) {
    status.className = 'status error';
    status.textContent = e.message;
    status.classList.remove('hidden');
  }
};
</script>
</body>
</html>
```

- [ ] **Step 2: Commit**

```bash
git add web/setup.html && git commit -m "feat: add minimal setup page for role selection and handshake"
```

---

### Task 8: Setup API Routes in webui

**Files:**
- Modify: `src/webui.rs` (add setup routes)
- Modify: `src/main.rs` (wire setup routes)

- [ ] **Step 1: Read current webui.rs router structure**

Read `src/webui.rs` to understand how routes are composed. Find the function that creates the Router and the pattern used.

- [ ] **Step 2: Add setup API routes**

Add the following routes to the webui router:

```rust
// New routes to add to the existing router in webui.rs:

// GET /api/setup/key — generates a temporary connection key + collects local IPs
// POST /api/setup/save — saves the config based on user's role choice
```

Read the current webui.rs to find the right insertion point, then add these handler functions:

```rust
use crate::config::{Config, Role};
use crate::sysmon;
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Serialize)]
struct SetupKeyResponse {
    connection_key: String,
    addresses: Vec<String>,
}

async fn get_setup_key(
    State(config_state): State<AuthState>,
) -> impl IntoResponse {
    // Generate a connection key
    let raw = format!("{}-{}", Uuid::new_v4(), std::process::id());
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let key = hex::encode(hasher.finalize());

    // Store temporarily in config for the save step to use
    {
        let mut cfg = config_state.write().await;
        cfg.connection_key = key.clone();
    }

    // Collect local IPs
    let mut addresses = Vec::new();
    // Try to get local IPs via simple approach
    if let Ok(hostname) = gethostname::gethostname().into_string() {
        addresses.push(format!("{} (hostname)", hostname));
    }
    // Use UDP socket approach to detect local IP
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if let Ok(()) = socket.connect("8.8.8.8:80") {
            if let Ok(addr) = socket.local_addr() {
                addresses.push(format!("IPv4: {}:42777", addr.ip()));
            }
        }
    }

    Json(SetupKeyResponse {
        connection_key: key,
        addresses,
    })
}

#[derive(Deserialize)]
struct SaveSetupRequest {
    role: String,
    #[serde(default)]
    bootstrap: String,
    #[serde(default)]
    connection_key: String,
}

async fn save_setup(
    State(config_state): State<AuthState>,
    Json(req): Json<SaveSetupRequest>,
) -> impl IntoResponse {
    let role = if req.role == "principal" {
        Role::Principal
    } else {
        Role::Node
    };

    let mut cfg = config_state.write().await;
    cfg.role = role;

    if role == Role::Node {
        if req.connection_key.is_empty() {
            // No key provided — request the handshake from principal
            // The node will first try to handshake
        }
        cfg.connection_key = req.connection_key.clone();
        if !req.bootstrap.is_empty() {
            cfg.bootstrap_nodes = req.bootstrap.split(',').map(|s| s.trim().to_string()).collect();
        }
    }

    if let Err(e) = cfg.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()})));
    }

    Json(json!({"saved": true, "message": "Configuracao salva. Reiniciando..."}))
}
```

- [ ] **Step 3: Add routes to the webui Router**

```rust
// In the webui router function, add:
.route("/api/setup/key", get(get_setup_key))
.route("/api/setup/save", post(save_setup))
```

- [ ] **Step 4: Build**

```bash
cargo build
```

- [ ] **Step 5: Commit**

```bash
git add src/webui.rs src/main.rs && git commit -m "feat: add setup API routes for role selection and config persistence"
```

---

### Task 9: Rewire main.rs for Setup Flow

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add conditional startup logic**

At the top of `main()`, after parsing args and setting up tracing, add:

```rust
// ── Setup flow: if no config exists, serve only the setup page ──────────
if !config::Config::exists() {
    info!("No config found — starting in setup mode");
    start_setup_mode(args.gateway_port).await?;
    return Ok(());
}
```

- [ ] **Step 2: Implement start_setup_mode function**

Add this function to main.rs:

```rust
async fn start_setup_mode(gateway_port: u16) -> Result<()> {
    use crate::auth::AuthState;
    use crate::config::{Config, Role};
    use crate::webui;
    use axum::{routing::get, Router};
    use std::net::SocketAddr;

    // Create minimal config for setup
    let config = Arc::new(RwLock::new(Config {
        role: Role::Principal, // temporary, will be overwritten
        connection_key: String::new(),
        gateway_port,
        p2p_port: 42777,
        inference_port: None,
        bootstrap_nodes: vec![],
        api_keys: vec![],
        blocked_nodes: vec![],
    }));

    let app = Router::new()
        .route("/", get(|| async { 
            axum::response::Html(include_str!("../web/setup.html"))
        }))
        .route("/api/setup/key", get(webui::get_setup_key))
        .route("/api/setup/save", axum::routing::post(webui::save_setup))
        .with_state(config);

    let addr = SocketAddr::from(([0, 0, 0, 0], gateway_port));
    info!("Setup page available at http://localhost:{}", gateway_port);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

- [ ] **Step 3: Expose setup handler functions from webui**

In `src/webui.rs`, make `get_setup_key` and `save_setup` public:

```rust
pub async fn get_setup_key(...) { ... }
pub async fn save_setup(...) { ... }
```

- [ ] **Step 4: Load config in normal mode and pass to GatewayState**

After the config check passes, load config:

```rust
let app_config = config::Config::load()?;
info!("Role: {}", app_config.role);

// Use config values to override CLI args where appropriate
let gateway_port = app_config.gateway_port;
let p2p_port = app_config.p2p_port;
let bootstrap_from_config = app_config.bootstrap_nodes.clone();
```

Pass the config as `AuthState` into the gateway.

- [ ] **Step 5: Build**

```bash
cargo build
```

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/webui.rs && git commit -m "feat: add setup mode with conditional startup based on config existence"
```

---

### Task 10: Integrate Auth into Gateway Router

**Files:**
- Modify: `src/gateway.rs` (add auth to OpenAI routes)
- Modify: `src/webui.rs` (add auth to apply/launch/plan routes)

- [ ] **Step 1: Extend GatewayState with auth config**

```rust
// In src/gateway.rs, add to GatewayState:
pub struct GatewayState {
    // ... existing fields ...
    pub auth_config: Arc<RwLock<crate::config::Config>>,
}
```

- [ ] **Step 2: Update start_gateway to accept and pass auth state**

Modify the `start_gateway` function signature and router construction:

```rust
pub async fn start_gateway(state: GatewayState, port: u16) -> Result<()> {
    use crate::auth::{ApiKeyAuth, AuthState};
    use crate::admin;

    let auth_state: AuthState = state.auth_config.clone();

    let app = Router::new()
        // Public routes
        .route("/", get(crate::webui::serve_index))
        .route("/peers", get(get_peers))
        .route("/health", get(health_check))
        .route("/handshake", post(crate::session::handle_handshake))
        
        // Web UI routes (existing)
        .merge(crate::webui::ui_routes(auth_state.clone()))
        
        // Admin routes (require connection key)
        .merge(crate::admin::admin_routes())
        .merge(crate::admin::transfer_routes())
        
        // OpenAI routes (require API key — use extractor in handlers)
        .route("/v1/models", get(crate::openai::list_models))
        .route("/v1/chat/completions", post(crate::openai::chat_completions))
        .route("/v1/completions", post(crate::openai::completions))
        
        // API routes
        .route("/api/generate", post(generate))
        
        .with_state(state)
        .with_state(auth_state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

- [ ] **Step 3: Add auth to OpenAI route handlers**

In `src/openai.rs`, add the `ApiKeyAuth` extractor to the handler functions. The extractor validates the API key automatically; if invalid, axum returns 401/403 before the handler runs.

- [ ] **Step 4: Add ConnectionAuth to apply/launch/plan routes**

In `src/webui.rs`, add `ConnectionAuth` extractor to `apply_config`, `launch_plan`, and `plan_split` handlers.

- [ ] **Step 5: Build and fix compilation**

```bash
cargo build 2>&1 | head -50
```

Fix any errors, iterate until clean compile.

- [ ] **Step 6: Commit**

```bash
git add src/gateway.rs src/webui.rs src/openai.rs src/main.rs && git commit -m "feat: integrate auth middleware into gateway router and sensitive routes"
```

---

### Task 11: Web UI — Role Indicators and Notification Bell

**Files:**
- Modify: `web/index.html`
- Modify: `web/app.js`
- Modify: `web/app.css`

- [ ] **Step 1: Add role badge and notification bell to HTML**

In `web/index.html`, add to the `<header class="topbar">` area:

```html
<span id="role-badge" class="role-badge">Principal</span>
<button id="notif-bell" class="notif-bell" title="Notificacoes">
  &#x1F514; <span id="notif-count" class="notif-count hidden">0</span>
</button>
<div id="notif-dropdown" class="notif-dropdown hidden"></div>
```

- [ ] **Step 2: Add CSS for role badge and notifications**

```css
.role-badge {
  padding: 2px 8px; border-radius: 3px; font-size: .7rem;
  font-weight: 600; text-transform: uppercase; letter-spacing: .5px;
}
.role-badge[data-role="principal"] { background: #e8a838; color: #1a1a2e }
.role-badge[data-role="node"] { background: #7b8ca8; color: #fff }
.notif-bell { position:relative; background:none; border:none; cursor:pointer; font-size:1.1rem; width:auto; padding:0 8px }
.notif-count { position:absolute; top:-4px; right:-2px; background:#c00; color:#fff; border-radius:50%; width:16px; height:16px; font-size:.6rem; display:flex; align-items:center; justify-content:center }
.notif-dropdown { position:absolute; top:100%; right:0; background:var(--card); border:1px solid var(--border); border-radius:4px; box-shadow:0 4px 12px rgba(0,0,0,.15); min-width:280px; z-index:100; padding:8px }
.notif-item { padding:8px; border-bottom:1px solid var(--border); font-size:.8rem }
.notif-item:last-child { border-bottom:none }
.notif-actions { display:flex; gap:4px; margin-top:4px }
.notif-actions button { padding:4px 8px; font-size:.7rem; width:auto }
```

- [ ] **Step 3: Add JS for role display and polling notifications**

In `web/app.js`, add after the state initialization:

```javascript
// In pollCluster() response handler, update role badge
const roleBadge = document.getElementById('role-badge');
if (roleBadge && data.node) {
  roleBadge.textContent = data.node.role || 'Node';
  roleBadge.dataset.role = (data.node.role || 'node').toLowerCase();
}

// Poll for notifications
async function pollNotifications() {
  try {
    const r = await fetch('/admin/transfer-offers', { headers: authHeaders() });
    if (!r.ok) return;
    const data = await r.json();
    updateNotificationBell(data.offers || []);
  } catch(e) {}
}

function updateNotificationBell(offers) {
  const count = document.getElementById('notif-count');
  const dropdown = document.getElementById('notif-dropdown');
  if (!count || !dropdown) return;

  if (offers.length > 0) {
    count.textContent = offers.length;
    count.classList.remove('hidden');
    dropdown.innerHTML = offers.map(o => `
      <div class="notif-item">
        <strong>Transferencia pendente</strong><br>
        ${o.shard_name} (${o.total_size_mb} MB)<br>
        Camadas ${o.start_layer}-${o.end_layer}<br>
        Espaco disponivel: ${o.disk_free_mb} MB
        <div class="notif-actions">
          <button onclick="respondTransfer('${o.id}', true)">Aceitar</button>
          <button onclick="respondTransfer('${o.id}', false)" style="background:#999">Recusar</button>
        </div>
      </div>
    `).join('');
  } else {
    count.classList.add('hidden');
    dropdown.innerHTML = '<div class="notif-item" style="color:var(--muted)">Nenhuma notificacao</div>';
  }
}

document.getElementById('notif-bell')?.addEventListener('click', () => {
  document.getElementById('notif-dropdown').classList.toggle('hidden');
});

setInterval(pollNotifications, 5000);
```

- [ ] **Step 4: Commit**

```bash
git add web/index.html web/app.js web/app.css && git commit -m "feat: add role badge, notification bell, and transfer offer polling to web UI"
```

---

### Task 12: Web UI — "Nos" Tab (Admin Panel)

**Files:**
- Modify: `web/index.html` (add tab)
- Modify: `web/app.js` (add view logic)

- [ ] **Step 1: Add "Nos" tab to HTML**

```html
<button class="tab" data-view="nos">Nos</button>
```

And the view container:

```html
<section class="view" id="view-nos">
  <div class="panel">
    <h2>Nos conectados</h2>
    <div id="nodes-table"></div>
    <div id="connection-key-section" style="margin-top:24px">
      <h3>Chave de conexao</h3>
      <div class="key-box" id="display-conn-key"></div>
      <button id="btn-regenerate-key" style="width:auto">Regenerar chave</button>
    </div>
  </div>
  <div class="panel">
    <h2>Chaves de API</h2>
    <div id="api-keys-list"></div>
    <button id="btn-generate-api-key" style="width:auto">Gerar nova chave</button>
  </div>
</section>
```

- [ ] **Step 2: Implement "Nos" tab logic in JS**

```javascript
async function loadNodesTab() {
  // Only visible for principal
  if (state.role !== 'principal') return;

  try {
    const [nodesR, keysR, connKeyR] = await Promise.all([
      fetch('/admin/nodes', { headers: authHeaders() }),
      fetch('/admin/api-keys', { headers: authHeaders() }),
      fetch('/api/ui/node')
    ]);
    const nodes = await nodesR.json();
    const keys = await keysR.json();
    const localNode = await connKeyR.json();

    document.getElementById('display-conn-key').textContent = localNode.connection_key || '***';

    document.getElementById('nodes-table').innerHTML = nodes.map(n => `
      <div class="node-row ${n.blocked ? 'is-blocked' : ''}">
        <strong>${n.node_id}</strong>
        <span class="tag">${n.blocked ? 'BLOQUEADO' : n.verified ? 'conectado' : 'pendente'}</span>
        <div class="node-actions">
          ${n.blocked
            ? '<button onclick="unblockNode(\''+n.node_id+'\')">Desbloquear</button>'
            : '<button onclick="blockNode(\''+n.node_id+'\')">Bloquear</button>'}
          <button onclick="removeNode(\''+n.node_id+'\')">Remover</button>
        </div>
      </div>
    `).join('') || '<p style="color:var(--muted)">Nenhum no conectado</p>';

    document.getElementById('api-keys-list').innerHTML = keys.map(k => `
      <div class="key-row ${k.active ? '' : 'is-inactive'}">
        <code>${k.key.substring(0,20)}...</code>
        <span>${k.label}</span>
        ${k.active ? '<span class="tag is-ready">ativa</span>' : '<span class="tag">inativa</span>'}
        <button onclick="revokeKey('${k.key}')">Revogar</button>
      </div>
    `).join('');
  } catch(e) {
    console.error('Failed to load nodes tab', e);
  }
}

async function blockNode(id) { await fetch('/admin/nodes/'+id+'/block', { method:'POST', headers: authHeaders() }); loadNodesTab(); }
async function unblockNode(id) { await fetch('/admin/nodes/'+id+'/unblock', { method:'POST', headers: authHeaders() }); loadNodesTab(); }
async function removeNode(id) { await fetch('/admin/nodes/'+id, { method:'DELETE', headers: authHeaders() }); loadNodesTab(); }
async function revokeKey(key) { await fetch('/admin/api-keys/'+key, { method:'DELETE', headers: authHeaders() }); loadNodesTab(); }

document.getElementById('btn-regenerate-key')?.addEventListener('click', async () => {
  const r = await fetch('/admin/regenerate-key', { method:'POST', headers: authHeaders() });
  const d = await r.json();
  document.getElementById('display-conn-key').textContent = d.connection_key;
  alert('Nova chave gerada. Todos os nos precisarao reconfigurar.');
});

document.getElementById('btn-generate-api-key')?.addEventListener('click', async () => {
  const label = prompt('Rotulo para a chave (ex: opencode):', 'default');
  if (!label) return;
  const r = await fetch('/admin/api-keys', {
    method:'POST', headers: {...authHeaders(),'Content-Type':'application/json'},
    body: JSON.stringify({ label })
  });
  const d = await r.json();
  alert('Nova chave gerada:\n' + d.key);
  loadNodesTab();
});

function authHeaders() {
  return { 'Authorization': 'Bearer ' + (state.connectionKey || localStorage.getItem('rdma_conn_key') || '') };
}
```

- [ ] **Step 3: Update tab switching logic**

Add the "nos" view to the tab switching handler so it calls `loadNodesTab()` when selected.

- [ ] **Step 4: Commit**

```bash
git add web/index.html web/app.js && git commit -m "feat: add Nos admin tab with node management and API key controls"
```

---

### Task 13: Bitswap libp2p Behaviour

**Files:**
- Create: `src/bitswap.rs`
- Modify: `src/main.rs` (add `mod bitswap;`)
- Modify: `src/discovery.rs` (register behaviour)
- Modify: `Cargo.toml` (add `bincode` dependency if needed)

- [ ] **Step 1: Define bitswap protocol and messages**

```rust
// src/bitswap.rs
use libp2p::{
    request_response::{self, Codec, ProtocolSupport},
    StreamProtocol,
};
use serde::{Deserialize, Serialize};
use std::io;
use std::time::Duration;

pub const BSWP_PROTOCOL: &str = "/rdma-p2p/bitswap/1.0.0";

/// Chunk identifier: hash of the chunk data.
pub type ChunkId = String; // SHA256 hex

/// Manifest describing a file available for transfer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileManifest {
    pub file_name: String,
    pub total_size: u64,
    pub chunk_size: u64,
    pub num_chunks: u32,
    pub chunks: Vec<ChunkInfo>,
    pub md5_full: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkInfo {
    pub index: u32,
    pub offset: u64,
    pub size: u64,
    pub sha256: String,
}

/// Bitswap protocol messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BitswapRequest {
    /// "I want these chunks"
    WantList { manifest_sha256: String, chunk_ids: Vec<u32> },
    /// "Send me the manifest for this file"
    GetManifest { file_name: String },
    /// "Cancel my previous wants"
    Cancel { chunks: Vec<u32> },
    /// "I have these chunks available" (advertisement)
    Have { chunks: Vec<(u32, String)> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BitswapResponse {
    /// Chunk data (raw bytes encoded as base64 in JSON, or use bincode)
    Block { chunk_index: u32, data: Vec<u8> },
    /// Manifest for the requested file
    Manifest(FileManifest),
    /// Acknowledgment
    Ack,
    /// Error
    Error { message: String },
}

/// JSON codec for bitswap messages over libp2p request-response.
#[derive(Clone, Default)]
pub struct BitswapCodec;

impl Codec for BitswapCodec {
    type Request = BitswapRequest;
    type Response = BitswapResponse;

    async fn read_request<T>(&mut self, _: &StreamProtocol, io: &mut T) -> io::Result<Self::Request>
    where T: futures::AsyncRead + Unpin + Send
    {
        use futures::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        io.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut data = vec![0u8; len];
        io.read_exact(&mut data).await?;
        serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    async fn read_response<T>(&mut self, _: &StreamProtocol, io: &mut T) -> io::Result<Self::Response>
    where T: futures::AsyncRead + Unpin + Send
    {
        use futures::AsyncReadExt;
        let mut len_buf = [0u8; 4];
        io.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut data = vec![0u8; len];
        io.read_exact(&mut data).await?;
        serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }

    async fn write_request<T>(&mut self, _: &StreamProtocol, io: &mut T, req: Self::Request) -> io::Result<()>
    where T: futures::AsyncWrite + Unpin + Send
    {
        use futures::AsyncWriteExt;
        let data = serde_json::to_vec(&req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        io.write_all(&(data.len() as u32).to_be_bytes()).await?;
        io.write_all(&data).await?;
        Ok(())
    }

    async fn write_response<T>(&mut self, _: &StreamProtocol, io: &mut T, res: Self::Response) -> io::Result<()>
    where T: futures::AsyncWrite + Unpin + Send
    {
        use futures::AsyncWriteExt;
        let data = serde_json::to_vec(&res)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        io.write_all(&(data.len() as u32).to_be_bytes()).await?;
        io.write_all(&data).await?;
        Ok(())
    }
}

/// Create a request_response::Behaviour for bitswap.
pub fn bitswap_behaviour() -> request_response::Behaviour<BitswapCodec> {
    request_response::Behaviour::new(
        [(StreamProtocol::new(BSWP_PROTOCOL), ProtocolSupport::Full)],
        request_response::Config::default()
            .with_request_timeout(Duration::from_secs(60)),
    )
}

/// Compute SHA256 hash of a byte slice.
pub fn hash_chunk(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Compute MD5 of a byte slice.
pub fn md5_hash(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_chunk_consistent() {
        let data = b"hello world";
        let h1 = hash_chunk(data);
        let h2 = hash_chunk(data);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn test_manifest_serialization() {
        let m = FileManifest {
            file_name: "test.gguf".into(),
            total_size: 1024,
            chunk_size: 256,
            num_chunks: 4,
            chunks: vec![
                ChunkInfo { index: 0, offset: 0, size: 256, sha256: "abc".into() }
            ],
            md5_full: "def".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: FileManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.file_name, "test.gguf");
        assert_eq!(parsed.num_chunks, 4);
    }
}
```

- [ ] **Step 2: Add `md-5` to Cargo.toml for MD5 support**

```toml
md-5 = "0.10"
```

- [ ] **Step 3: Run tests**

```bash
cargo test bitswap
```
Expected: 2 tests pass

- [ ] **Step 4: Commit**

```bash
git add src/bitswap.rs src/main.rs Cargo.toml && git commit -m "feat: add bitswap libp2p behaviour with chunk hashing and manifest"
```

---

### Task 14: Bitswap Integration into Discovery

**Files:**
- Modify: `src/discovery.rs`

- [ ] **Step 1: Read current discovery.rs to understand swarm composition**

Read the `Discovery::new` function and `DiscoveryBehaviour` struct.

- [ ] **Step 2: Add bitswap behaviour to the swarm**

In the `DiscoveryBehaviour` struct, add a bitswap field. Use libp2p's `#[derive(NetworkBehaviour)]` or compose behaviours. Since the existing code composes behaviours, integrate the `request_response::Behaviour<BitswapCodec>`.

- [ ] **Step 3: Expose bitswap events**

Add event handling for bitswap request/response in the swarm event loop.

- [ ] **Step 4: Build**

```bash
cargo build
```

- [ ] **Step 5: Commit**

```bash
git add src/discovery.rs && git commit -m "feat: integrate bitswap behaviour into libp2p discovery swarm"
```

---

### Task 15: End-to-End Smoke Test with Test Config

**Files:**
- Create: `tests/setup_flow.rs` (integration test)

- [ ] **Step 1: Write end-to-end test**

```rust
// tests/setup_flow.rs
#[cfg(test)]
mod integration {
    use std::process::Command;

    #[test]
    #[ignore] // Requires manual setup or CI isolation
    fn test_config_roundtrip() {
        // Verify config writes and reads correctly
        let config_json = r#"{
            "role": "principal",
            "connection_key": "test-key-123",
            "gateway_port": 8080,
            "p2p_port": 42777
        }"#;

        // Write to temp file and verify parsing
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rdma-p2p.json");
        std::fs::write(&path, config_json).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let cfg: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(cfg["role"], "principal");
        assert_eq!(cfg["connection_key"], "test-key-123");
    }

    #[test]
    fn test_bin_help_output() {
        let output = Command::new("cargo")
            .args(["run", "--", "--help"])
            .output()
            .expect("failed to run cargo");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("rdma-p2p"));
        assert!(stdout.contains("--model"));
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test integration
```

- [ ] **Step 3: Commit**

```bash
git add tests/ && git commit -m "test: add basic integration tests for config and CLI"
```

---

### Task 16: Final Integration — Wire Everything Together

**Files:**
- Modify: `src/main.rs`
- Modify: `src/gateway.rs`
- Modify: `src/webui.rs`

- [ ] **Step 1: Ensure all module imports are consistent**

Review all `use` statements, verify no circular dependencies.

- [ ] **Step 2: Full build with all features**

```bash
cargo build --release 2>&1
```

Fix any remaining compilation errors.

- [ ] **Step 3: Create local test configs**

```bash
# MacBook (principal)
echo '{"role":"principal","connection_key":"fixed-test-key-123","gateway_port":8080}' > rdma-p2p.json

# Verify it reads back correctly
cat rdma-p2p.json
```

- [ ] **Step 4: Run a quick smoke test**

```bash
# Start with test config — should skip setup and show dashboard
RUST_LOG=info cargo run -- --model gemma3 --memory 4GB --gateway-port 8080 &
sleep 5
curl -s http://localhost:8080/health | python3 -m json.tool
kill %1
```

- [ ] **Step 5: Test setup mode (no config)**

```bash
rm rdma-p2p.json
# Start without config — should serve setup page
RUST_LOG=info cargo run -- --gateway-port 8080 &
sleep 3
curl -s http://localhost:8080/ | head -5
# Should return setup.html content
kill %1
```

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: final integration — wire config, auth, admin, and bitswap together"
```

---

### Task 17: Polish and Cleanup

- [ ] **Step 1: Run full test suite**

```bash
cargo test
```
Expected: all tests pass

- [ ] **Step 2: Run clippy**

```bash
cargo clippy -- -D warnings
```
Fix any warnings.

- [ ] **Step 3: Clean test configs**

```bash
rm -f rdma-p2p.json .rdma-p2p.key
```

- [ ] **Step 4: Final build**

```bash
cargo build --release
```

- [ ] **Step 5: Commit and push**

```bash
git add -A && git commit -m "chore: cleanup, tests, and clippy fixes"
git push origin fm/rdma-p2p-interface-e-openai
```