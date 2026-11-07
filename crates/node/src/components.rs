//! Image model components (diffusion model, text encoder, VAE) that ComfyUI
//! serves on this node and on the network. Apart from the LLM runtime: a node
//! serves an LLM and image components at once.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gateway::GatewayState;
use crate::plugin_services::ServedComponent;
pub(crate) use crate::image_records::{
    load_image_deployments, save_image_deployments, ComponentChoice, ImageDeployment,
};

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

const RELEASE_PATH: &str = "/api/ui/components/release";

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ImageDeployRequest {
    pub name: String,
    pub components: Vec<ComponentChoice>,
}

/// One readable image file of a machine, as the stage plan needs it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ImageFileFact {
    pub path: String,
    pub size_bytes: u64,
    pub category: String,
}

/// What the deploy needs to know of one machine.
#[derive(Debug, Clone, Default)]
pub(crate) struct MachineFacts {
    pub node_id: String,
    pub hostname: String,
    /// (path, category) of its readable image files.
    pub image_files: Vec<(String, String)>,
    pub has_image_service: bool,
    /// The same files with their sizes (the identity across machines is the
    /// file name and size).
    pub files: Vec<ImageFileFact>,
    /// The status of its `image` service, when it publishes one.
    pub service_status: Option<String>,
    pub gpu_free_mb: u64,
    pub ram_free_mb: u64,
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

pub(crate) fn assignment_of(deploy_id: &str, name: &str, choice: &ComponentChoice) -> Value {
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

/// Deploy an image model from the form before the stages (one machine per
/// component). The stage plan checks it; on a failure the machines already
/// applied are released and nothing is recorded.
pub(crate) async fn deploy_image(
    db: &crate::db::PomDb,
    machines: &[MachineFacts],
    call: &TargetCall,
    request: ImageDeployRequest,
) -> Result<String, String> {
    let request = crate::image_plan::DeployBody::Components(request).into_request();
    let plan = crate::image_plan::plan_image(&request, machines);
    if let Some((_, body)) = crate::image_plan::refusal(&plan, false) {
        return Err(body["error"].as_str().unwrap_or("refused").to_owned());
    }
    let deploy_id = crate::image_plan::new_deploy_id();
    crate::image_plan::deploy_stages(db, &plan, call, &deploy_id, "", &|_| {}).await?;
    Ok(deploy_id)
}

/// Undeploy: every target is asked to release; the record goes either way.
pub(crate) async fn undeploy_image(
    db: &crate::db::PomDb,
    call: &TargetCall,
    deploy_id: &str,
) -> Vec<Released> {
    let deployments = load_image_deployments(db);
    let Some(deployment) = deployments
        .iter()
        .find(|d| d.deploy_id == deploy_id)
        .cloned()
    else {
        return Vec::new();
    };
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
    // A machine that did not answer would serve its part again on its next
    // start (it restores its own list): its part stays on record, so the
    // screen still lists it and undeploying again releases it.
    let pending: Vec<ComponentChoice> = deployment
        .components
        .iter()
        .filter(|choice| {
            released
                .iter()
                .any(|r| !r.ok && r.node_id == choice.node_id)
        })
        .cloned()
        .collect();
    let mut deployments: Vec<ImageDeployment> = deployments
        .into_iter()
        .filter(|d| d.deploy_id != deploy_id)
        .collect();
    if !pending.is_empty() {
        let mut record = ImageDeployment {
            components: pending,
            versions: Vec::new(),
            ..deployment
        };
        record.ensure_versions();
        deployments.push(record);
    }
    save_image_deployments(db, &deployments);
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
        let files: Vec<ImageFileFact> = body["image_files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| f["metadata_error"].is_null())
            .filter_map(|f| {
                Some(ImageFileFact {
                    path: f["path"].as_str()?.to_owned(),
                    size_bytes: f["size_bytes"].as_u64().unwrap_or(0),
                    category: f["category"].as_str()?.to_owned(),
                })
            })
            .collect();
        let services = crate::plugin_engine::services_of(state, &peer.node_id)
            .await
            .unwrap_or_default();
        let service_status = services
            .iter()
            .find(|s| s["kind"] == "image")
            .map(|s| s["status"].as_str().unwrap_or("ready").to_owned());
        // The free memory the LLM plan reads, from the machine's node view.
        let node: Option<Value> = match state
            .control_get(&peer, "/api/ui/node", std::time::Duration::from_secs(8))
            .await
        {
            Ok(response) => response.json().await.ok(),
            Err(_) => None,
        };
        let (gpu_free_mb, ram_free_mb) =
            crate::deploy::memory_capacity(node.as_ref().and_then(|n| n.get("system")));
        Some(MachineFacts {
            node_id: peer.node_id.clone(),
            hostname: peer.hostname.clone(),
            image_files: files
                .iter()
                .map(|f| (f.path.clone(), f.category.clone()))
                .collect(),
            has_image_service: service_status.is_some(),
            files,
            service_status,
            gpu_free_mb,
            ram_free_mb,
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

/// `POST /api/ui/image-deploy`: checks the plan, answers `{deploy_id,
/// job_id}` at once and applies the stages in a deploy job, which the
/// deploy studio's progress window and the bell follow.
pub(crate) async fn image_deploy_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::Json(body): axum::Json<crate::image_plan::DeployBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let request = body.into_request();
    let machines = survey_machines(&state).await;
    let plan = crate::image_plan::plan_image(&request, &machines);
    if let Some((status, body)) = crate::image_plan::refusal(&plan, request.allow_transfers) {
        let status = axum::http::StatusCode::from_u16(status)
            .unwrap_or(axum::http::StatusCode::BAD_REQUEST);
        return (status, axum::Json(body)).into_response();
    }
    let deploy_id = crate::image_plan::new_deploy_id();
    let job_id = format!("deploy-{deploy_id}");
    let mut job = crate::webui::new_job(&job_id, "deploy", format!("Deploy {}", plan.name), "applying");
    job.deploy_id = Some(deploy_id.clone());
    job.deployment_name = Some(plan.name.clone());
    crate::webui::jobs().write().await.insert(job_id.clone(), job);
    let by = state.network.node_id.clone();
    let (task_deploy_id, task_job_id) = (deploy_id.clone(), job_id.clone());
    tokio::spawn(async move {
        let call = live_targets(state.clone());
        let progress_job = task_job_id.clone();
        let progress = move |pct: f64| {
            let id = progress_job.clone();
            tokio::spawn(async move { crate::webui::set_job_progress(&id, pct).await });
        };
        match crate::image_plan::deploy_stages(
            &state.database,
            &plan,
            &call,
            &task_deploy_id,
            &by,
            &progress,
        )
        .await
        {
            Ok(failed) if failed.is_empty() => {
                crate::webui::set_job(&task_job_id, "done", "ready", "every machine is serving").await
            }
            Ok(failed) => {
                let names: Vec<String> = failed
                    .iter()
                    .map(|f| format!("{}: {}", f.hostname, f.reason.clone().unwrap_or_default()))
                    .collect();
                crate::webui::set_job(
                    &task_job_id,
                    "done",
                    "ready",
                    &format!("serving; these machines could not join: {}", names.join("; ")),
                )
                .await
            }
            Err(error) => crate::webui::set_job(&task_job_id, "failed", "failed", &error).await,
        }
    });
    axum::Json(serde_json::json!({ "deploy_id": deploy_id, "job_id": job_id })).into_response()
}

/// Deploy with this node's authority and wait for the result (the plugin
/// host API's deploy, which answers `{deploy_id, failed}`).
pub(crate) async fn deploy_response(
    state: &GatewayState,
    body: crate::image_plan::DeployBody,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let request = body.into_request();
    let machines = survey_machines(state).await;
    let plan = crate::image_plan::plan_image(&request, &machines);
    if let Some((status, body)) = crate::image_plan::refusal(&plan, request.allow_transfers) {
        let status = axum::http::StatusCode::from_u16(status)
            .unwrap_or(axum::http::StatusCode::BAD_REQUEST);
        return (status, axum::Json(body)).into_response();
    }
    let call = live_targets(state.clone());
    let deploy_id = crate::image_plan::new_deploy_id();
    let by = state.network.node_id.clone();
    match crate::image_plan::deploy_stages(&state.database, &plan, &call, &deploy_id, &by, &|_| {})
        .await
    {
        Ok(failed) => {
            axum::Json(serde_json::json!({ "deploy_id": deploy_id, "failed": failed })).into_response()
        }
        Err(error) => (
            axum::http::StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
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
    axum::Json(undeploy_view(&state, &request.deploy_id).await)
}

/// Release a deployment everywhere: `{released: [...]}`.
pub(crate) async fn undeploy_view(state: &GatewayState, deploy_id: &str) -> Value {
    let call = live_targets(state.clone());
    let released = undeploy_image(&state.database, &call, deploy_id).await;
    serde_json::json!({ "released": released })
}

/// The network's image files per machine, for a deploy dialog that offers
/// only the machines holding a file: `{machines: [{node_id, hostname,
/// image_files: [{path, name, category}], has_image_service}]}`.
pub(crate) async fn image_files_view(state: &GatewayState) -> Value {
    let machines: Vec<Value> = survey_machines(state)
        .await
        .into_iter()
        .map(|machine| {
            let files: Vec<Value> = machine
                .image_files
                .iter()
                .map(|(path, category)| {
                    let name = std::path::Path::new(path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone());
                    serde_json::json!({ "path": path, "name": name, "category": category })
                })
                .collect();
            serde_json::json!({
                "node_id": machine.node_id, "hostname": machine.hostname,
                "image_files": files, "has_image_service": machine.has_image_service,
            })
        })
        .collect();
    serde_json::json!({ "machines": machines })
}

// ── Calls to a component ─────────────────────────────────────────────────────

/// The largest op body that crosses the network (a latent, an image, the
/// conditioning of a prompt).
pub(crate) const MAX_COMPONENT_BODY: usize = 64 * 1024 * 1024;
/// A cold load of a large diffusion model can take minutes.
const COMPONENT_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

fn valid_op(op: &str) -> bool {
    !op.is_empty() && op.len() <= 32 && op.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// Sends an op to the machine that serves the component: `(target, op, body,
/// job)` -> `(status, body)`, or the transport error.
pub(crate) type Forward = std::sync::Arc<
    dyn Fn(
            NetworkComponent,
            String,
            Vec<u8>,
            Option<String>,
        ) -> futures::future::BoxFuture<'static, Result<(u16, Vec<u8>), String>>
        + Send
        + Sync,
>;

fn error_body(message: String) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "error": message })).unwrap_or_default()
}

/// One op on a network component: find who serves it and forward.
pub(crate) async fn call_component(
    network: &[NetworkComponent],
    deploy_id: &str,
    component: &str,
    op: &str,
    body: Vec<u8>,
    job: Option<String>,
    forward: &Forward,
) -> (u16, Vec<u8>) {
    if !valid_op(op) {
        return (400, error_body(format!("invalid op {op:?}")));
    }
    let Some(target) = network.iter().find(|c| {
        c.deploy_id == deploy_id
            && c.component == component
            && (c.status == "ready" || c.status == "loading")
    }) else {
        return (
            404,
            error_body(format!("no machine serves {component} of {deploy_id}")),
        );
    };
    let hostname = target.hostname.clone();
    match forward(target.clone(), op.to_owned(), body, job).await {
        Ok(answer) => answer,
        Err(cause) => (
            502,
            error_body(format!(
                "{hostname} did not answer for {component}: {cause}"
            )),
        ),
    }
}

/// The plugin that serves this component of this deployment here, if any.
pub(crate) fn serving_plugin(
    assignments: &[ComponentAssignment],
    deploy_id: &str,
    component: &str,
) -> Option<String> {
    assignments
        .iter()
        .find(|a| a.deploy_id == deploy_id && a.component == component)
        .map(|a| a.plugin_code.clone())
}

/// Runs the op on this node's plugin: its loopback component server, behind
/// the plugin's own token.
async fn call_local_plugin(
    state: &GatewayState,
    code: &str,
    deploy_id: &str,
    component: &str,
    op: &str,
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>), String> {
    let upstream = state
        .plugins
        .query_audit(
            code,
            "component.upstream",
            serde_json::json!({ "deploy_id": deploy_id, "component": component }),
        )
        .await
        .ok_or_else(|| format!("the plugin '{code}' did not answer"))?;
    if upstream["status"] != "ready" {
        return Err(format!(
            "ComfyUI is {}",
            upstream["status"].as_str().unwrap_or("not ready")
        ));
    }
    let port = upstream["port"].as_u64().ok_or("the plugin gave no port")?;
    let token = upstream["token"].as_str().unwrap_or_default();
    let url = format!("http://127.0.0.1:{port}/pom/component/{deploy_id}/{component}/{op}");
    let response = state
        .peer_http
        .plain()
        .post(url)
        .header("x-pom-plugin-token", token)
        .body(body)
        .timeout(COMPONENT_CALL_TIMEOUT)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status().as_u16();
    let bytes = response.bytes().await.map_err(|error| error.to_string())?;
    Ok((status, bytes.to_vec()))
}

/// The forward through this node: its own plugin when it serves the
/// component, else the signed internal route of the machine that does.
pub(crate) fn live_forward(state: GatewayState) -> Forward {
    std::sync::Arc::new(
        move |target: NetworkComponent, op: String, body: Vec<u8>, _job: Option<String>| {
            let state = state.clone();
            Box::pin(async move {
                if target.node_id == state.network.node_id {
                    return call_local_plugin(
                        &state,
                        &target.plugin_code,
                        &target.deploy_id,
                        &target.component,
                        &op,
                        body,
                    )
                    .await;
                }
                let peer = state
                    .all_peers()
                    .await
                    .into_iter()
                    .find(|peer| peer.node_id == target.node_id)
                    .ok_or("the machine left the network")?;
                let path = format!(
                    "/api/internal/plugins/{}/component/{}/{}/{op}",
                    target.plugin_code, target.deploy_id, target.component
                );
                let response = state
                    .control_post_bytes(&peer, &path, body, COMPONENT_CALL_TIMEOUT)
                    .await
                    .map_err(|error| error.to_string())?;
                let status = response.status().as_u16();
                let bytes = response.bytes().await.map_err(|error| error.to_string())?;
                Ok((status, bytes.to_vec()))
            })
        },
    )
}

/// `POST /api/internal/plugins/:code/component/:deploy_id/:component/:op`:
/// a live member asks this node to run an op of a component it serves.
pub(crate) async fn internal_component_route(
    auth: crate::auth::InternalNodeAuth,
    axum::extract::State(state): axum::extract::State<GatewayState>,
    axum::extract::Path((code, deploy_id, component, op)): axum::extract::Path<(
        String,
        String,
        String,
        String,
    )>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let refuse = |status: StatusCode, message: String| {
        (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
    };
    let Some(principal) = auth.principal() else {
        return refuse(
            StatusCode::FORBIDDEN,
            "a signed peer call is required".into(),
        );
    };
    if !state
        .network
        .alive_peers()
        .await
        .iter()
        .any(|peer| peer.node_id == principal.node_id)
    {
        return refuse(
            StatusCode::FORBIDDEN,
            "the caller is not a live peer".into(),
        );
    }
    if !valid_op(&op) {
        return refuse(StatusCode::BAD_REQUEST, format!("invalid op {op:?}"));
    }
    match serving_plugin(&load_assignments(&state.database), &deploy_id, &component) {
        Some(serving) if serving == code => {}
        _ => {
            return refuse(
                StatusCode::NOT_FOUND,
                format!("this machine does not serve {component} of {deploy_id}"),
            )
        }
    }
    match call_local_plugin(&state, &code, &deploy_id, &component, &op, body.to_vec()).await {
        Ok((status, bytes)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Err(error) => refuse(StatusCode::BAD_GATEWAY, error),
    }
}

// ── Image generations: the request log and the image budget ─────────────────

/// A generation left open this long is closed `incomplete`.
pub(crate) const JOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// One op a generation ran on a network component.
#[derive(Debug, Clone)]
pub(crate) struct JobCall {
    pub component: String,
    pub deploy_id: String,
    pub name: String,
    pub node_id: String,
    pub hostname: String,
    pub ms: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// A ComfyUI generation that uses network components: one audit record.
#[derive(Debug, Clone)]
pub(crate) struct ImageJob {
    pub audit_id: String,
    pub started: std::time::Instant,
    pub calls: Vec<JobCall>,
    /// The images asked for at the start: what a generation ComfyUI never
    /// reports finished is charged, so it is never free.
    pub images: u64,
}

/// The open generations of this node, by ComfyUI prompt id.
#[derive(Default)]
pub(crate) struct ImageJobs {
    jobs: std::sync::Mutex<std::collections::HashMap<String, ImageJob>>,
}

impl ImageJobs {
    /// Open the generation (the same id again returns the same record), or
    /// refuse it when the budget said so: nothing is recorded then.
    pub(crate) fn begin(
        &self,
        job_id: &str,
        denial: Option<crate::quota::Denial>,
        now: std::time::Instant,
        images: u64,
    ) -> Result<String, crate::quota::Denial> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.get(job_id) {
            return Ok(job.audit_id.clone());
        }
        if let Some(denial) = denial {
            return Err(denial);
        }
        let audit_id = uuid::Uuid::new_v4().simple().to_string();
        jobs.insert(
            job_id.to_owned(),
            ImageJob {
                audit_id: audit_id.clone(),
                started: now,
                calls: Vec::new(),
                images,
            },
        );
        Ok(audit_id)
    }

    pub(crate) fn is_open(&self, job_id: &str) -> bool {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(job_id)
    }

    pub(crate) fn record(&self, job_id: &str, call: JobCall) {
        if let Some(job) = self
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(job_id)
        {
            job.calls.push(call);
        }
    }

    pub(crate) fn take(&self, job_id: &str) -> Option<ImageJob> {
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(job_id)
    }

    /// The generations open longer than `JOB_TIMEOUT` at `now`, removed.
    pub(crate) fn sweep(&self, now: std::time::Instant) -> Vec<(String, ImageJob)> {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let old: Vec<String> = jobs
            .iter()
            .filter(|(_, job)| now.saturating_duration_since(job.started) >= JOB_TIMEOUT)
            .map(|(id, _)| id.clone())
            .collect();
        old.into_iter()
            .filter_map(|id| jobs.remove(&id).map(|job| (id, job)))
            .collect()
    }
}

/// This node's open generations.
pub(crate) fn image_jobs() -> &'static ImageJobs {
    static JOBS: std::sync::OnceLock<ImageJobs> = std::sync::OnceLock::new();
    JOBS.get_or_init(ImageJobs::default)
}

/// The telemetry, hops and source of a finished generation.
pub(crate) fn image_telemetry(
    job: &ImageJob,
    images: u64,
    width: Option<u64>,
    height: Option<u64>,
    steps: Option<u64>,
) -> (Value, Vec<String>, String) {
    let mut hops: Vec<String> = Vec::new();
    let mut parts: Vec<(String, String, Value)> = Vec::new();
    for call in &job.calls {
        if !hops.contains(&call.hostname) {
            hops.push(call.hostname.clone());
        }
        match parts
            .iter_mut()
            .find(|(component, node, _)| *component == call.component && *node == call.node_id)
        {
            Some((_, _, entry)) => {
                for (key, add) in [
                    ("calls", 1),
                    ("ms", call.ms),
                    ("bytes_in", call.bytes_in),
                    ("bytes_out", call.bytes_out),
                ] {
                    entry[key] = serde_json::json!(entry[key].as_u64().unwrap_or(0) + add);
                }
            }
            None => parts.push((
                call.component.clone(),
                call.node_id.clone(),
                serde_json::json!({
                    "component": call.component, "deploy_id": call.deploy_id, "name": call.name,
                    "node_id": call.node_id, "hostname": call.hostname, "calls": 1,
                    "ms": call.ms, "bytes_in": call.bytes_in, "bytes_out": call.bytes_out,
                }),
            )),
        }
    }
    let telemetry = serde_json::json!({
        "kind": "image",
        "images": images,
        "width": width,
        "height": height,
        "steps": steps,
        "components": parts.into_iter().map(|(_, _, entry)| entry).collect::<Vec<_>>(),
        "total_ms": job.started.elapsed().as_millis() as u64,
    });
    let source = format!("components:{}", hops.join(","));
    (telemetry, hops, source)
}

/// The caller an image generation is recorded under: this node's ComfyUI,
/// so the generation belongs to (and is budgeted on) this machine.
pub(crate) fn image_caller(state: &GatewayState, plugin_code: &str) -> crate::reqlog::Caller {
    crate::reqlog::Caller {
        key_id: format!("plugin:{plugin_code}"),
        key_label: "ComfyUI".into(),
        key_node_id: Some(state.network.node_id.clone()),
    }
}

/// Close a generation in the audit.
pub(crate) fn complete_image_job(
    state: &GatewayState,
    job: ImageJob,
    status: crate::reqlog::AuditStatus,
    error: Option<String>,
    images: u64,
    width: Option<u64>,
    height: Option<u64>,
    steps: Option<u64>,
) {
    let (telemetry, hops, source) = image_telemetry(&job, images, width, height, steps);
    state.audit.complete(
        job.audit_id.clone(),
        crate::reqlog::AuditCompletion {
            completed_at: chrono::Utc::now().to_rfc3339(),
            result: None,
            telemetry: Some(telemetry),
            timings: None,
            source,
            hops,
            prompt_tokens: None,
            completion_tokens: None,
            duration_ms: Some(job.started.elapsed().as_millis() as u64),
            status,
            error,
        },
    );
}

/// Close the generations ComfyUI never reported finished.
pub(crate) fn spawn_job_sweeper(state: GatewayState) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            for (_, job) in image_jobs().sweep(std::time::Instant::now()) {
                let images = job.images;
                complete_image_job(
                    &state,
                    job,
                    crate::reqlog::AuditStatus::Incomplete,
                    Some("ComfyUI did not report the end of this generation".into()),
                    images,
                    None,
                    None,
                    None,
                );
            }
        }
    });
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

/// How long a component call reuses the network's component list: a
/// sampling run makes one call per step and branch, and asking every peer
/// for its services each time adds a round trip (or a slow peer's timeout)
/// to every step.
const COMPONENT_LIST_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// A value kept for `ttl` after it was read.
pub(crate) struct Recent<T> {
    ttl: std::time::Duration,
    value: std::sync::Mutex<Option<(std::time::Instant, T)>>,
}

impl<T: Clone> Recent<T> {
    pub(crate) const fn new(ttl: std::time::Duration) -> Self {
        Self {
            ttl,
            value: std::sync::Mutex::new(None),
        }
    }

    /// The kept value, while it is younger than `ttl` at `now`.
    pub(crate) fn get(&self, now: std::time::Instant) -> Option<T> {
        let value = self.value.lock().unwrap_or_else(|e| e.into_inner());
        value
            .as_ref()
            .filter(|(at, _)| now.saturating_duration_since(*at) < self.ttl)
            .map(|(_, value)| value.clone())
    }

    pub(crate) fn put(&self, now: std::time::Instant, value: T) {
        *self.value.lock().unwrap_or_else(|e| e.into_inner()) = Some((now, value));
    }
}

static RECENT_COMPONENTS: Recent<Vec<NetworkComponent>> = Recent::new(COMPONENT_LIST_TTL);

/// The network's components for a component call: the list read in the last
/// few seconds when it names `deploy_id`'s `component`, a fresh one
/// otherwise (a deployment made a moment ago is found at once).
pub(crate) async fn components_for_call(
    state: &GatewayState,
    deploy_id: &str,
    component: &str,
) -> Vec<NetworkComponent> {
    let now = std::time::Instant::now();
    if let Some(list) = RECENT_COMPONENTS.get(now) {
        if list
            .iter()
            .any(|c| c.deploy_id == deploy_id && c.component == component)
        {
            return list;
        }
    }
    let list = network_components(state).await;
    RECENT_COMPONENTS.put(std::time::Instant::now(), list.clone());
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_recent_list_is_reused_only_while_young() {
        let recent: Recent<u32> = Recent::new(std::time::Duration::from_secs(5));
        let start = std::time::Instant::now();
        assert_eq!(recent.get(start), None);
        recent.put(start, 7);
        assert_eq!(
            recent.get(start + std::time::Duration::from_secs(4)),
            Some(7)
        );
        assert_eq!(recent.get(start + std::time::Duration::from_secs(5)), None);
    }

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
            files: files
                .iter()
                .map(|(path, category)| ImageFileFact {
                    path: path.to_string(),
                    size_bytes: 0,
                    category: category.to_string(),
                })
                .collect(),
            service_status: service.then(|| "ready".to_owned()),
            ..Default::default()
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
            // In flow order: text encoder, diffusion, VAE.
            [
                "/api/ui/components/apply b text_encoder",
                "/api/ui/components/apply a diffusion",
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
        let req = request(vec![choice("diffusion", "b", "/m/none.safetensors")]);
        let error = deploy_image(&db, &network(), &call, req).await.unwrap_err();
        assert!(
            error.contains("host-b") && error.contains("none.safetensors"),
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
        assert!(seen.contains(&"/api/ui/components/release a diffusion".to_owned()));
        assert!(seen.contains(&"/api/ui/components/release b text_encoder".to_owned()));
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

    #[tokio::test]
    async fn an_offline_target_keeps_its_part_until_released() {
        let (_dir, db) = db();
        let (call, _) = targets_call(None);
        let req = request(vec![
            choice("diffusion", "a", "/m/z.safetensors"),
            choice("vae", "c", "/m/ae.safetensors"),
        ]);
        let deploy_id = deploy_image(&db, &network(), &call, req).await.unwrap();
        // `c` does not answer: `a` is released, `c` stays on record.
        let call: TargetCall = Arc::new(|node_id: String, _path: &'static str, _body: Value| {
            Box::pin(async move {
                if node_id == "c" {
                    Err("host-c did not answer".to_owned())
                } else {
                    Ok(())
                }
            })
        });
        let released = undeploy_image(&db, &call, &deploy_id).await;
        assert_eq!(released.iter().filter(|r| r.ok).count(), 1);
        let left = load_image_deployments(&db);
        assert_eq!(left.len(), 1, "the screen still lists it for a retry");
        assert_eq!(left[0].components.len(), 1);
        assert_eq!(left[0].components[0].node_id, "c");
        // Undeploying again once `c` answers drops the record.
        let (call, _) = targets_call(None);
        undeploy_image(&db, &call, &deploy_id).await;
        assert!(load_image_deployments(&db).is_empty());
    }

    fn served(deploy_id: &str, component: &str, node_id: &str) -> NetworkComponent {
        NetworkComponent {
            deploy_id: deploy_id.into(),
            name: "Z-Image".into(),
            component: component.into(),
            files: vec![],
            status: "ready".into(),
            node_id: node_id.into(),
            hostname: format!("host-{node_id}"),
            plugin_code: "comfyui".into(),
        }
    }

    type ForwardLog = Arc<Mutex<Vec<(String, String, usize)>>>;

    fn forwarding(answer: Result<(u16, Vec<u8>), String>) -> (Forward, ForwardLog) {
        let seen: ForwardLog = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let forward: Forward = Arc::new(
            move |target: NetworkComponent, op: String, body: Vec<u8>, _job: Option<String>| {
                log.lock()
                    .unwrap()
                    .push((target.node_id.clone(), op, body.len()));
                let answer = answer.clone();
                Box::pin(async move { answer })
            },
        );
        (forward, seen)
    }

    #[tokio::test]
    async fn remote_calls_reach_the_serving_node() {
        let network = vec![
            served("img-1", "vae", "c"),
            served("img-1", "diffusion", "a"),
        ];
        let (forward, seen) = forwarding(Ok((200, b"out".to_vec())));
        let (status, body) = call_component(
            &network,
            "img-1",
            "diffusion",
            "forward",
            b"in".to_vec(),
            None,
            &forward,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body, b"out");
        assert_eq!(
            *seen.lock().unwrap(),
            [("a".to_owned(), "forward".to_owned(), 2)]
        );
    }

    #[tokio::test]
    async fn an_unserved_component_is_not_found() {
        let (forward, seen) = forwarding(Ok((200, vec![])));
        let (status, body) = call_component(
            &[served("img-1", "vae", "c")],
            "img-2",
            "vae",
            "decode",
            vec![],
            None,
            &forward,
        )
        .await;
        assert_eq!(status, 404);
        assert!(String::from_utf8_lossy(&body).contains("no machine serves vae of img-2"));
        assert!(seen.lock().unwrap().is_empty());
        let (status, _) = call_component(
            &[served("img-1", "vae", "c")],
            "img-1",
            "vae",
            "../x",
            vec![],
            None,
            &forward,
        )
        .await;
        assert_eq!(status, 400, "an op is a short lowercase word");
    }

    #[tokio::test]
    async fn a_vanished_server_is_a_named_error() {
        let (forward, _) = forwarding(Err("connection refused".into()));
        let (status, body) = call_component(
            &[served("img-1", "vae", "c")],
            "img-1",
            "vae",
            "decode",
            vec![],
            None,
            &forward,
        )
        .await;
        assert_eq!(status, 502);
        let text = String::from_utf8_lossy(&body);
        assert!(
            text.contains("host-c") && text.contains("vae") && text.contains("connection refused"),
            "{text}"
        );
    }

    #[test]
    fn unknown_components_are_refused() {
        let assignments = vec![ComponentAssignment {
            deploy_id: "img-1".into(),
            name: "Z".into(),
            component: "vae".into(),
            paths: vec!["/m/ae.safetensors".into()],
            plugin_code: "comfyui".into(),
            options: Value::Null,
        }];
        assert_eq!(
            serving_plugin(&assignments, "img-1", "vae"),
            Some("comfyui".to_owned())
        );
        assert_eq!(serving_plugin(&assignments, "img-1", "diffusion"), None);
        assert_eq!(serving_plugin(&assignments, "img-2", "vae"), None);
    }

    #[tokio::test]
    async fn oversized_bodies_are_refused() {
        use tower::ServiceExt;
        let reached = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&reached);
        let app = axum::Router::new().route(
            "/x",
            axum::routing::post(move |_body: axum::body::Bytes| {
                *flag.lock().unwrap() = true;
                async { "ok" }
            })
            .layer(axum::extract::DefaultBodyLimit::max(MAX_COMPONENT_BODY)),
        );
        let request = axum::http::Request::post("/x")
            .body(axum::body::Body::from(vec![0u8; MAX_COMPONENT_BODY + 1]))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!*reached.lock().unwrap());
    }

    fn job_call(component: &str, host: &str) -> JobCall {
        JobCall {
            component: component.into(),
            deploy_id: "img-1".into(),
            name: "Z-Image".into(),
            node_id: format!("id-{host}"),
            hostname: host.into(),
            ms: 10,
            bytes_in: 100,
            bytes_out: 200,
        }
    }

    #[test]
    fn an_image_generation_is_logged_once_with_its_telemetry() {
        let jobs = ImageJobs::default();
        let now = std::time::Instant::now();
        let first = jobs.begin("p1", None, now, 1).unwrap();
        assert_eq!(
            jobs.begin("p1", None, now, 1).unwrap(),
            first,
            "begin is idempotent per job"
        );
        jobs.record("p1", job_call("text_encoder", "gpu-b"));
        jobs.record("p1", job_call("diffusion", "gpu-a"));
        jobs.record("p1", job_call("diffusion", "gpu-a"));
        jobs.record("p1", job_call("vae", "gpu-c"));
        jobs.record("unknown", job_call("vae", "gpu-c"));
        let job = jobs.take("p1").unwrap();
        assert_eq!(job.audit_id, first);
        assert!(jobs.take("p1").is_none(), "taken once");
        let (telemetry, hops, source) = image_telemetry(&job, 1, Some(1024), Some(1024), Some(8));
        assert_eq!(hops, ["gpu-b", "gpu-a", "gpu-c"]);
        assert_eq!(source, "components:gpu-b,gpu-a,gpu-c");
        assert_eq!(telemetry["kind"], "image");
        assert_eq!(telemetry["images"], 1);
        assert_eq!(telemetry["steps"], 8);
        let components = telemetry["components"].as_array().unwrap();
        assert_eq!(components.len(), 3);
        assert_eq!(components[1]["component"], "diffusion");
        assert_eq!(components[1]["calls"], 2);
        assert_eq!(components[1]["bytes_out"], 400);
    }

    #[test]
    fn a_spent_image_budget_refuses_before_any_gpu() {
        let jobs = ImageJobs::default();
        let denial = crate::quota::Denial {
            message: "Image budget exhausted".into(),
            retry_after_secs: Some(60),
        };
        let refused = jobs
            .begin("p1", Some(denial), std::time::Instant::now(), 1)
            .unwrap_err();
        assert!(refused.message.contains("Image budget"));
        jobs.record("p1", job_call("vae", "gpu-c"));
        assert!(jobs.take("p1").is_none(), "no job, nothing recorded");
    }

    #[test]
    fn the_check_carries_the_image_estimate() {
        let caller = crate::reqlog::Caller {
            key_id: "plugin:comfyui".into(),
            key_label: "ComfyUI".into(),
            key_node_id: Some("n".into()),
        };
        let image = crate::quota::check_params(
            "n",
            &caller,
            "a cat",
            2,
            &json!({"kind": "image", "images": 3}),
        );
        assert_eq!(image["images"], 3);
        let chat = crate::quota::check_params("n", &caller, "hi", 1, &json!({"messages": []}));
        assert!(chat.get("images").is_none());
    }

    #[test]
    fn an_unfinished_generation_is_closed_incomplete() {
        let jobs = ImageJobs::default();
        let start = std::time::Instant::now();
        jobs.begin("old", None, start, 2).unwrap();
        jobs.begin(
            "new",
            None,
            start + std::time::Duration::from_secs(29 * 60),
            1,
        )
        .unwrap();
        let swept = jobs.sweep(start + std::time::Duration::from_secs(31 * 60));
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].0, "old");
        assert_eq!(
            swept[0].1.images, 2,
            "a swept generation keeps its estimate"
        );
        assert!(jobs.take("new").is_some());
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
