//! Planning an image deployment by stage: which machines may serve each stage
//! (text encoder, diffusion, VAE), why the others may not, what each machine
//! would load and which copies the plan needs. Pure: the route surveys the
//! network and hands the facts in.

use serde::{Deserialize, Serialize};

use crate::components::MachineFacts;
use crate::image_records::{FileIdentity, STAGES};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StageRequest {
    pub component: String,
    pub file: FileIdentity,
    #[serde(default)]
    pub node_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ImagePlanRequest {
    pub name: String,
    pub stages: Vec<StageRequest>,
    #[serde(default)]
    pub allow_transfers: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MachineVerdict {
    pub node_id: String,
    pub hostname: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The file on this machine's disk; absent when it would receive a copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct StagePlan {
    pub component: String,
    pub file: FileIdentity,
    pub chosen: Vec<MachineVerdict>,
    pub alternatives: Vec<MachineVerdict>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MachineMemory {
    pub node_id: String,
    pub gpu_free_mb: u64,
    pub ram_free_mb: u64,
    /// The sum of the files it would load, a rough need.
    pub need_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Copy {
    pub component: String,
    pub file: FileIdentity,
    pub from_node_id: String,
    pub to_node_id: String,
    /// The source path on `from_node_id`.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ImagePlan {
    pub name: String,
    pub stages: Vec<StagePlan>,
    pub memory: Vec<MachineMemory>,
    pub copies: Vec<Copy>,
    pub blocked: Vec<String>,
}

/// A category a stage accepts: its own, or a checkpoint holding all three.
pub(crate) fn fits(component: &str, category: &str) -> bool {
    category == component || category == "checkpoint"
}

fn base_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned())
}

/// The machine's copy of `file`: the same name, and the same size when the
/// request knows it.
fn holding<'a>(
    machine: &'a MachineFacts,
    file: &FileIdentity,
) -> Option<&'a crate::components::ImageFileFact> {
    machine.files.iter().find(|f| {
        base_name(&f.path) == file.name && (file.size_bytes == 0 || f.size_bytes == file.size_bytes)
    })
}

/// Whether `machine` can serve `component` from `file`, and the holder a copy
/// would come from when it does not hold it itself.
fn verdict(
    machine: &MachineFacts,
    component: &str,
    file: &FileIdentity,
    machines: &[MachineFacts],
) -> (MachineVerdict, Option<Copy>) {
    let refuse = |reason: String| {
        (
            MachineVerdict {
                node_id: machine.node_id.clone(),
                hostname: machine.hostname.clone(),
                ok: false,
                reason: Some(reason),
                path: None,
            },
            None,
        )
    };
    match machine.service_status.as_deref() {
        None => return refuse("does not run ComfyUI".into()),
        Some(status @ ("error" | "waiting")) => return refuse(format!("ComfyUI is {status}")),
        Some(_) => {}
    }
    if let Some(found) = holding(machine, file) {
        if !fits(component, &found.category) {
            return refuse(format!(
                "{} is a {}, not a {component}",
                file.name, found.category
            ));
        }
        return (
            MachineVerdict {
                node_id: machine.node_id.clone(),
                hostname: machine.hostname.clone(),
                ok: true,
                reason: None,
                path: Some(found.path.clone()),
            },
            None,
        );
    }
    if let Some(other) = machine.files.iter().find(|f| base_name(&f.path) == file.name) {
        return refuse(format!(
            "holds another {} ({} bytes)",
            file.name, other.size_bytes
        ));
    }
    let holder = machines.iter().find_map(|m| {
        holding(m, file)
            .filter(|f| fits(component, &f.category))
            .map(|f| (m.node_id.clone(), f.path.clone()))
    });
    match holder {
        Some((from_node_id, path)) => (
            MachineVerdict {
                node_id: machine.node_id.clone(),
                hostname: machine.hostname.clone(),
                ok: true,
                reason: Some(format!("would receive {}", file.name)),
                path: None,
            },
            Some(Copy {
                component: component.to_owned(),
                file: file.clone(),
                from_node_id,
                to_node_id: machine.node_id.clone(),
                path,
            }),
        ),
        None => refuse(format!("does not hold {}", file.name)),
    }
}

pub(crate) fn plan_image(request: &ImagePlanRequest, machines: &[MachineFacts]) -> ImagePlan {
    let mut blocked = Vec::new();
    if request.name.trim().is_empty() {
        blocked.push("an image deployment needs a name".to_owned());
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut requests: Vec<&StageRequest> = Vec::new();
    for stage in &request.stages {
        if !STAGES.contains(&stage.component.as_str()) {
            blocked.push(format!("unknown component {}", stage.component));
            continue;
        }
        if !seen.insert(stage.component.as_str()) {
            blocked.push(format!("{} is chosen twice", stage.component));
            continue;
        }
        requests.push(stage);
    }
    requests.sort_by_key(|stage| STAGES.iter().position(|s| *s == stage.component));

    let mut stages = Vec::new();
    let mut copies = Vec::new();
    let mut needs: Vec<(String, u64)> = Vec::new();
    for stage in requests {
        if stage.node_ids.is_empty() {
            blocked.push(format!("{} has no machine", stage.component));
        }
        let mut chosen = Vec::new();
        for node_id in &stage.node_ids {
            let Some(machine) = machines.iter().find(|m| &m.node_id == node_id) else {
                chosen.push(MachineVerdict {
                    node_id: node_id.clone(),
                    hostname: node_id.clone(),
                    ok: false,
                    reason: Some("is not on the network".into()),
                    path: None,
                });
                blocked.push(format!("{node_id} is not on the network"));
                continue;
            };
            let (verdict, copy) = verdict(machine, &stage.component, &stage.file, machines);
            if let Some(reason) = verdict.reason.as_ref().filter(|_| !verdict.ok) {
                blocked.push(format!(
                    "{} cannot serve the {}: {reason}",
                    machine.hostname, stage.component
                ));
            }
            if verdict.ok {
                let mb = stage.file.size_bytes.div_ceil(1024 * 1024);
                match needs.iter_mut().find(|(id, _)| id == node_id) {
                    Some((_, need)) => *need += mb,
                    None => needs.push((node_id.clone(), mb)),
                }
            }
            copies.extend(copy);
            chosen.push(verdict);
        }
        let alternatives = machines
            .iter()
            .map(|m| verdict(m, &stage.component, &stage.file, machines).0)
            .collect();
        stages.push(StagePlan {
            component: stage.component.clone(),
            file: stage.file.clone(),
            chosen,
            alternatives,
        });
    }
    if !copies.is_empty() && !request.allow_transfers {
        blocked.push("copies need your permission".to_owned());
    }
    let memory = needs
        .into_iter()
        .filter_map(|(node_id, need_mb)| {
            let machine = machines.iter().find(|m| m.node_id == node_id)?;
            Some(MachineMemory {
                node_id,
                gpu_free_mb: machine.gpu_free_mb,
                ram_free_mb: machine.ram_free_mb,
                need_mb,
            })
        })
        .collect();
    ImagePlan {
        name: request.name.trim().to_owned(),
        stages,
        memory,
        copies,
        blocked,
    }
}

// ── Deploying the stages ─────────────────────────────────────────────────────

const APPLY_PATH: &str = "/api/ui/components/apply";
const RELEASE_PATH: &str = "/api/ui/components/release";

/// The body of `POST /api/ui/image-deploy`: the stage form, or the form from
/// before the stages (one machine per component), which still deploys.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum DeployBody {
    Stages(ImagePlanRequest),
    Components(crate::components::ImageDeployRequest),
}

impl DeployBody {
    pub(crate) fn into_request(self) -> ImagePlanRequest {
        match self {
            DeployBody::Stages(request) => request,
            DeployBody::Components(old) => {
                let mut stages: Vec<StageRequest> = Vec::new();
                for choice in old.components {
                    // A component chosen twice stays twice: the plan refuses it.
                    stages.push(StageRequest {
                        component: choice.component.clone(),
                        file: FileIdentity { name: base_name(&choice.path), size_bytes: 0 },
                        node_ids: vec![choice.node_id],
                    });
                }
                ImagePlanRequest { name: old.name, stages, allow_transfers: false }
            }
        }
    }
}

/// Why a plan cannot be deployed, as the route answers it: `None` when it can.
pub(crate) fn refusal(plan: &ImagePlan, allow_transfers: bool) -> Option<(u16, serde_json::Value)> {
    if !plan.copies.is_empty() && !allow_transfers {
        return Some((
            409,
            serde_json::json!({ "error": "transfers_not_authorized", "copies": plan.copies }),
        ));
    }
    if !plan.copies.is_empty() {
        return Some((
            400,
            serde_json::json!({ "error": "image files cannot be copied between machines yet; place the file on the machine first" }),
        ));
    }
    plan.blocked
        .first()
        .map(|reason| (400, serde_json::json!({ "error": reason })))
}

/// Applies every (stage, replica) of `plan` under `deploy_id`. A replica that
/// fails is released and reported; when a stage is left with no replica,
/// everything applied is released and the deploy fails. Only a deploy that
/// succeeds is recorded. Returns the replicas that failed.
pub(crate) async fn deploy_stages(
    db: &crate::db::PomDb,
    plan: &ImagePlan,
    call: &crate::components::TargetCall,
    deploy_id: &str,
    by: &str,
    progress: &(dyn Fn(f64) + Send + Sync),
) -> Result<Vec<MachineVerdict>, String> {
    use crate::image_records::{ImageDeployment, ImageStage, Replica};
    let total: usize = plan.stages.iter().map(|s| s.chosen.len()).sum::<usize>().max(1);
    let mut done = 0usize;
    let mut applied: Vec<(String, String)> = Vec::new(); // (node_id, component)
    let mut failed: Vec<MachineVerdict> = Vec::new();
    let mut stages: Vec<ImageStage> = Vec::new();
    for stage in &plan.stages {
        let mut replicas = Vec::new();
        let mut errors = Vec::new();
        for verdict in &stage.chosen {
            let Some(path) = verdict.path.clone() else { continue };
            let choice = crate::components::ComponentChoice {
                component: stage.component.clone(),
                node_id: verdict.node_id.clone(),
                path: path.clone(),
            };
            let body = crate::components::assignment_of(deploy_id, &plan.name, &choice);
            match call(verdict.node_id.clone(), APPLY_PATH, body).await {
                Ok(()) => {
                    applied.push((verdict.node_id.clone(), stage.component.clone()));
                    replicas.push(Replica {
                        node_id: verdict.node_id.clone(),
                        hostname: verdict.hostname.clone(),
                        path,
                    });
                }
                Err(error) => {
                    let _ = call(
                        verdict.node_id.clone(),
                        RELEASE_PATH,
                        serde_json::json!({ "deploy_id": deploy_id, "component": stage.component }),
                    )
                    .await;
                    errors.push(format!("{}: {error}", verdict.hostname));
                    failed.push(MachineVerdict {
                        ok: false,
                        reason: Some(error),
                        ..verdict.clone()
                    });
                }
            }
            done += 1;
            progress(100.0 * done as f64 / total as f64);
        }
        if replicas.is_empty() {
            for (node_id, component) in &applied {
                let _ = call(
                    node_id.clone(),
                    RELEASE_PATH,
                    serde_json::json!({ "deploy_id": deploy_id, "component": component }),
                )
                .await;
            }
            return Err(format!(
                "{}: no machine could serve it ({})",
                stage.component,
                errors.join("; ")
            ));
        }
        stages.push(ImageStage {
            component: stage.component.clone(),
            file: stage.file.clone(),
            replicas,
        });
    }
    let mut deployments = crate::image_records::load_image_deployments(db);
    deployments.retain(|d| d.deploy_id != deploy_id);
    deployments.push(ImageDeployment::new(
        deploy_id.to_owned(),
        plan.name.clone(),
        by,
        None,
        stages,
    ));
    crate::image_records::save_image_deployments(db, &deployments);
    Ok(failed)
}

pub(crate) fn new_deploy_id() -> String {
    format!("img-{}", hex::encode(rand::random::<[u8; 6]>()))
}

// ── Changing a live deployment ───────────────────────────────────────────────

/// A refusal of a change: the HTTP status and the JSON body.
pub(crate) type Refused = (u16, serde_json::Value);

fn refused(status: u16, error: impl Into<String>) -> Refused {
    (status, serde_json::json!({ "error": error.into() }))
}

fn record_of(
    db: &crate::db::PomDb,
    deploy_id: &str,
) -> Result<(Vec<crate::image_records::ImageDeployment>, usize), Refused> {
    let deployments = crate::image_records::load_image_deployments(db);
    let index = deployments
        .iter()
        .position(|d| d.deploy_id == deploy_id)
        .ok_or_else(|| refused(404, "this deployment was made from another machine"))?;
    Ok((deployments, index))
}

/// Adds `node_id` as a replica of `component` and appends a version.
pub(crate) async fn add_replica(
    db: &crate::db::PomDb,
    machines: &[MachineFacts],
    call: &crate::components::TargetCall,
    deploy_id: &str,
    component: &str,
    node_id: &str,
    by: &str,
) -> Result<u32, Refused> {
    let (mut deployments, index) = record_of(db, deploy_id)?;
    let record = &deployments[index];
    let stage = record
        .active()
        .stages
        .iter()
        .find(|s| s.component == component)
        .cloned()
        .ok_or_else(|| refused(400, format!("this deployment has no {component}")))?;
    if stage.replicas.iter().any(|r| r.node_id == node_id) {
        return Err(refused(400, format!("that machine already serves the {component}")));
    }
    let request = ImagePlanRequest {
        name: record.name.clone(),
        stages: vec![StageRequest {
            component: component.to_owned(),
            file: stage.file.clone(),
            node_ids: vec![node_id.to_owned()],
        }],
        allow_transfers: false,
    };
    let plan = plan_image(&request, machines);
    let verdict = plan.stages[0].chosen[0].clone();
    let path = match (&verdict.path, verdict.ok) {
        (Some(path), true) => path.clone(),
        _ => {
            return Err(refused(
                400,
                verdict
                    .reason
                    .unwrap_or_else(|| "that machine cannot serve it".into()),
            ))
        }
    };
    let choice = crate::components::ComponentChoice {
        component: component.to_owned(),
        node_id: node_id.to_owned(),
        path: path.clone(),
    };
    let body = crate::components::assignment_of(deploy_id, &record.name, &choice);
    call(node_id.to_owned(), APPLY_PATH, body)
        .await
        .map_err(|error| refused(502, format!("{}: {error}", verdict.hostname)))?;
    let mut stages = record.active().stages.clone();
    if let Some(target) = stages.iter_mut().find(|s| s.component == component) {
        target.replicas.push(crate::image_records::Replica {
            node_id: node_id.to_owned(),
            hostname: verdict.hostname.clone(),
            path,
        });
    }
    let version = deployments[index].push_version(stages, by);
    crate::image_records::save_image_deployments(db, &deployments);
    Ok(version)
}

/// Releases `component` on `node_id` and appends a version. The last replica
/// of a stage needs `confirm_gap`. Answers the version and whether every
/// stage still has a replica.
pub(crate) async fn remove_replica(
    db: &crate::db::PomDb,
    call: &crate::components::TargetCall,
    deploy_id: &str,
    component: &str,
    node_id: &str,
    confirm_gap: bool,
    by: &str,
) -> Result<(u32, bool), Refused> {
    let (mut deployments, index) = record_of(db, deploy_id)?;
    let mut stages = deployments[index].active().stages.clone();
    let stage = stages
        .iter_mut()
        .find(|s| s.component == component)
        .ok_or_else(|| refused(400, format!("this deployment has no {component}")))?;
    if !stage.replicas.iter().any(|r| r.node_id == node_id) {
        return Err(refused(404, format!("that machine does not serve the {component}")));
    }
    if stage.replicas.len() == 1 && !confirm_gap {
        return Err((
            409,
            serde_json::json!({ "error": "gap", "component": component }),
        ));
    }
    call(
        node_id.to_owned(),
        RELEASE_PATH,
        serde_json::json!({ "deploy_id": deploy_id, "component": component }),
    )
    .await
    .map_err(|error| refused(502, error))?;
    stage.replicas.retain(|r| r.node_id != node_id);
    let serving = stages.iter().all(|s| !s.replicas.is_empty());
    let version = deployments[index].push_version(stages, by);
    crate::image_records::save_image_deployments(db, &deployments);
    Ok((version, serving))
}

/// Brings `version` back: releases what it does not have, applies what it
/// has and makes it the live version, without appending one.
pub(crate) async fn redeploy_version(
    db: &crate::db::PomDb,
    call: &crate::components::TargetCall,
    deploy_id: &str,
    version: u32,
) -> Result<u32, Refused> {
    let (mut deployments, index) = record_of(db, deploy_id)?;
    let record = &deployments[index];
    let target = record
        .versions
        .iter()
        .find(|v| v.version == version)
        .cloned()
        .ok_or_else(|| refused(400, format!("version {version} does not exist")))?;
    let pairs = |stages: &[crate::image_records::ImageStage]| -> Vec<(String, String, String)> {
        stages
            .iter()
            .flat_map(|s| {
                s.replicas
                    .iter()
                    .map(|r| (s.component.clone(), r.node_id.clone(), r.path.clone()))
            })
            .collect()
    };
    let live = pairs(&record.active().stages);
    let wanted = pairs(&target.stages);
    for (component, node_id, _) in live.iter().filter(|p| !wanted.iter().any(|w| w.0 == p.0 && w.1 == p.1)) {
        let _ = call(
            node_id.clone(),
            RELEASE_PATH,
            serde_json::json!({ "deploy_id": deploy_id, "component": component }),
        )
        .await;
    }
    for (component, node_id, path) in &wanted {
        let choice = crate::components::ComponentChoice {
            component: component.clone(),
            node_id: node_id.clone(),
            path: path.clone(),
        };
        let body = crate::components::assignment_of(deploy_id, &record.name, &choice);
        call(node_id.clone(), APPLY_PATH, body)
            .await
            .map_err(|error| refused(502, format!("{node_id}: {error}")))?;
    }
    deployments[index].active_version = version;
    crate::image_records::save_image_deployments(db, &deployments);
    Ok(version)
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReplicaChange {
    pub component: String,
    pub node_id: String,
    #[serde(default)]
    pub confirm_gap: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct VersionChoice {
    pub version: u32,
}

fn answer(result: Result<serde_json::Value, Refused>) -> axum::response::Response {
    use axum::response::IntoResponse;
    match result {
        Ok(body) => axum::Json(body).into_response(),
        Err((status, body)) => (
            axum::http::StatusCode::from_u16(status).unwrap_or(axum::http::StatusCode::BAD_REQUEST),
            axum::Json(body),
        )
            .into_response(),
    }
}

/// `POST /api/ui/image-deploy/:deploy_id/add`.
pub(crate) async fn add_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(deploy_id): axum::extract::Path<String>,
    axum::Json(change): axum::Json<ReplicaChange>,
) -> axum::response::Response {
    let machines = crate::components::survey_machines(&state).await;
    let call = crate::components::live_targets(state.clone());
    let by = state.network.node_id.clone();
    answer(
        add_replica(&state.database, &machines, &call, &deploy_id, &change.component, &change.node_id, &by)
            .await
            .map(|version| serde_json::json!({ "version": version })),
    )
}

/// `POST /api/ui/image-deploy/:deploy_id/remove`.
pub(crate) async fn remove_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(deploy_id): axum::extract::Path<String>,
    axum::Json(change): axum::Json<ReplicaChange>,
) -> axum::response::Response {
    let call = crate::components::live_targets(state.clone());
    let by = state.network.node_id.clone();
    answer(
        remove_replica(&state.database, &call, &deploy_id, &change.component, &change.node_id, change.confirm_gap, &by)
            .await
            .map(|(version, serving)| serde_json::json!({ "version": version, "serving": serving })),
    )
}

/// `POST /api/ui/image-deploy/:deploy_id/redeploy`.
pub(crate) async fn redeploy_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(deploy_id): axum::extract::Path<String>,
    axum::Json(choice): axum::Json<VersionChoice>,
) -> axum::response::Response {
    let call = crate::components::live_targets(state.clone());
    answer(
        redeploy_version(&state.database, &call, &deploy_id, choice.version)
            .await
            .map(|version| serde_json::json!({ "version": version })),
    )
}

/// `POST /api/ui/image-deploy/plan`.
pub(crate) async fn plan_route(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::Json(request): axum::Json<ImagePlanRequest>,
) -> axum::Json<ImagePlan> {
    let machines = crate::components::survey_machines(&state).await;
    axum::Json(plan_image(&request, &machines))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::ImageFileFact;

    const GB: u64 = 1024 * 1024 * 1024;

    fn machine(node_id: &str, files: &[(&str, u64, &str)], status: Option<&str>) -> MachineFacts {
        MachineFacts {
            node_id: node_id.into(),
            hostname: format!("host-{node_id}"),
            files: files
                .iter()
                .map(|(path, size, category)| ImageFileFact {
                    path: (*path).into(),
                    size_bytes: *size,
                    category: (*category).into(),
                })
                .collect(),
            service_status: status.map(str::to_owned),
            has_image_service: status.is_some(),
            gpu_free_mb: 16000,
            ram_free_mb: 64000,
            ..Default::default()
        }
    }

    fn network() -> Vec<MachineFacts> {
        vec![
            machine(
                "a",
                &[
                    ("/m/qwen.safetensors", 8 * GB, "text_encoder"),
                    ("/m/ae.safetensors", GB / 3, "vae"),
                ],
                Some("ready"),
            ),
            machine("b", &[("/m/z.safetensors", 12 * GB, "diffusion")], Some("ready")),
            machine("c", &[("/m/z.safetensors", 12 * GB, "diffusion")], Some("ready")),
            machine("d", &[("/m/z.safetensors", 12 * GB, "diffusion")], None),
        ]
    }

    fn file(name: &str, size: u64) -> FileIdentity {
        FileIdentity {
            name: name.into(),
            size_bytes: size,
        }
    }

    fn stage(component: &str, f: FileIdentity, nodes: &[&str]) -> StageRequest {
        StageRequest {
            component: component.into(),
            file: f,
            node_ids: nodes.iter().map(|n| (*n).into()).collect(),
        }
    }

    fn zimage(diffusion: &[&str]) -> ImagePlanRequest {
        ImagePlanRequest {
            name: "z".into(),
            stages: vec![
                stage("vae", file("ae.safetensors", GB / 3), &["a"]),
                stage("diffusion", file("z.safetensors", 12 * GB), diffusion),
                stage("text_encoder", file("qwen.safetensors", 8 * GB), &["a"]),
            ],
            allow_transfers: false,
        }
    }

    #[test]
    fn the_plan_orders_stages_by_flow() {
        let plan = plan_image(&zimage(&["b"]), &network());
        let order: Vec<_> = plan.stages.iter().map(|s| s.component.as_str()).collect();
        assert_eq!(order, ["text_encoder", "diffusion", "vae"]);
        assert!(plan.blocked.is_empty(), "{:?}", plan.blocked);
    }

    #[test]
    fn a_machine_on_two_stages_is_fine() {
        let plan = plan_image(&zimage(&["b"]), &network());
        assert!(plan.stages[0].chosen[0].ok && plan.stages[2].chosen[0].ok);
        assert_eq!(plan.stages[2].chosen[0].path.as_deref(), Some("/m/ae.safetensors"));
    }

    #[test]
    fn two_machines_on_one_stage_are_replicas() {
        let plan = plan_image(&zimage(&["b", "c"]), &network());
        assert_eq!(plan.stages[1].chosen.len(), 2);
        assert!(plan.stages[1].chosen.iter().all(|v| v.ok));
        assert!(plan.blocked.is_empty());
    }

    #[test]
    fn an_empty_stage_blocks() {
        let plan = plan_image(&zimage(&[]), &network());
        assert!(plan.blocked.contains(&"diffusion has no machine".to_owned()));
    }

    #[test]
    fn a_machine_without_comfyui_is_refused_with_its_reason() {
        let plan = plan_image(&zimage(&["d"]), &network());
        assert_eq!(
            plan.stages[1].chosen[0].reason.as_deref(),
            Some("does not run ComfyUI")
        );
        assert!(!plan.blocked.is_empty());
        let mut net = network();
        net[1].service_status = Some("error".into());
        let plan = plan_image(&zimage(&["b"]), &net);
        assert_eq!(plan.stages[1].chosen[0].reason.as_deref(), Some("ComfyUI is error"));
    }

    #[test]
    fn the_same_name_with_another_size_is_another_file() {
        let mut net = network();
        net[2].files[0].size_bytes = 11 * GB;
        let plan = plan_image(&zimage(&["c"]), &net);
        let verdict = &plan.stages[1].chosen[0];
        assert!(!verdict.ok);
        assert_eq!(
            verdict.reason.as_deref(),
            Some(format!("holds another z.safetensors ({} bytes)", 11 * GB).as_str())
        );
    }

    #[test]
    fn a_wrong_category_is_refused() {
        let mut request = zimage(&["b"]);
        request.stages[0].file = file("z.safetensors", 12 * GB);
        request.stages[0].node_ids = vec!["b".into()];
        let plan = plan_image(&request, &network());
        let vae = plan.stages.iter().find(|s| s.component == "vae").unwrap();
        assert_eq!(
            vae.chosen[0].reason.as_deref(),
            Some("z.safetensors is a diffusion, not a vae")
        );
    }

    #[test]
    fn a_checkpoint_fits_every_stage() {
        let net = vec![machine("e", &[("/m/sdxl.safetensors", 7 * GB, "checkpoint")], Some("ready"))];
        let request = ImagePlanRequest {
            name: "sdxl".into(),
            stages: STAGES
                .iter()
                .map(|c| stage(c, file("sdxl.safetensors", 7 * GB), &["e"]))
                .collect(),
            allow_transfers: false,
        };
        let plan = plan_image(&request, &net);
        assert!(plan.blocked.is_empty(), "{:?}", plan.blocked);
    }

    #[test]
    fn alternatives_list_every_machine_with_a_reason() {
        let plan = plan_image(&zimage(&["b"]), &network());
        let alternatives = &plan.stages[1].alternatives;
        assert_eq!(alternatives.len(), 4);
        let a = alternatives.iter().find(|v| v.node_id == "a").unwrap();
        assert!(a.ok, "a can receive a copy");
        let d = alternatives.iter().find(|v| v.node_id == "d").unwrap();
        assert_eq!(d.reason.as_deref(), Some("does not run ComfyUI"));
    }

    #[test]
    fn a_holder_elsewhere_becomes_a_copy() {
        let plan = plan_image(&zimage(&["a"]), &network());
        assert_eq!(plan.copies.len(), 1);
        assert_eq!(plan.copies[0].to_node_id, "a");
        assert_eq!(plan.copies[0].from_node_id, "b");
        assert!(plan.stages[1].chosen[0].ok);
        assert!(plan.blocked.contains(&"copies need your permission".to_owned()));
        let mut allowed = zimage(&["a"]);
        allowed.allow_transfers = true;
        assert!(plan_image(&allowed, &network()).blocked.is_empty());
    }

    #[test]
    fn memory_sums_the_files_a_machine_loads() {
        let plan = plan_image(&zimage(&["b"]), &network());
        let a = plan.memory.iter().find(|m| m.node_id == "a").unwrap();
        assert_eq!(a.need_mb, 8 * 1024 + (GB / 3).div_ceil(1024 * 1024));
        assert_eq!(a.gpu_free_mb, 16000);
    }

    #[test]
    fn unknown_and_repeated_stages_block() {
        let mut request = zimage(&["b"]);
        request.stages.push(stage("lora", file("x", 1), &["a"]));
        request.stages.push(stage("vae", file("ae.safetensors", GB / 3), &["a"]));
        let plan = plan_image(&request, &network());
        assert!(plan.blocked.contains(&"unknown component lora".to_owned()));
        assert!(plan.blocked.contains(&"vae is chosen twice".to_owned()));
    }

    // ── deploy_stages ──

    type Calls = std::sync::Arc<std::sync::Mutex<Vec<(String, &'static str, serde_json::Value)>>>;

    fn targets(fail_on: &'static [&'static str]) -> (crate::components::TargetCall, Calls) {
        let calls: Calls = Default::default();
        let seen = calls.clone();
        let call: crate::components::TargetCall = std::sync::Arc::new(move |node: String, path: &'static str, body: serde_json::Value| {
            seen.lock().unwrap().push((node.clone(), path, body));
            let fail = fail_on.contains(&node.as_str()) && path == APPLY_PATH;
            Box::pin(async move { if fail { Err("offline".to_owned()) } else { Ok(()) } })
        });
        (call, calls)
    }

    fn db() -> (tempfile::TempDir, crate::db::PomDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::PomDb::open(dir.path().join("pom.db")).unwrap();
        (dir, db)
    }

    fn applies(calls: &Calls) -> Vec<(String, String)> {
        calls.lock().unwrap().iter()
            .filter(|(_, path, _)| *path == APPLY_PATH)
            .map(|(node, _, body)| (node.clone(), body["component"].as_str().unwrap().to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn replicas_are_all_applied() {
        let (_dir, db) = db();
        let plan = plan_image(&zimage(&["b", "c"]), &network());
        let (call, calls) = targets(&[]);
        let failed = deploy_stages(&db, &plan, &call, "img-1", "me", &|_| {}).await.unwrap();
        assert!(failed.is_empty());
        let diffusion: Vec<_> = applies(&calls).into_iter().filter(|(_, c)| c == "diffusion").map(|(n, _)| n).collect();
        assert_eq!(diffusion, ["b", "c"]);
        let record = &crate::image_records::load_image_deployments(&db)[0];
        assert_eq!(record.active().stages[1].replicas.len(), 2);
    }

    #[tokio::test]
    async fn a_machine_on_two_stages_gets_two_assignments() {
        let (_dir, db) = db();
        let plan = plan_image(&zimage(&["b"]), &network());
        let (call, calls) = targets(&[]);
        deploy_stages(&db, &plan, &call, "img-1", "me", &|_| {}).await.unwrap();
        let on_a: Vec<_> = applies(&calls).into_iter().filter(|(n, _)| n == "a").map(|(_, c)| c).collect();
        assert_eq!(on_a, ["text_encoder", "vae"]);
    }

    #[tokio::test]
    async fn a_failed_replica_is_reported_and_the_deploy_succeeds() {
        let (_dir, db) = db();
        let plan = plan_image(&zimage(&["b", "c"]), &network());
        let (call, calls) = targets(&["c"]);
        let failed = deploy_stages(&db, &plan, &call, "img-1", "me", &|_| {}).await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].node_id, "c");
        assert!(calls.lock().unwrap().iter().any(|(n, p, b)| n == "c" && *p == RELEASE_PATH && b["component"] == "diffusion"));
        let record = &crate::image_records::load_image_deployments(&db)[0];
        assert_eq!(record.active().stages[1].replicas.len(), 1);
    }

    #[tokio::test]
    async fn a_stage_left_empty_releases_everything() {
        let (_dir, db) = db();
        let plan = plan_image(&zimage(&["b"]), &network());
        let (call, calls) = targets(&["b"]);
        let error = deploy_stages(&db, &plan, &call, "img-1", "me", &|_| {}).await.unwrap_err();
        assert!(error.starts_with("diffusion: no machine could serve it"), "{error}");
        assert!(calls.lock().unwrap().iter().any(|(n, p, _)| n == "a" && *p == RELEASE_PATH));
        assert!(crate::image_records::load_image_deployments(&db).is_empty());
    }

    #[test]
    fn the_old_components_form_still_deploys() {
        let body: DeployBody = serde_json::from_value(serde_json::json!({
            "name": "z",
            "components": [{"component": "diffusion", "node_id": "b", "path": "/m/z.safetensors"},
                           {"component": "vae", "node_id": "a", "path": "/m/ae.safetensors"}]
        })).unwrap();
        let request = body.into_request();
        let plan = plan_image(&request, &network());
        assert!(plan.blocked.is_empty(), "{:?}", plan.blocked);
        assert_eq!(plan.stages[0].chosen[0].path.as_deref(), Some("/m/z.safetensors"));
    }

    #[test]
    fn a_plan_with_copies_and_no_permission_is_refused() {
        let plan = plan_image(&zimage(&["a"]), &network());
        let (status, body) = refusal(&plan, false).unwrap();
        assert_eq!(status, 409);
        assert_eq!(body["error"], "transfers_not_authorized");
        assert!(refusal(&plan_image(&zimage(&["b"]), &network()), false).is_none());
    }

    // ── add / remove / redeploy ──

    async fn deployed(db: &crate::db::PomDb, diffusion: &[&str]) -> String {
        let plan = plan_image(&zimage(diffusion), &network());
        let (call, _) = targets(&[]);
        deploy_stages(db, &plan, &call, "img-1", "me", &|_| {}).await.unwrap();
        "img-1".to_owned()
    }

    #[tokio::test]
    async fn adding_a_replica_appends_a_version() {
        let (_dir, db) = db();
        let id = deployed(&db, &["b"]).await;
        let (call, calls) = targets(&[]);
        let version = add_replica(&db, &network(), &call, &id, "diffusion", "c", "me").await.unwrap();
        assert_eq!(version, 2);
        assert_eq!(applies(&calls), [("c".to_owned(), "diffusion".to_owned())]);
        let record = &crate::image_records::load_image_deployments(&db)[0];
        assert_eq!(record.active().stages[1].replicas.len(), 2);
        let refused = add_replica(&db, &network(), &call, &id, "diffusion", "d", "me").await.unwrap_err();
        assert_eq!(refused.0, 400);
        assert_eq!(refused.1["error"], "does not run ComfyUI");
        let missing = add_replica(&db, &network(), &call, "img-other", "diffusion", "c", "me").await.unwrap_err();
        assert_eq!(missing.0, 404);
    }

    #[tokio::test]
    async fn removing_the_last_replica_asks_first() {
        let (_dir, db) = db();
        let id = deployed(&db, &["b"]).await;
        let (call, calls) = targets(&[]);
        let refused = remove_replica(&db, &call, &id, "diffusion", "b", false, "me").await.unwrap_err();
        assert_eq!(refused.0, 409);
        assert_eq!(refused.1["error"], "gap");
        assert!(calls.lock().unwrap().is_empty());
        let (version, serving) = remove_replica(&db, &call, &id, "diffusion", "b", true, "me").await.unwrap();
        assert_eq!((version, serving), (2, false));
    }

    #[tokio::test]
    async fn removing_one_stage_keeps_the_other_on_that_machine() {
        let (_dir, db) = db();
        let id = deployed(&db, &["b"]).await;
        let (call, calls) = targets(&[]);
        remove_replica(&db, &call, &id, "vae", "a", true, "me").await.unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "a");
        assert_eq!(calls[0].2["component"], "vae");
        let record = &crate::image_records::load_image_deployments(&db)[0];
        assert_eq!(record.active().stages[0].replicas[0].node_id, "a", "text encoder stays");
    }

    #[tokio::test]
    async fn redeploy_reactivates_an_old_version() {
        let (_dir, db) = db();
        let id = deployed(&db, &["b"]).await;
        let (call, _) = targets(&[]);
        add_replica(&db, &network(), &call, &id, "diffusion", "c", "me").await.unwrap();
        let (call, calls) = targets(&[]);
        assert_eq!(redeploy_version(&db, &call, &id, 1).await.unwrap(), 1);
        assert!(calls.lock().unwrap().iter().any(|(n, p, b)| n == "c" && *p == RELEASE_PATH && b["component"] == "diffusion"));
        let record = &crate::image_records::load_image_deployments(&db)[0];
        assert_eq!(record.active_version, 1);
        assert_eq!(record.versions.len(), 2, "no version appended");
        assert_eq!(redeploy_version(&db, &call, &id, 9).await.unwrap_err().0, 400);
    }
}
