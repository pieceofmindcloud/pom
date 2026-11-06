//! Image model components (diffusion model, text encoder, VAE) that ComfyUI
//! serves on this node and on the network. Apart from the LLM runtime: a node
//! serves an LLM and image components at once.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gateway::GatewayState;
use crate::plugin_services::ServedComponent;

// ── This node's assignments ──────────────────────────────────────────────────

const ASSIGNMENTS_KEY: &str = "served_components";
const COMPONENTS: [&str; 3] = ["diffusion", "text_encoder", "vae"];

/// One component this node was asked to serve, as stored and as handed to the
/// plugin (`component.serve`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ComponentAssignment {
    pub deploy_id: String,
    pub name: String,
    pub component: String,
    /// Absolute paths on this node.
    pub paths: Vec<String>,
    pub plugin_code: String,
    #[serde(default)]
    pub options: Value,
}

/// A plugin operation: `(plugin_code, operation, payload)`. The live one opens
/// the on-demand plugin first; tests record the calls instead.
pub(crate) type PluginCall = std::sync::Arc<
    dyn Fn(String, String, Value) -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

/// The plugin call through this node's plugin manager. A plugin refuses with
/// `{"status": "refused", "error": "..."}`; no answer is a refusal too.
pub(crate) fn live_plugins(plugins: std::sync::Arc<crate::plugins::PluginManager>) -> PluginCall {
    std::sync::Arc::new(move |code: String, op: String, payload: Value| {
        let plugins = std::sync::Arc::clone(&plugins);
        Box::pin(async move {
            plugins.open_on_demand(&code).await.or_else(|error| {
                // A resident plugin is already running.
                if error.contains("not on demand") {
                    Ok(())
                } else {
                    Err(error)
                }
            })?;
            match plugins.query_audit(&code, &op, payload).await {
                Some(answer) if answer["status"] == "refused" => Err(answer["error"]
                    .as_str()
                    .unwrap_or("the plugin refused")
                    .to_owned()),
                Some(_) => Ok(()),
                None => Err(format!("the plugin '{code}' did not answer {op}")),
            }
        })
    })
}

pub(crate) fn load_assignments(db: &crate::db::PomDb) -> Vec<ComponentAssignment> {
    db.local_setting_json(ASSIGNMENTS_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_assignments(db: &crate::db::PomDb, assignments: &[ComponentAssignment]) {
    if let Err(error) = db.save_local_setting(ASSIGNMENTS_KEY, assignments) {
        tracing::warn!(%error, "could not store the served image components");
    }
}

type Refusal = (axum::http::StatusCode, String);

/// Serve one component here: every path must be one of this node's image
/// files (`catalog`), the plugin must accept it, and only then it is stored
/// (replacing the same component of the same deployment).
pub(crate) async fn apply_assignment(
    db: &crate::db::PomDb,
    catalog: &[String],
    call: &PluginCall,
    assignment: ComponentAssignment,
) -> Result<(), Refusal> {
    use axum::http::StatusCode;
    if !COMPONENTS.contains(&assignment.component.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown component {}", assignment.component),
        ));
    }
    if assignment.paths.is_empty() || assignment.deploy_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "a component needs a deployment and a file".into(),
        ));
    }
    if let Some(path) = assignment.paths.iter().find(|path| !catalog.contains(path)) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{path} is not an image model file of this machine"),
        ));
    }
    let payload = serde_json::to_value(&assignment).unwrap_or(Value::Null);
    call(
        assignment.plugin_code.clone(),
        "component.serve".into(),
        payload,
    )
    .await
    .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
    let mut assignments = load_assignments(db);
    assignments.retain(|kept| {
        !(kept.deploy_id == assignment.deploy_id && kept.component == assignment.component)
    });
    assignments.push(assignment);
    save_assignments(db, &assignments);
    Ok(())
}

/// Stop serving every component of a deployment here.
pub(crate) async fn release_deployment(db: &crate::db::PomDb, call: &PluginCall, deploy_id: &str) {
    let mut assignments = load_assignments(db);
    let codes: std::collections::BTreeSet<String> = assignments
        .iter()
        .filter(|a| a.deploy_id == deploy_id)
        .map(|a| a.plugin_code.clone())
        .collect();
    assignments.retain(|a| a.deploy_id != deploy_id);
    save_assignments(db, &assignments);
    for code in codes {
        let payload = serde_json::json!({ "deploy_id": deploy_id });
        if let Err(error) = call(code.clone(), "component.release".into(), payload).await {
            tracing::warn!(plugin = %code, %error, deploy_id, "component release");
        }
    }
}

/// Serve again what this node served before (at startup and periodically,
/// `component.serve` being idempotent in the plugin).
pub(crate) async fn restore(db: &crate::db::PomDb, call: &PluginCall) {
    for assignment in load_assignments(db) {
        let payload = serde_json::to_value(&assignment).unwrap_or(Value::Null);
        if let Err(error) = call(
            assignment.plugin_code.clone(),
            "component.serve".into(),
            payload,
        )
        .await
        {
            tracing::warn!(deploy_id = %assignment.deploy_id, component = %assignment.component, %error,
                "could not serve the image component again");
        }
    }
}

/// This node's image model files, read the way the catalog reads them.
pub(crate) async fn local_image_paths(state: &GatewayState) -> Vec<String> {
    let dirs = state.model_dirs().await;
    tokio::task::spawn_blocking(move || {
        crate::image_catalog::discover_image_files(&crate::catalog::image_roots(&dirs))
            .into_iter()
            .filter(|file| file.metadata_error.is_none())
            .map(|file| file.path)
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// `POST /api/ui/components/apply`.
pub(crate) async fn apply_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::Json(assignment): axum::Json<ComponentAssignment>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let catalog = local_image_paths(&state).await;
    let call = live_plugins(std::sync::Arc::clone(&state.plugins));
    match apply_assignment(&state.database, &catalog, &call, assignment).await {
        Ok(()) => axum::Json(serde_json::json!({ "ok": true })).into_response(),
        Err((status, error)) => {
            (status, axum::Json(serde_json::json!({ "error": error }))).into_response()
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct ReleaseRequest {
    pub deploy_id: String,
}

/// `POST /api/ui/components/release`.
pub(crate) async fn release_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::Json(request): axum::Json<ReleaseRequest>,
) -> axum::Json<Value> {
    let call = live_plugins(std::sync::Arc::clone(&state.plugins));
    release_deployment(&state.database, &call, &request.deploy_id).await;
    axum::Json(serde_json::json!({ "ok": true }))
}

// ── Image deployments (the node an admin deploys from) ──────────────────────

const DEPLOYMENTS_KEY: &str = "image_deployments";
const APPLY_PATH: &str = "/api/ui/components/apply";
const RELEASE_PATH: &str = "/api/ui/components/release";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ComponentChoice {
    pub component: String,
    pub node_id: String,
    pub path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ImageDeployRequest {
    pub name: String,
    pub components: Vec<ComponentChoice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ImageDeployment {
    pub deploy_id: String,
    pub name: String,
    pub created_at: String,
    pub components: Vec<ComponentChoice>,
}

/// What the deploy needs to know of one machine.
#[derive(Debug, Clone)]
pub(crate) struct MachineFacts {
    pub node_id: String,
    pub hostname: String,
    /// (path, category) of its readable image files.
    pub image_files: Vec<(String, String)>,
    pub has_image_service: bool,
}

/// A call to a machine's component route: `(node_id, path, body)`.
pub(crate) type TargetCall = std::sync::Arc<
    dyn Fn(String, &'static str, Value) -> futures::future::BoxFuture<'static, Result<(), String>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Released {
    pub node_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub(crate) fn load_image_deployments(db: &crate::db::PomDb) -> Vec<ImageDeployment> {
    db.local_setting_json(DEPLOYMENTS_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_image_deployments(db: &crate::db::PomDb, deployments: &[ImageDeployment]) {
    if let Err(error) = db.save_local_setting(DEPLOYMENTS_KEY, deployments) {
        tracing::warn!(%error, "could not store the image deployments");
    }
}

/// A category a component accepts: its own, or a checkpoint holding all three.
fn fits(component: &str, category: &str) -> bool {
    category == component || category == "checkpoint"
}

fn validate(request: &ImageDeployRequest, machines: &[MachineFacts]) -> Result<(), String> {
    if request.name.trim().is_empty() {
        return Err("an image deployment needs a name".into());
    }
    if request.components.is_empty() || request.components.len() > COMPONENTS.len() {
        return Err("choose one to three components".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for choice in &request.components {
        if !COMPONENTS.contains(&choice.component.as_str()) {
            return Err(format!("unknown component {}", choice.component));
        }
        if !seen.insert(choice.component.as_str()) {
            return Err(format!("{} is chosen twice", choice.component));
        }
        let Some(machine) = machines.iter().find(|m| m.node_id == choice.node_id) else {
            return Err(format!("machine {} is not on the network", choice.node_id));
        };
        if !machine.has_image_service {
            return Err(format!("{} does not run ComfyUI", machine.hostname));
        }
        let file = std::path::Path::new(&choice.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        match machine
            .image_files
            .iter()
            .find(|(path, _)| *path == choice.path)
        {
            None => return Err(format!("{} does not hold {file}", machine.hostname)),
            Some((_, category)) if !fits(&choice.component, category) => {
                return Err(format!(
                    "{file} is a {category}, not a {}",
                    choice.component
                ))
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn assignment_of(deploy_id: &str, name: &str, choice: &ComponentChoice) -> Value {
    serde_json::json!({
        "deploy_id": deploy_id,
        "name": name,
        "component": choice.component,
        "paths": [choice.path],
        "plugin_code": crate::components::IMAGE_PLUGIN,
        "options": {},
    })
}

/// The plugin that serves image components. One today; the service lists
/// its own code, so another plugin could join later.
pub(crate) const IMAGE_PLUGIN: &str = "comfyui";

/// Deploy an image model by component. On a failure the machines already
/// applied are released and nothing is recorded.
pub(crate) async fn deploy_image(
    db: &crate::db::PomDb,
    machines: &[MachineFacts],
    call: &TargetCall,
    request: ImageDeployRequest,
) -> Result<String, String> {
    validate(&request, machines)?;
    let deploy_id = format!("img-{}", &hex::encode(rand::random::<[u8; 6]>()));
    let name = request.name.trim().to_owned();
    let mut applied: Vec<String> = Vec::new();
    for choice in &request.components {
        let body = assignment_of(&deploy_id, &name, choice);
        if let Err(error) = call(choice.node_id.clone(), APPLY_PATH, body).await {
            for node_id in &applied {
                let _ = call(
                    node_id.clone(),
                    RELEASE_PATH,
                    serde_json::json!({"deploy_id": deploy_id}),
                )
                .await;
            }
            let hostname = machines
                .iter()
                .find(|m| m.node_id == choice.node_id)
                .map(|m| m.hostname.clone())
                .unwrap_or_else(|| choice.node_id.clone());
            return Err(format!(
                "{hostname} could not serve the {}: {error}",
                choice.component
            ));
        }
        if !applied.contains(&choice.node_id) {
            applied.push(choice.node_id.clone());
        }
    }
    let mut deployments = load_image_deployments(db);
    deployments.push(ImageDeployment {
        deploy_id: deploy_id.clone(),
        name,
        created_at: chrono::Utc::now().to_rfc3339(),
        components: request.components,
    });
    save_image_deployments(db, &deployments);
    Ok(deploy_id)
}

/// Undeploy: every target is asked to release; the record goes either way.
pub(crate) async fn undeploy_image(
    db: &crate::db::PomDb,
    call: &TargetCall,
    deploy_id: &str,
) -> Vec<Released> {
    let mut deployments = load_image_deployments(db);
    let Some(position) = deployments.iter().position(|d| d.deploy_id == deploy_id) else {
        return Vec::new();
    };
    let deployment = deployments.remove(position);
    save_image_deployments(db, &deployments);
    let mut nodes: Vec<String> = Vec::new();
    for choice in &deployment.components {
        if !nodes.contains(&choice.node_id) {
            nodes.push(choice.node_id.clone());
        }
    }
    let mut released = Vec::new();
    for node_id in nodes {
        let result = call(
            node_id.clone(),
            RELEASE_PATH,
            serde_json::json!({"deploy_id": deploy_id}),
        )
        .await;
        released.push(Released {
            node_id,
            ok: result.is_ok(),
            error: result.err(),
        });
    }
    released
}

/// The machines of the network as the deploy sees them: their image files
/// (`GET /api/ui/models`) and whether they run an image service.
pub(crate) async fn survey_machines(state: &GatewayState) -> Vec<MachineFacts> {
    let peers = state.all_peers().await;
    let lookups = peers.into_iter().map(|peer| async move {
        let models = state
            .control_get(&peer, "/api/ui/models", std::time::Duration::from_secs(16))
            .await
            .ok()?;
        let body: Value = models.json().await.ok()?;
        let image_files = body["image_files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| f["metadata_error"].is_null())
            .filter_map(|f| {
                Some((
                    f["path"].as_str()?.to_owned(),
                    f["category"].as_str()?.to_owned(),
                ))
            })
            .collect();
        let services = crate::plugin_engine::services_of(state, &peer.node_id)
            .await
            .unwrap_or_default();
        Some(MachineFacts {
            node_id: peer.node_id.clone(),
            hostname: peer.hostname.clone(),
            image_files,
            has_image_service: services.iter().any(|s| s["kind"] == "image"),
        })
    });
    futures::future::join_all(lookups)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Calls to the machines' component routes, signed, through each one's own
/// address (this node through loopback).
pub(crate) fn live_targets(state: GatewayState) -> TargetCall {
    std::sync::Arc::new(move |node_id: String, path: &'static str, body: Value| {
        let state = state.clone();
        Box::pin(async move {
            let peer = state
                .all_peers()
                .await
                .into_iter()
                .find(|peer| peer.node_id == node_id)
                .ok_or_else(|| format!("machine {node_id} is not on the network"))?;
            let response = state
                .control_post_json(&peer, path, &body, std::time::Duration::from_secs(120))
                .await
                .map_err(|error| error.to_string())?;
            if response.status().is_success() {
                return Ok(());
            }
            let status = response.status();
            let body: Value = response.json().await.unwrap_or(Value::Null);
            Err(body["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| status.to_string()))
        })
    })
}

/// `POST /api/ui/image-deploy`.
pub(crate) async fn image_deploy_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::Json(request): axum::Json<ImageDeployRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let machines = survey_machines(&state).await;
    let call = live_targets(state.clone());
    match deploy_image(&state.database, &machines, &call, request).await {
        Ok(deploy_id) => axum::Json(serde_json::json!({ "deploy_id": deploy_id })).into_response(),
        Err(error) => {
            let status = if error.contains("could not serve") {
                axum::http::StatusCode::BAD_GATEWAY
            } else {
                axum::http::StatusCode::BAD_REQUEST
            };
            (status, axum::Json(serde_json::json!({ "error": error }))).into_response()
        }
    }
}

/// The image deployments this node recorded, with each component's status
/// on the network (`missing` when no live service lists it).
pub(crate) async fn image_deployments_view(state: &GatewayState) -> Value {
    let served = network_components(state).await;
    let peers = state.all_peers().await;
    let deployments: Vec<Value> = load_image_deployments(&state.database)
        .into_iter()
        .map(|deployment| {
            let components: Vec<Value> = deployment
                .components
                .iter()
                .map(|choice| {
                    let status = served
                        .iter()
                        .find(|s| {
                            s.deploy_id == deployment.deploy_id
                                && s.component == choice.component
                                && s.node_id == choice.node_id
                        })
                        .map(|s| s.status.clone())
                        .unwrap_or_else(|| "missing".into());
                    let hostname = peers
                        .iter()
                        .find(|p| p.node_id == choice.node_id)
                        .map(|p| p.hostname.clone())
                        .unwrap_or_else(|| choice.node_id.clone());
                    serde_json::json!({
                        "component": choice.component, "node_id": choice.node_id,
                        "hostname": hostname, "path": choice.path, "status": status,
                    })
                })
                .collect();
            serde_json::json!({
                "deploy_id": deployment.deploy_id, "name": deployment.name,
                "created_at": deployment.created_at, "components": components,
            })
        })
        .collect();
    serde_json::json!({ "deployments": deployments })
}

/// `GET /api/ui/image-deployments`.
pub(crate) async fn image_deployments_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
) -> axum::Json<Value> {
    axum::Json(image_deployments_view(&state).await)
}

/// `POST /api/ui/image-undeploy`.
pub(crate) async fn image_undeploy_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::Json(request): axum::Json<ReleaseRequest>,
) -> axum::Json<Value> {
    let call = live_targets(state.clone());
    let released = undeploy_image(&state.database, &call, &request.deploy_id).await;
    axum::Json(serde_json::json!({ "released": released }))
}

/// A component served somewhere on the network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct NetworkComponent {
    pub deploy_id: String,
    pub name: String,
    pub component: String,
    pub files: Vec<String>,
    pub status: String,
    pub node_id: String,
    pub hostname: String,
    pub plugin_code: String,
}

/// The components the image services of one machine serve.
pub(crate) fn components_of(
    node_id: &str,
    hostname: &str,
    services: &[Value],
) -> Vec<NetworkComponent> {
    services
        .iter()
        .filter(|service| service["kind"] == "image")
        .flat_map(|service| {
            let code = service["plugin_code"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            service["components"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| serde_json::from_value::<ServedComponent>(c.clone()).ok())
                .map(move |c| NetworkComponent {
                    deploy_id: c.deploy_id,
                    name: c.name,
                    component: c.component,
                    files: c.files,
                    status: c.status,
                    node_id: node_id.to_owned(),
                    hostname: hostname.to_owned(),
                    plugin_code: code.clone(),
                })
        })
        .collect()
}

/// Every image component served on the network: this node's and each live
/// peer's, sorted by deployment name, component and machine.
pub(crate) async fn network_components(state: &GatewayState) -> Vec<NetworkComponent> {
    let mut machines = vec![(state.network.node_id.clone(), state.network.hostname())];
    for peer in state.network.alive_peers().await {
        if peer.node_id != state.network.node_id {
            machines.push((peer.node_id.clone(), peer.hostname.clone()));
        }
    }
    let lookups = machines.into_iter().map(|(node_id, hostname)| async move {
        let services = crate::plugin_engine::services_of(state, &node_id)
            .await
            .unwrap_or_default();
        components_of(&node_id, &hostname, &services)
    });
    let mut all: Vec<NetworkComponent> = futures::future::join_all(lookups)
        .await
        .into_iter()
        .flatten()
        .collect();
    all.sort_by(|a, b| {
        (a.name.as_str(), a.component.as_str(), a.hostname.as_str()).cmp(&(
            b.name.as_str(),
            b.component.as_str(),
            b.hostname.as_str(),
        ))
    });
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// A plugin that records what it was asked and answers `answer`.
    fn recording(answer: Result<(), String>) -> (PluginCall, Arc<Mutex<Vec<(String, Value)>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let call: PluginCall = Arc::new(move |_code: String, op: String, payload: Value| {
            log.lock().unwrap().push((op, payload));
            let answer = answer.clone();
            Box::pin(async move { answer })
        });
        (call, seen)
    }

    fn db() -> (tempfile::TempDir, crate::db::PomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::PomDb::open(dir.path().join("pom.db")).unwrap();
        (dir, db)
    }

    fn assignment(deploy_id: &str, component: &str, path: &str) -> ComponentAssignment {
        ComponentAssignment {
            deploy_id: deploy_id.into(),
            name: "Z-Image".into(),
            component: component.into(),
            paths: vec![path.into()],
            plugin_code: "comfyui".into(),
            options: Value::Null,
        }
    }

    #[tokio::test]
    async fn a_component_is_served_and_stored() {
        let (_dir, db) = db();
        let (call, seen) = recording(Ok(()));
        let catalog = vec!["/m/ae.safetensors".to_owned()];
        apply_assignment(
            &db,
            &catalog,
            &call,
            assignment("img-1", "vae", "/m/ae.safetensors"),
        )
        .await
        .unwrap();
        assert_eq!(load_assignments(&db).len(), 1);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "component.serve");
        assert_eq!(seen[0].1["component"], "vae");
    }

    #[tokio::test]
    async fn a_path_outside_the_catalog_is_refused() {
        let (_dir, db) = db();
        let (call, seen) = recording(Ok(()));
        let error = apply_assignment(&db, &[], &call, assignment("img-1", "vae", "/etc/passwd"))
            .await
            .unwrap_err();
        assert_eq!(error.0, axum::http::StatusCode::BAD_REQUEST);
        assert!(load_assignments(&db).is_empty());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_plugin_refusal_stores_nothing() {
        let (_dir, db) = db();
        let (call, _) = recording(Err("ComfyUI is not installed".into()));
        let catalog = vec!["/m/ae.safetensors".to_owned()];
        let error = apply_assignment(
            &db,
            &catalog,
            &call,
            assignment("img-1", "vae", "/m/ae.safetensors"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.0, axum::http::StatusCode::BAD_GATEWAY);
        assert!(error.1.contains("not installed"));
        assert!(load_assignments(&db).is_empty());
    }

    #[tokio::test]
    async fn two_components_on_one_node_are_kept_apart() {
        let (_dir, db) = db();
        let (call, seen) = recording(Ok(()));
        let catalog = vec![
            "/m/ae.safetensors".to_owned(),
            "/m/z.safetensors".to_owned(),
        ];
        for a in [
            assignment("img-1", "diffusion", "/m/z.safetensors"),
            assignment("img-1", "vae", "/m/ae.safetensors"),
            assignment("img-2", "vae", "/m/ae.safetensors"),
        ] {
            apply_assignment(&db, &catalog, &call, a).await.unwrap();
        }
        // The same component of the same deployment replaces, never duplicates.
        apply_assignment(
            &db,
            &catalog,
            &call,
            assignment("img-1", "vae", "/m/ae.safetensors"),
        )
        .await
        .unwrap();
        assert_eq!(load_assignments(&db).len(), 3);
        release_deployment(&db, &call, "img-1").await;
        let left = load_assignments(&db);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].deploy_id, "img-2");
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|(op, payload)| op == "component.release" && payload["deploy_id"] == "img-1"));
    }

    #[tokio::test]
    async fn served_components_survive_a_restart() {
        let (dir, db) = db();
        let (call, _) = recording(Ok(()));
        let catalog = vec!["/m/ae.safetensors".to_owned()];
        apply_assignment(
            &db,
            &catalog,
            &call,
            assignment("img-1", "vae", "/m/ae.safetensors"),
        )
        .await
        .unwrap();
        drop(db);
        let reopened = crate::db::PomDb::open(dir.path().join("pom.db")).unwrap();
        let (call, seen) = recording(Ok(()));
        restore(&reopened, &call).await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "component.serve");
        assert_eq!(seen[0].1["deploy_id"], "img-1");
    }

    fn machine(node_id: &str, files: &[(&str, &str)], service: bool) -> MachineFacts {
        MachineFacts {
            node_id: node_id.into(),
            hostname: format!("host-{node_id}"),
            image_files: files
                .iter()
                .map(|(path, category)| (path.to_string(), category.to_string()))
                .collect(),
            has_image_service: service,
        }
    }

    fn choice(component: &str, node_id: &str, path: &str) -> ComponentChoice {
        ComponentChoice {
            component: component.into(),
            node_id: node_id.into(),
            path: path.into(),
        }
    }

    fn network() -> Vec<MachineFacts> {
        vec![
            machine("a", &[("/m/z.safetensors", "diffusion")], true),
            machine("b", &[("/m/qwen.safetensors", "text_encoder")], true),
            machine("c", &[("/m/ae.safetensors", "vae")], true),
            machine("d", &[("/m/ae.safetensors", "vae")], false),
        ]
    }

    fn request(components: Vec<ComponentChoice>) -> ImageDeployRequest {
        ImageDeployRequest {
            name: "Z-Image".into(),
            components,
        }
    }

    type Calls = Arc<Mutex<Vec<String>>>;

    fn targets_call(fail_on: Option<&'static str>) -> (TargetCall, Calls) {
        let seen: Calls = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let call: TargetCall = Arc::new(move |node_id: String, path: &'static str, body: Value| {
            log.lock().unwrap().push(format!(
                "{path} {node_id} {}",
                body["component"].as_str().unwrap_or("")
            ));
            let fail = fail_on == Some(node_id.as_str()) && path.ends_with("apply");
            Box::pin(async move {
                if fail {
                    Err("ComfyUI is not installed".to_owned())
                } else {
                    Ok(())
                }
            })
        });
        (call, seen)
    }

    #[tokio::test]
    async fn an_image_model_is_deployed_on_its_holders() {
        let (_dir, db) = db();
        let (call, seen) = targets_call(None);
        let req = request(vec![
            choice("diffusion", "a", "/m/z.safetensors"),
            choice("text_encoder", "b", "/m/qwen.safetensors"),
            choice("vae", "c", "/m/ae.safetensors"),
        ]);
        let deploy_id = deploy_image(&db, &network(), &call, req).await.unwrap();
        assert!(deploy_id.starts_with("img-"));
        let seen = seen.lock().unwrap();
        assert_eq!(
            *seen,
            [
                "/api/ui/components/apply a diffusion",
                "/api/ui/components/apply b text_encoder",
                "/api/ui/components/apply c vae",
            ]
        );
        let records = load_image_deployments(&db);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].components.len(), 3);
    }

    #[tokio::test]
    async fn a_machine_without_the_file_is_refused() {
        let (_dir, db) = db();
        let (call, seen) = targets_call(None);
        let req = request(vec![choice("diffusion", "b", "/m/z.safetensors")]);
        let error = deploy_image(&db, &network(), &call, req).await.unwrap_err();
        assert!(
            error.contains("host-b") && error.contains("z.safetensors"),
            "{error}"
        );
        assert!(seen.lock().unwrap().is_empty());
        let req = request(vec![choice("vae", "d", "/m/ae.safetensors")]);
        let error = deploy_image(&db, &network(), &call, req).await.unwrap_err();
        assert!(
            error.contains("ComfyUI"),
            "a machine without the image service: {error}"
        );
    }

    #[tokio::test]
    async fn a_wrong_category_is_refused() {
        let (_dir, db) = db();
        let (call, _) = targets_call(None);
        let req = request(vec![choice("vae", "a", "/m/z.safetensors")]);
        assert!(deploy_image(&db, &network(), &call, req).await.is_err());
        let req = request(vec![
            choice("vae", "c", "/m/ae.safetensors"),
            choice("vae", "c", "/m/ae.safetensors"),
        ]);
        assert!(
            deploy_image(&db, &network(), &call, req).await.is_err(),
            "a component twice"
        );
    }

    #[tokio::test]
    async fn a_failed_target_releases_the_others() {
        let (_dir, db) = db();
        let (call, seen) = targets_call(Some("c"));
        let req = request(vec![
            choice("diffusion", "a", "/m/z.safetensors"),
            choice("text_encoder", "b", "/m/qwen.safetensors"),
            choice("vae", "c", "/m/ae.safetensors"),
        ]);
        let error = deploy_image(&db, &network(), &call, req).await.unwrap_err();
        assert!(
            error.contains("host-c") && error.contains("not installed"),
            "{error}"
        );
        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"/api/ui/components/release a ".to_owned()));
        assert!(seen.contains(&"/api/ui/components/release b ".to_owned()));
        assert!(load_image_deployments(&db).is_empty());
    }

    #[tokio::test]
    async fn undeploy_releases_every_target() {
        let (_dir, db) = db();
        let (call, _) = targets_call(None);
        let req = request(vec![
            choice("diffusion", "a", "/m/z.safetensors"),
            choice("vae", "c", "/m/ae.safetensors"),
        ]);
        let deploy_id = deploy_image(&db, &network(), &call, req).await.unwrap();
        let (call, seen) = targets_call(None);
        let released = undeploy_image(&db, &call, &deploy_id).await;
        assert_eq!(released.len(), 2);
        assert!(released.iter().all(|r| r.ok));
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert!(load_image_deployments(&db).is_empty());
    }

    #[test]
    fn a_visitor_refuses_to_serve() {
        use axum::http::Method;
        assert!(crate::webui::visitor_refuses("/api/ui/components/apply"));
        for path in ["/api/ui/components/apply", "/api/ui/components/release"] {
            assert_eq!(
                crate::webui::route_scope(&Method::POST, path),
                crate::webui::RouteScope::AnyAdmin
            );
        }
    }

    #[test]
    fn network_components_list_only_image_services() {
        let services = vec![
            json!({"plugin_code": "comfyui", "id": "comfyui-image", "kind": "image",
                   "name": "ComfyUI", "status": "ready",
                   "components": [{"deploy_id": "img-1", "name": "Z-Image",
                                   "component": "vae", "files": ["ae.safetensors"],
                                   "status": "ready"}]}),
            json!({"plugin_code": "strata", "id": "strata", "kind": "inference",
                   "name": "Strata", "status": "ready",
                   "components": [{"deploy_id": "x", "name": "x", "component": "vae",
                                   "files": [], "status": "ready"}]}),
        ];
        let listed = components_of("node-a", "machine-a", &services);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].node_id, "node-a");
        assert_eq!(listed[0].hostname, "machine-a");
        assert_eq!(listed[0].plugin_code, "comfyui");
        assert_eq!(listed[0].component, "vae");
        assert_eq!(listed[0].files, ["ae.safetensors"]);
    }
}
