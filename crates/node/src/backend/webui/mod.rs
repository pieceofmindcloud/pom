//! Web interface served by the peer itself, plus the control API behind it.
//!
//! The assets are NOT compiled into the binary: `webapp.rs` reads the built
//! bundle from disk at `config::ui_dist_dir()` (default
//! `<project_root>/apps/frontend/dist`, overridable with `POM_UI_DIST`),
//! so editing the frontend requires `npm run build` in `apps/frontend/` and
//! shipping `dist/` next to the binary.
//!
//! Every node serves it, so the UI can be opened on any peer. The cluster view
//! works by asking each peer's own `/api/ui/node`, which is why `PeerInfo`
//! carries `gateway_port`.

use crate::auth::{AuthState, ConnectionAuth};
use crate::catalog::{self, Catalog};
use crate::config::{AdminExposure, KvCacheType};
use crate::deploy::normalize_deployment_name;
use crate::gateway::GatewayState;
use crate::gguf;
use crate::network::PeerInfo;
use crate::plan::{self, PeerBudget, DEFAULT_RESERVE_PCT};
use crate::runtime::{NodeConfig, RuntimeStatus};
use crate::sysmon::{self, SystemLoad};
use crate::thermal::ThermalPolicy;
use anyhow::Context;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock, Semaphore, SemaphorePermit};
use tracing::{info, warn};

/// How long a scanned catalog is served before a refresh is started behind it.
const CATALOG_TTL: Duration = Duration::from_secs(20);
/// A cluster snapshot is cheap to reuse briefly, but never allowed to hide a
/// peer/runtime mutation for more than this window.
const CLUSTER_CACHE_TTL: Duration = Duration::from_secs(2);
/// Timeout for one peer-to-peer control call.
pub(crate) const PEER_TIMEOUT: Duration = Duration::from_secs(4);
/// Number of capacity previews this process may inspect at the same time.
///
/// The route only reads GGUF metadata and system memory, but it still uses
/// blocking file work. Rejecting excess requests immediately keeps a busy
/// browser from turning that work into an unbounded queue.
const CAPACITY_PREVIEW_MAX_IN_FLIGHT: usize = 4;
/// Maximum number of peer preview calls made by one deployment plan at once.
pub(crate) const CAPACITY_PREVIEW_MAX_PEER_REQUESTS: usize = 4;

static CAPACITY_PREVIEW_SLOTS: OnceLock<Semaphore> = OnceLock::new();

fn capacity_preview_slots() -> &'static Semaphore {
    CAPACITY_PREVIEW_SLOTS.get_or_init(|| Semaphore::new(CAPACITY_PREVIEW_MAX_IN_FLIGHT))
}

fn try_capacity_preview_slot() -> Option<SemaphorePermit<'static>> {
    capacity_preview_slots().try_acquire().ok()
}

/// A stable validator for JSON consumed by the browser. Callers must pass the
/// UI payload, not a larger server response containing sampling timestamps or
/// other transport-only noise.
pub(crate) fn etag_for_json<T: Serialize>(body: &T) -> String {
    let bytes = serde_json::to_vec(body).expect("JSON de estado da UI deve serializar");
    format!("\"{}\"", hex::encode(Sha256::digest(bytes)))
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|values| {
            values
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == "*" || candidate == etag)
        })
}

fn json_response_with_etag(
    headers: &HeaderMap,
    bytes: Vec<u8>,
    etag: &str,
    version: &str,
) -> Response {
    let not_modified = if_none_match(headers, etag);
    let mut response = if not_modified {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(bytes))
    };
    *response.status_mut() = if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(etag).expect("ETag de SHA256 é um header válido"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("x-state-version"),
        HeaderValue::from_str(version).expect("versão de estado é um header válido"),
    );
    if !not_modified {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    response
}

/// Return a conditional JSON response, keeping 304 bodies empty.
pub(crate) fn conditional_json<T: Serialize>(headers: &HeaderMap, body: &T) -> Response {
    let bytes = serde_json::to_vec(body).expect("JSON de estado da UI deve serializar");
    let etag = etag_for_json(body);
    json_response_with_etag(headers, bytes, &etag, &etag)
}

/// Same response helper with a monotonic snapshot version.
pub(crate) fn conditional_json_with_version<T: Serialize>(
    headers: &HeaderMap,
    body: &T,
    etag: &str,
    version: u64,
) -> Response {
    let bytes = serde_json::to_vec(body).expect("JSON de estado da UI deve serializar");
    json_response_with_etag(headers, bytes, etag, &version.to_string())
}

mod auto_discovery;
#[path = "catalog.rs"]
mod catalog_cache;
mod cluster;
mod jobs;
mod jobs_apply;
mod kv_cache;
mod lifecycle;
mod lobby;
mod network;
mod params;
mod routes;
mod setup;
mod sidecars;
pub(crate) use sidecars::load_links as load_sidecar_links;
mod slicing;
pub(crate) mod standby;
mod trust;

pub use auto_discovery::*;
pub use catalog_cache::*;
pub use cluster::*;
pub use jobs::*;
pub use jobs_apply::*;
pub use kv_cache::*;
pub use lifecycle::*;
pub use lobby::*;
pub use network::*;
pub use params::*;
pub use routes::*;
pub use setup::*;
pub use slicing::*;
pub use trust::*;

include!("tests/early.rs");
include!("tests/core_a.rs");
include!("tests/core_b.rs");
include!("tests/core_c.rs");
