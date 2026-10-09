//! KV cache kept by this machine: the snapshot directory on disk
//! (`slot_save_path`, `~/.pom/kv-slots` by default) and the host-memory cache
//! of the inference server (`--cache-ram`). The dashboard shows how much disk
//! the directory uses on each machine's card and lets the administrator empty
//! both.

use super::*;

/// What the snapshot directory of this machine holds, published in
/// `NodeInfo.kv_cache`. The directory is flat: every model and every slice
/// this machine ever served writes here, each snapshot identified by its own
/// sidecar (`model_label`, layer range, `n_embd`).
#[derive(Debug, Default, Clone, Serialize)]
pub struct KvCacheDisk {
    /// `false` when `slot_save_path` is turned off on this machine.
    pub enabled: bool,
    pub path: Option<String>,
    /// Snapshots (`.bin` files).
    pub snapshots: usize,
    /// Bytes of everything the directory holds for the cache: snapshots,
    /// sidecars and the automatic prefix index.
    pub bytes: u64,
}

/// A cache file is a snapshot (`.bin`), its sidecar or the automatic prefix
/// index (`.json`). Anything else in the directory is not ours to count or
/// to delete.
fn is_cache_file(path: &std::path::Path) -> bool {
    path.is_file()
        && matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("bin" | "json")
        )
}

fn cache_files(dir: &std::path::Path) -> Vec<(std::path::PathBuf, u64)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_cache_file(path))
        .map(|path| {
            let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            (path, bytes)
        })
        .collect()
}

pub(super) fn kv_cache_disk(dir: Option<&std::path::Path>) -> KvCacheDisk {
    let Some(dir) = dir else {
        return KvCacheDisk::default();
    };
    let files = cache_files(dir);
    KvCacheDisk {
        enabled: true,
        path: Some(dir.display().to_string()),
        snapshots: files
            .iter()
            .filter(|(path, _)| path.extension().and_then(|e| e.to_str()) == Some("bin"))
            .count(),
        bytes: files.iter().map(|(_, bytes)| bytes).sum(),
    }
}

/// Deletes every cache file of the directory. Returns `(files, bytes)`
/// actually removed; a file that vanished meanwhile is not an error.
pub(super) fn clear_kv_cache_dir(dir: &std::path::Path) -> anyhow::Result<(usize, u64)> {
    let mut removed = (0usize, 0u64);
    for (path, bytes) in cache_files(dir) {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed.0 += 1;
                removed.1 += bytes;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => anyhow::bail!("delete {}: {error}", path.display()),
        }
    }
    Ok(removed)
}

#[derive(Debug, Default, Deserialize)]
pub struct ClearKvCacheRequest {
    /// Machine whose cache is emptied. Absent or this node's own id: local.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// `POST /api/ui/kv-cache/clear` - empties the KV cache of **one** machine:
/// the snapshot directory on disk, this gateway's automatic prefix index and
/// the host-memory cache of the inference server. Live conversations keep the
/// KV they hold in the model context; they only lose what was saved.
///
/// The panel talks to its own node, which forwards to the chosen machine:
/// only the owner of a disk deletes from it (same shape as `/api/ui/leave`).
pub(super) async fn clear_kv_cache(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    body: Option<Json<ClearKvCacheRequest>>,
) -> Response {
    let target = body.and_then(|Json(b)| b.node_id).unwrap_or_default();
    if !target.is_empty() && target != state.network.node_id {
        let Some(peer) = state
            .all_peers()
            .await
            .into_iter()
            .find(|peer| peer.node_id == target)
        else {
            return bad_request("unknown peer");
        };
        return match state
            .control_post_json(&peer, "/api/ui/kv-cache/clear", &json!({}), PEER_TIMEOUT)
            .await
        {
            Ok(response) if response.status().is_success() => {
                match response.json::<serde_json::Value>().await {
                    Ok(body) => Json(body).into_response(),
                    Err(error) => bad_request(format!("unreadable response from the machine: {error}")),
                }
            }
            Ok(response) => bad_request(format!("the machine refused: HTTP {}", response.status())),
            Err(error) => bad_request(format!("machine unreachable: {error}")),
        };
    }

    let dir = state.auth.read().await.slot_save_dir();
    let (files, bytes) = match dir {
        Some(dir) => match tokio::task::spawn_blocking(move || clear_kv_cache_dir(&dir)).await {
            Ok(Ok(removed)) => removed,
            Ok(Err(error)) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("{error:#}") })),
                )
                    .into_response()
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": error.to_string() })),
                )
                    .into_response()
            }
        },
        None => (0, 0),
    };
    // The index named snapshots that no longer exist on this machine.
    if let Some(index) = &state.prefix_disk {
        index.clear();
    }
    let ram = clear_inference_ram_cache(&state).await;
    info!(
        files,
        bytes,
        ram_entries = ram.0,
        ram_bytes = ram.1,
        "KV cache cleared from the interface"
    );
    Json(json!({
        "cleared": true,
        "node_id": state.network.node_id,
        "disk_files": files,
        "disk_bytes": bytes,
        "ram_entries": ram.0,
        "ram_bytes": ram.1,
    }))
    .into_response()
}

/// Empties the host-memory cache of the local inference server, when one is
/// running. `(entries, bytes)` released; zeros when there is no model loaded.
async fn clear_inference_ram_cache(state: &GatewayState) -> (u64, u64) {
    let Some(port) = state.network.inference_port else {
        return (0, 0);
    };
    let url = format!("http://127.0.0.1:{port}/v1/session/cache/clear");
    let request = state
        .peer_http
        .plain()
        .post(&url)
        .timeout(Duration::from_secs(30));
    let response = state
        .peer_http
        .authorize_inference_request_for_session(request, None)
        .await
        .send()
        .await;
    let Ok(response) = response else {
        return (0, 0);
    };
    if !response.status().is_success() {
        warn!(
            "the inference server refused to clear the RAM cache: HTTP {}",
            response.status()
        );
        return (0, 0);
    }
    let body = response
        .json::<serde_json::Value>()
        .await
        .unwrap_or_default();
    (
        body["entries"].as_u64().unwrap_or(0),
        body["bytes"].as_u64().unwrap_or(0),
    )
}

#[cfg(test)]
mod kv_cache_tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pom-kv-cache-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn usage_counts_snapshots_and_every_cache_byte() {
        let dir = temp_dir("usage");
        std::fs::write(dir.join("auto__a__n1.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("auto__a__n1.json"), vec![0u8; 10]).unwrap();
        std::fs::write(dir.join("prefix-auto-index.json"), vec![0u8; 5]).unwrap();
        std::fs::write(dir.join("notes.txt"), vec![0u8; 1000]).unwrap();

        let usage = kv_cache_disk(Some(&dir));
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(usage.enabled);
        assert_eq!((usage.snapshots, usage.bytes), (1, 115));
    }

    #[test]
    fn a_disabled_or_missing_directory_reports_nothing() {
        let off = kv_cache_disk(None);
        assert!(!off.enabled && off.path.is_none() && off.bytes == 0);
        let missing = std::env::temp_dir().join("pom-kv-cache-that-does-not-exist");
        let usage = kv_cache_disk(Some(&missing));
        assert!(usage.enabled);
        assert_eq!((usage.snapshots, usage.bytes), (0, 0));
    }

    #[test]
    fn clearing_removes_cache_files_and_leaves_everything_else() {
        let dir = temp_dir("clear");
        std::fs::write(dir.join("a.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("a.json"), vec![0u8; 10]).unwrap();
        std::fs::write(dir.join("prefix-auto-index.json"), vec![0u8; 5]).unwrap();
        std::fs::write(dir.join("notes.txt"), b"keep").unwrap();
        std::fs::create_dir(dir.join("sub.bin")).unwrap();

        let removed = clear_kv_cache_dir(&dir).unwrap();
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(removed, (3, 115));
        assert_eq!(left.len(), 2);
        assert!(left.contains(&"notes.txt".to_string()) && left.contains(&"sub.bin".to_string()));
    }
}
