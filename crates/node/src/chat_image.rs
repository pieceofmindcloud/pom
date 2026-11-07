//! Images from the Chat: the ComfyUI workflows the network offers, running one
//! on the machine that lists it, and keeping its images on the node that made
//! the request, in `<workspace>/.comfyui/images/<sha256>.<png|jpg>`, so they
//! survive the serving machine going away.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gateway::GatewayState;

/// At most this many images per request.
pub(crate) const MAX_IMAGES: usize = 16;
/// At most this many bytes per image.
pub(crate) const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// The request body, here and when it crosses to another machine: the prompt
/// and, for image to image, a base image (at most 16 MiB, base64).
pub(crate) const MAX_RUN_BODY: usize = 24 * 1024 * 1024;
/// A workflow on a cold diffusion model can take minutes.
const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// A workflow the Chat may run, and the machine that runs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Workflow {
    pub id: String,
    pub name: String,
    pub node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deploy_id: Option<String>,
    /// What the workflow takes: `prompt`, and `image` when it accepts a base image.
    #[serde(default)]
    pub inputs: Vec<String>,
}

/// The workflows the network's `image` services list. A built-in one is
/// offered only while its deployment serves, a published one while its
/// service is `ready`. One entry per id: this node's own when it lists it,
/// else the first. Built-ins first, then by name.
pub(crate) fn workflows_of(
    services_by_node: &[(String, Vec<Value>)],
    serving: &[String],
    local_node_id: &str,
) -> Vec<Workflow> {
    let mut found: Vec<Workflow> = Vec::new();
    for (node_id, services) in services_by_node {
        for service in services.iter().filter(|s| s["kind"] == "image") {
            let ready = service["status"] == "ready";
            let listed = service["workflows"].as_array().cloned().unwrap_or_default();
            for entry in listed {
                let Ok(workflow) =
                    serde_json::from_value::<crate::plugin_services::ImageWorkflow>(entry)
                else {
                    continue;
                };
                let offered = match &workflow.deploy_id {
                    Some(deploy_id) if workflow.id.starts_with("builtin:") => {
                        serving.contains(deploy_id)
                    }
                    _ => ready,
                };
                if !offered {
                    continue;
                }
                let candidate = Workflow {
                    id: workflow.id,
                    name: workflow.name,
                    node_id: node_id.clone(),
                    deploy_id: workflow.deploy_id,
                    inputs: workflow.inputs,
                };
                match found.iter_mut().find(|w| w.id == candidate.id) {
                    Some(existing) if candidate.node_id == local_node_id => *existing = candidate,
                    Some(_) => {}
                    None => found.push(candidate),
                }
            }
        }
    }
    found.sort_by(|a, b| {
        let builtin = |w: &Workflow| u8::from(!w.id.starts_with("builtin:"));
        builtin(a).cmp(&builtin(b)).then(a.name.cmp(&b.name))
    });
    found
}

/// The deployments whose every listed component has a ready replica.
pub(crate) fn serving_deployments(served: &[crate::components::NetworkComponent]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for component in served {
        if !ids.contains(&component.deploy_id) {
            ids.push(component.deploy_id.clone());
        }
    }
    ids.into_iter()
        .filter(|id| {
            let parts: Vec<_> = served.iter().filter(|c| &c.deploy_id == id).collect();
            parts.iter().any(|c| c.component == "diffusion")
                && parts.iter().all(|c| {
                    parts
                        .iter()
                        .any(|other| other.component == c.component && other.status == "ready")
                })
        })
        .collect()
}

/// Every machine's services: this node's and each live peer's.
async fn network_services(state: &GatewayState) -> Vec<(String, Vec<Value>)> {
    let mut nodes = vec![state.network.node_id.clone()];
    for peer in state.network.alive_peers().await {
        if !nodes.contains(&peer.node_id) {
            nodes.push(peer.node_id.clone());
        }
    }
    let lookups = nodes.into_iter().map(|node_id| async move {
        let services = crate::plugin_engine::services_of(state, &node_id)
            .await
            .unwrap_or_default();
        (node_id, services)
    });
    futures::future::join_all(lookups).await
}

/// The workflows the Chat offers now.
pub(crate) async fn network_workflows(state: &GatewayState) -> Vec<Workflow> {
    let services = network_services(state).await;
    let served: Vec<crate::components::NetworkComponent> = services
        .iter()
        .flat_map(|(node_id, list)| crate::components::components_of(node_id, node_id, list))
        .collect();
    workflows_of(
        &services,
        &serving_deployments(&served),
        &state.network.node_id,
    )
}

// ── Keeping the images ──────────────────────────────────────────────────────

/// `<root>/.comfyui/images`, the root being the node's workspace.
pub(crate) fn images_dir(root: &Path) -> PathBuf {
    root.join(".comfyui").join("images")
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct StoredImage {
    pub hash: String,
    pub ext: &'static str,
    pub width: u32,
    pub height: u32,
}

fn image_kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else {
        None
    }
}

/// Writes `bytes` as `<sha256>.<ext>` in `dir`; the same image is one file.
pub(crate) fn store_image(
    dir: &Path,
    bytes: &[u8],
    width: u32,
    height: u32,
) -> Result<StoredImage, String> {
    use sha2::Digest;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("an image is larger than 32 MiB".into());
    }
    let ext = image_kind(bytes).ok_or("not a PNG or JPEG image")?;
    let hash = hex::encode(sha2::Sha256::digest(bytes));
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{hash}.{ext}"));
    if !path.exists() {
        let partial = dir.join(format!(".{hash}.{ext}.part"));
        std::fs::write(&partial, bytes).map_err(|e| format!("cannot write the image: {e}"))?;
        std::fs::rename(&partial, &path).map_err(|e| format!("cannot write the image: {e}"))?;
    }
    Ok(StoredImage {
        hash,
        ext,
        width,
        height,
    })
}

/// The file of `hash` in `dir`, when `hash` is 64 lowercase hex digits.
pub(crate) fn find_image(
    dir: &Path,
    hash: &str,
) -> Result<Option<(PathBuf, &'static str)>, String> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("not an image hash".into());
    }
    for (ext, mime) in [("png", "image/png"), ("jpg", "image/jpeg")] {
        let path = dir.join(format!("{hash}.{ext}"));
        if path.is_file() {
            return Ok(Some((path, mime)));
        }
    }
    Ok(None)
}

/// Stores the images of the plugin's last line: `{"images": [{mime, width,
/// height, data}]}` -> `{"images": [{hash, ext, width, height}]}`. Nothing is
/// written when one image is refused or there are too many.
pub(crate) fn store_images(dir: &Path, line: &Value) -> Result<Value, String> {
    use base64::Engine;
    let images = line["images"].as_array().cloned().unwrap_or_default();
    if images.len() > MAX_IMAGES {
        return Err(format!(
            "the workflow made {} images; at most {MAX_IMAGES}",
            images.len()
        ));
    }
    let mut decoded = Vec::new();
    for image in &images {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(image["data"].as_str().unwrap_or_default())
            .map_err(|_| "an image is not valid base64".to_owned())?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err("an image is larger than 32 MiB".into());
        }
        if image_kind(&bytes).is_none() {
            return Err("not a PNG or JPEG image".into());
        }
        let width = image["width"].as_u64().unwrap_or(0) as u32;
        let height = image["height"].as_u64().unwrap_or(0) as u32;
        decoded.push((bytes, width, height));
    }
    let mut stored = Vec::new();
    for (bytes, width, height) in decoded {
        stored.push(store_image(dir, &bytes, width, height)?);
    }
    Ok(serde_json::json!({ "images": stored }))
}

// ── Running a workflow ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunRequest {
    pub workflow_id: String,
    pub node_id: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negative: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// A base image for image to image: a PNG or JPEG data URL (or plain base64).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// How far image to image moves from the base image, 0.05 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength: Option<f64>,
}

type LineStream = futures::stream::BoxStream<'static, Result<bytes::Bytes, String>>;

/// The plugin's run on this machine: its loopback server, behind its token.
async fn run_local(state: &GatewayState, body: Vec<u8>) -> Result<LineStream, String> {
    use futures::StreamExt;
    let code = crate::components::IMAGE_PLUGIN;
    let _ = state.plugins.open_on_demand(code).await;
    let upstream = state
        .plugins
        .query_audit(code, "ui.upstream", serde_json::json!({}))
        .await
        .ok_or("ComfyUI did not answer")?;
    let port = upstream["port"].as_u64().ok_or("ComfyUI is not running")?;
    let token = upstream["token"].as_str().unwrap_or_default();
    let response = state
        .peer_http
        .plain()
        .post(format!("http://127.0.0.1:{port}/pom/workflow/run"))
        .header("x-pom-plugin-token", token)
        .header("content-type", "application/json")
        .body(body)
        .timeout(RUN_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let reason = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        return Err(reason);
    }
    Ok(response
        .bytes_stream()
        .map(|r| r.map_err(|e| e.to_string()))
        .boxed())
}

/// The run on `node_id`: this node's plugin, or the signed internal route of
/// the machine that lists the workflow.
async fn run_on(state: &GatewayState, node_id: &str, body: Vec<u8>) -> Result<LineStream, String> {
    use futures::StreamExt;
    if node_id == state.network.node_id {
        return run_local(state, body).await;
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == node_id)
        .ok_or("the machine that runs this workflow left the network")?;
    let path = format!(
        "/api/internal/plugins/{}/workflow/run",
        crate::components::IMAGE_PLUGIN
    );
    let response = state
        .control_post_bytes(&peer, &path, body, RUN_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        return Err(body["error"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| status.to_string()));
    }
    Ok(response
        .bytes_stream()
        .map(|r| r.map_err(|e| e.to_string()))
        .boxed())
}

/// Reads the plugin's NDJSON, relays progress and stores the images: every
/// line out is one NDJSON line for the Chat.
pub(crate) async fn relay(
    mut lines: LineStream,
    dir: PathBuf,
    out: tokio::sync::mpsc::Sender<Value>,
) {
    use futures::StreamExt;
    let mut buffer: Vec<u8> = Vec::new();
    let mut finished = false;
    let handle = |line: &[u8], finished: &mut bool| -> Option<Value> {
        let value: Value = serde_json::from_slice(line).ok()?;
        if value.get("images").is_some() {
            *finished = true;
            return Some(match store_images(&dir, &value) {
                Ok(stored) => stored,
                Err(error) => serde_json::json!({ "error": error }),
            });
        }
        if value.get("error").is_some() {
            *finished = true;
        }
        Some(value)
    };
    while let Some(chunk) = lines.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let _ = out.send(serde_json::json!({ "error": error })).await;
                return;
            }
        };
        buffer.extend_from_slice(&chunk);
        if buffer.len() > MAX_IMAGES * MAX_IMAGE_BYTES * 2 {
            let _ = out
                .send(serde_json::json!({ "error": "the workflow answered too much" }))
                .await;
            return;
        }
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            if let Some(value) = handle(&line[..line.len() - 1], &mut finished) {
                let _ = out.send(value).await;
            }
            if finished {
                return;
            }
        }
    }
    if !buffer.is_empty() {
        if let Some(value) = handle(&buffer, &mut finished) {
            let _ = out.send(value).await;
        }
    }
    if !finished {
        let _ = out
            .send(serde_json::json!({ "error": "the workflow ended without images" }))
            .await;
    }
}

fn ndjson(rx: tokio::sync::mpsc::Receiver<Value>) -> axum::response::Response {
    use axum::response::IntoResponse;
    use futures::StreamExt;
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|value| (value, rx))
    })
    .map(|value| {
        let mut line = serde_json::to_vec(&value).unwrap_or_default();
        line.push(b'\n');
        Ok::<_, std::convert::Infallible>(bytes::Bytes::from(line))
    });
    (
        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

/// `POST /api/ui/chat/image`.
pub(crate) async fn run_route(
    _auth: crate::auth::ApiKeyAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::Json(request): axum::Json<RunRequest>,
) -> axum::response::Response {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let root = state.auth.read().await.workspace_root_path();
    let dir = images_dir(&root);
    let body = serde_json::to_vec(&request).unwrap_or_default();
    tokio::spawn(async move {
        match run_on(&state, &request.node_id, body).await {
            Ok(lines) => relay(lines, dir, tx).await,
            Err(error) => {
                let _ = tx.send(serde_json::json!({ "error": error })).await;
            }
        }
    });
    ndjson(rx)
}

/// `GET /api/ui/chat/image/:hash`.
pub(crate) async fn image_route(
    _auth: crate::auth::ApiKeyAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let root = state.auth.read().await.workspace_root_path();
    match find_image(&images_dir(&root), &hash) {
        Err(error) => (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "error": "this image is not on this node" })),
        )
            .into_response(),
        Ok(Some((path, mime))) => match tokio::fs::read(&path).await {
            Ok(bytes) => (
                [
                    (axum::http::header::CONTENT_TYPE, mime),
                    (
                        axum::http::header::CACHE_CONTROL,
                        "private, max-age=31536000, immutable",
                    ),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => StatusCode::NOT_FOUND.into_response(),
        },
    }
}

/// `POST /api/internal/plugins/:code/workflow/run`: a live member asks this
/// machine to run a workflow its ComfyUI lists; the NDJSON comes back as is.
pub(crate) async fn internal_run_route(
    auth: crate::auth::InternalNodeAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let refuse = |status: StatusCode, message: &str| {
        (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
    };
    let Some(principal) = auth.principal() else {
        return refuse(StatusCode::FORBIDDEN, "a signed peer call is required");
    };
    if !state
        .network
        .alive_peers()
        .await
        .iter()
        .any(|peer| peer.node_id == principal.node_id)
    {
        return refuse(StatusCode::FORBIDDEN, "the caller is not a live peer");
    }
    if code != crate::components::IMAGE_PLUGIN {
        return refuse(StatusCode::NOT_FOUND, "no such workflow service");
    }
    if body.len() > MAX_RUN_BODY {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "the request is too large");
    }
    match run_local(&state, body.to_vec()).await {
        Ok(lines) => (
            [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
            axum::body::Body::from_stream(lines),
        )
            .into_response(),
        Err(error) => refuse(StatusCode::BAD_GATEWAY, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest-of-png";
    const JPG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];

    #[test]
    fn a_base_image_travels_with_the_run() {
        let request: RunRequest = serde_json::from_value(serde_json::json!({
            "workflow_id": "builtin:img-1", "node_id": "n", "prompt": "a boat",
            "image": "data:image/png;base64,AAAA", "strength": 0.5
        }))
        .unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["image"], "data:image/png;base64,AAAA");
        assert_eq!(body["strength"], 0.5);
        let plain: RunRequest = serde_json::from_value(serde_json::json!({
            "workflow_id": "w", "node_id": "n", "prompt": "x"
        }))
        .unwrap();
        assert!(serde_json::to_value(&plain).unwrap().get("image").is_none());
        assert!(MAX_RUN_BODY >= 16 * 1024 * 1024 * 4 / 3 + 1024, "a 16 MiB image in base64 fits");
    }

    #[test]
    fn png_and_jpeg_get_their_extension() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(store_image(dir.path(), PNG, 1, 1).unwrap().ext, "png");
        let jpg = store_image(dir.path(), JPG, 2, 3).unwrap();
        assert_eq!((jpg.ext, jpg.width, jpg.height), ("jpg", 2, 3));
        assert!(dir.path().join(format!("{}.jpg", jpg.hash)).is_file());
    }

    #[test]
    fn other_bytes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            store_image(dir.path(), b"GIF89a", 1, 1).unwrap_err(),
            "not a PNG or JPEG image"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_oversized_image_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut big = PNG.to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        assert_eq!(
            store_image(dir.path(), &big, 1, 1).unwrap_err(),
            "an image is larger than 32 MiB"
        );
    }

    #[test]
    fn the_same_image_is_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = store_image(dir.path(), PNG, 1, 1).unwrap();
        let b = store_image(dir.path(), PNG, 1, 1).unwrap();
        assert_eq!(a.hash, b.hash);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn the_folder_is_under_dot_comfyui_images() {
        assert_eq!(images_dir(Path::new("/w")), Path::new("/w/.comfyui/images"));
    }

    #[test]
    fn a_bad_hash_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_image(dir.path(), "../etc/passwd").is_err());
        assert!(find_image(dir.path(), &"A".repeat(64)).is_err());
        assert_eq!(find_image(dir.path(), &"a".repeat(64)).unwrap(), None);
        let stored = store_image(dir.path(), PNG, 1, 1).unwrap();
        assert_eq!(
            find_image(dir.path(), &stored.hash).unwrap().unwrap().1,
            "image/png"
        );
    }

    fn image_service(status: &str, workflows: Value) -> Value {
        serde_json::json!({"kind": "image", "status": status, "workflows": workflows})
    }

    #[test]
    fn workflows_list_builtins_of_serving_deployments_first() {
        let services = vec![
            (
                "n1".to_owned(),
                vec![image_service(
                    "ready",
                    serde_json::json!([
                        {"id": "pub:sketch", "name": "A sketch", "inputs": ["prompt", "image"]},
                        {"id": "builtin:img-1", "name": "Z-Image", "deploy_id": "img-1", "inputs": ["prompt"]},
                        {"id": "builtin:img-2", "name": "Flux", "deploy_id": "img-2", "inputs": ["prompt"]},
                    ]),
                )],
            ),
            (
                "n2".to_owned(),
                vec![image_service(
                    "ready",
                    serde_json::json!([
                        {"id": "builtin:img-1", "name": "Z-Image", "deploy_id": "img-1"},
                    ]),
                )],
            ),
        ];
        let list = workflows_of(&services, &["img-1".to_owned()], "n2");
        let ids: Vec<_> = list.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["builtin:img-1", "pub:sketch"], "img-2 does not serve");
        assert_eq!(list[0].node_id, "n2", "this node runs it when it lists it");
        assert_eq!(list[1].inputs, ["prompt", "image"], "what a workflow takes is passed on");
    }

    #[test]
    fn a_published_workflow_needs_a_ready_service() {
        let services = vec![(
            "n1".to_owned(),
            vec![image_service(
                "starting",
                serde_json::json!([
                    {"id": "pub:sketch", "name": "A sketch"},
                ]),
            )],
        )];
        assert!(workflows_of(&services, &[], "n1").is_empty());
    }

    #[test]
    fn serving_needs_a_ready_part_per_component() {
        let part = |component: &str, status: &str| crate::components::NetworkComponent {
            deploy_id: "img-1".into(),
            name: "Z".into(),
            component: component.into(),
            files: vec![],
            status: status.into(),
            node_id: "n".into(),
            hostname: "n".into(),
            plugin_code: "comfyui".into(),
            in_flight: None,
        };
        assert_eq!(
            serving_deployments(&[part("diffusion", "ready"), part("vae", "ready")]),
            ["img-1"]
        );
        assert!(
            serving_deployments(&[part("diffusion", "loading"), part("vae", "ready")]).is_empty()
        );
        assert!(serving_deployments(&[part("vae", "ready")]).is_empty());
    }

    #[test]
    fn too_many_images_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(PNG);
        let images: Vec<Value> = (0..17)
            .map(|_| serde_json::json!({"mime": "image/png", "data": data}))
            .collect();
        assert!(store_images(dir.path(), &serde_json::json!({"images": images})).is_err());
        let mixed = serde_json::json!({"images": [{"data": data}, {"data": base64::engine::general_purpose::STANDARD.encode(b"GIF89a")}]});
        assert!(store_images(dir.path(), &mixed).is_err());
        assert!(!dir.path().exists() || std::fs::read_dir(dir.path()).unwrap().count() == 0);
    }

    async fn relayed(chunks: Vec<&'static [u8]>) -> (tempfile::TempDir, Vec<Value>) {
        use futures::StreamExt;
        let dir = tempfile::tempdir().unwrap();
        let lines: LineStream =
            futures::stream::iter(chunks.into_iter().map(|c| Ok(bytes::Bytes::from_static(c))))
                .boxed();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        relay(lines, dir.path().join("images"), tx).await;
        let mut out = Vec::new();
        while let Ok(value) = rx.try_recv() {
            out.push(value);
        }
        (dir, out)
    }

    #[tokio::test]
    async fn progress_then_stored_images() {
        let data = base64::engine::general_purpose::STANDARD.encode(PNG);
        let last: &'static [u8] = Box::leak(format!("{{\"images\":[{{\"mime\":\"image/png\",\"width\":8,\"height\":8,\"data\":\"{data}\"}}]}}\n").into_bytes().into_boxed_slice());
        let (dir, out) =
            relayed(vec![b"{\"p\":1,\"of\":8}\n{\"p\":", b"8,\"of\":8}\n", last]).await;
        assert_eq!(out[0]["p"], 1);
        assert_eq!(out[1]["p"], 8);
        let image = &out[2]["images"][0];
        assert_eq!(image["ext"], "png");
        assert_eq!(image["width"], 8);
        let hash = image["hash"].as_str().unwrap();
        assert!(dir
            .path()
            .join("images")
            .join(format!("{hash}.png"))
            .is_file());
    }

    #[tokio::test]
    async fn a_failed_run_answers_the_plugin_reason() {
        let (_dir, out) = relayed(vec![
            b"{\"error\":\"no machine serves diffusion of img-x\"}\n",
        ])
        .await;
        assert_eq!(
            out.last().unwrap()["error"],
            "no machine serves diffusion of img-x"
        );
        let (_dir, out) = relayed(vec![b"{\"p\":1,\"of\":8}\n"]).await;
        assert_eq!(
            out.last().unwrap()["error"],
            "the workflow ended without images"
        );
    }
}
