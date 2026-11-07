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
}
