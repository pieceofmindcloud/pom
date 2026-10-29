use super::*;
use crate::profile::{NodeProfile, ProfileAction};
// ── Adding or removing a machine of a model already on the air ───────────────
//
// Both paths touch **one** machine. `deploy_endpoint` redeploys the
// whole set, and using that path here would stop the inference of whoever is already
// serving - exactly what adding a machine exists to avoid.

#[derive(Debug, Deserialize)]
pub struct AddNodeOptionsRequest {
    pub group: String,
    #[serde(default)]
    pub kind: DeployKind,
    pub target_node_id: String,
}

#[derive(Debug, Serialize)]
pub struct AddNodeOption {
    pub kind: DeployKind,
    pub start_layer: u32,
    pub end_layer: u32,
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct AddNodeOptions {
    pub node_id: String,
    pub options: Vec<AddNodeOption>,
    pub cache_types: serde_json::Value,
    pub defaults: AddNodeDefaults,
    pub vision: AddNodeVision,
}

/// The dialog's vision switch for the new machine.
#[derive(Debug, Serialize)]
pub struct AddNodeVision {
    /// The machine holds a projector compatible with the model.
    pub available: bool,
    /// The deployment runs with a projector now: the switch starts on.
    pub deployed: bool,
}

/// The projector the deployment runs with now. The record does not keep it, so it is read from the live
/// configuration of its machines (as the redeploy does) and named by the survey's sidecars. `None`: no machine
/// reads images; `Some(None)`: one does, with a file the survey cannot name.
async fn deployment_projector(
    state: &GatewayState,
    record: Option<&crate::deployment::Deployment>,
    sidecars: &HashMap<String, Vec<crate::catalog::Sidecar>>,
) -> Option<Option<crate::catalog::SidecarFile>> {
    let record = record?;
    let peers = state.all_peers().await;
    for node in &record.nodes {
        let Some(peer) = peers.iter().find(|peer| peer.node_id == node.node_id) else {
            continue;
        };
        let Ok(snapshot) = peer_node_snapshot(state, peer).await else {
            continue;
        };
        let Some(path) = snapshot
            .get("config")
            .and_then(|config| config.get("mmproj_path"))
            .and_then(|value| value.as_str())
            .filter(|path| !path.is_empty())
        else {
            continue;
        };
        let file = sidecars.get(&node.node_id).and_then(|held| {
            held.iter()
                .find(|sidecar| sidecar.kind == crate::catalog::SidecarKind::Mmproj && sidecar.path == path)
                .map(|sidecar| crate::catalog::SidecarFile {
                    file_name: sidecar.file_name.clone(),
                    size_bytes: sidecar.size_bytes,
                })
        });
        return Some(file);
    }
    None
}

/// The projectors compatible with the model that `node_id` holds, the deployment's own first.
fn machine_projectors(
    survey: &Survey,
    machines: &[Machine],
    node_id: &str,
    deployed: Option<&crate::catalog::SidecarFile>,
) -> Vec<crate::catalog::SidecarFile> {
    let Some(shape) = survey.model_shape.as_ref() else {
        return Vec::new();
    };
    let mut files: Vec<crate::catalog::SidecarFile> = compatible_options(&survey.sidecars, machines, shape)
        .into_iter()
        .filter(|option| {
            option.kind == crate::catalog::SidecarKind::Mmproj
                && option.holders.iter().any(|holder| holder.node_id == node_id)
        })
        .map(|option| crate::catalog::SidecarFile {
            file_name: option.file_name,
            size_bytes: option.size_bytes,
        })
        .collect();
    files.sort_by_key(|file| Some(file) != deployed);
    files
}

#[derive(Debug, Serialize)]
pub struct AddNodeDefaults {
    pub n_gpu_layers: u32,
    pub offload_kqv: bool,
}

/// The dialog may add either an actual slice or a whole model. Query both
/// catalog views so the options are always backed by the exact `HeldKind` the
/// add endpoint will validate. Tensor parallelism has its own catalog.
pub(crate) fn add_node_option_survey_kinds(kind: DeployKind) -> Vec<DeployKind> {
    match kind {
        DeployKind::TensorParallel => vec![DeployKind::TensorParallel],
        DeployKind::Shards | DeployKind::Whole => {
            vec![DeployKind::Shards, DeployKind::Whole]
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct AddNodeRequest {
    pub group: String,
    /// The deployment that gains the machine. Empty while the screen does not send it:
    /// then the node joins without a record, serving the range, and the deployment keeps
    /// being identified by the label as before.
    #[serde(default)]
    pub deploy_id: String,
    /// Which catalog the range is looked up in. A whole model already in
    /// service takes a second machine the same way a slice does — the added
    /// node becomes a parallel route over the same layers.
    #[serde(default)]
    pub kind: DeployKind,
    pub target_node_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    #[serde(default)]
    pub params: Option<RuntimeParams>,
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    #[serde(default)]
    pub offload_kqv: Option<bool>,
    /// The new machine's own parameters (context, sessions, GPU layers, KV placement). They launch this machine only;
    /// the deployment's version keeps its parameters.
    #[serde(default)]
    pub overrides: Option<crate::deployment::NodeOverrides>,
    /// Install values of the target, applied before the new process
    /// comes up. They are optional for old clients, which keep the config
    /// already persisted on the peer.
    #[serde(default)]
    pub cache_type_k: Option<crate::config::KvCacheType>,
    #[serde(default)]
    pub cache_type_v: Option<crate::config::KvCacheType>,
    /// The vision projector on the new machine, when it is the one that reads the prompt (`vision_reader`). `None`
    /// follows the deployment: on when its machines run with a projector.
    #[serde(default)]
    pub vision: Option<bool>,
}
#[derive(Debug, Deserialize)]
pub struct RemoveNodeRequest {
    pub group: String,
    pub target_node_id: String,
    /// The exact deployment avoids mixing two deployments of the same model.
    #[serde(default)]
    pub deploy_id: String,
}

const ADD_NODE_ADMIN_ONLY: &str = "only the administrator can add a machine to the deployment";
const UNDEPLOY_ADMIN_ONLY: &str = "only the administrator can undeploy the deployment";

pub(super) fn deployment_authority(
    profile: NodeProfile,
    refusal: &'static str,
) -> Result<(), String> {
    profile
        .allows(ProfileAction::ManageDeployment)
        .then_some(())
        .ok_or_else(|| refusal.to_string())
}

/// Finds the active deployment that matches the model, including when
/// the panel has not loaded the history screen yet and cannot send the id.
pub(super) async fn record_for_group(
    state: &GatewayState,
    group: &str,
) -> Option<crate::deployment::Deployment> {
    let ids = state
        .all_peers()
        .await
        .into_iter()
        .filter(|peer| peer.model == group && !peer.deploy_id.is_empty())
        .map(|peer| peer.deploy_id)
        .collect::<std::collections::BTreeSet<_>>();
    let records = futures::future::join_all(ids.iter().map(|id| find_record(state, id)))
        .await
        .into_iter()
        .flatten();
    newest_deployment(records)
}

async fn target_json(
    state: &GatewayState,
    target_node_id: &str,
    path: &str,
) -> Result<serde_json::Value, String> {
    if target_node_id == state.network.node_id {
        let runtime = state.runtime.config().await;
        return Ok(match path {
            "node" => json!({
                "config": runtime,
            }),
            "cache-types" => {
                let cfg = state.auth.read().await;
                json!({
                    "cache_type_k": cfg.cache_type_k,
                    "cache_type_v": cfg.cache_type_v,
                    "choices": crate::config::KvCacheType::ALL
                        .iter()
                        .map(|kind| kind.as_str())
                        .collect::<Vec<_>>(),
                })
            }
            _ => json!({}),
        });
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == target_node_id)
        .ok_or_else(|| "the chosen machine is not in the cluster".to_string())?;
    state
        .control_get(&peer, &format!("/api/ui/{path}"), PEER_TIMEOUT)
        .await
        .map_err(|e| format!("peer unreachable: {e:#}"))?
        .json()
        .await
        .map_err(|e| format!("invalid peer response: {e}"))
}

pub(super) async fn apply_target_cache_types(
    state: &GatewayState,
    target_node_id: &str,
    requested_k: Option<crate::config::KvCacheType>,
    requested_v: Option<crate::config::KvCacheType>,
) -> Result<(), String> {
    if requested_k.is_none() && requested_v.is_none() {
        return Ok(());
    }
    let current = target_json(state, target_node_id, "cache-types").await?;
    let current_k = current
        .get("cache_type_k")
        .cloned()
        .ok_or_else(|| "the machine did not report the K cache type".to_string())
        .and_then(|value| {
            serde_json::from_value(value).map_err(|e| format!("invalid K cache: {e}"))
        })?;
    let current_v = current
        .get("cache_type_v")
        .cloned()
        .ok_or_else(|| "the machine did not report the V cache type".to_string())
        .and_then(|value| {
            serde_json::from_value(value).map_err(|e| format!("invalid V cache: {e}"))
        })?;
    let cache_type_k = requested_k.unwrap_or(current_k);
    let cache_type_v = requested_v.unwrap_or(current_v);

    if target_node_id == state.network.node_id {
        let mut cfg = state.auth.write().await;
        cfg.cache_type_k = cache_type_k;
        cfg.cache_type_v = cache_type_v;
        cfg.save()
            .map_err(|e| format!("could not write pom.json: {e}"))?;
        return Ok(());
    }

    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == target_node_id)
        .ok_or_else(|| "the chosen machine is not in the cluster".to_string())?;
    let response = state
        .control_post_json(
            &peer,
            "/api/ui/cache-types",
            &json!({ "cache_type_k": cache_type_k, "cache_type_v": cache_type_v }),
            PEER_TIMEOUT,
        )
        .await
        .map_err(|e| format!("peer unreachable: {e:#}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "the machine refused the cache configuration: HTTP {}",
            response.status()
        ))
    }
}

pub(super) async fn add_node_options_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<AddNodeOptionsRequest>,
) -> Response {
    let mut held = Vec::new();
    let mut target_seen = false;
    for survey_kind in add_node_option_survey_kinds(req.kind) {
        let survey = survey(&state, &req.group, survey_kind, None, None).await;
        if let Some(target) = survey
            .machines
            .iter()
            .find(|machine| machine.node_id == req.target_node_id)
        {
            target_seen = true;
            held.extend(target.held.iter().map(|held| AddNodeOption {
                kind: match held.kind {
                    HeldKind::Whole => DeployKind::Whole,
                    HeldKind::Shard => DeployKind::Shards,
                },
                start_layer: held.start_layer,
                end_layer: held.end_layer,
                label: held.name.clone(),
            }));
        }
    }
    if !target_seen {
        return bad(json!({ "error": "the chosen machine is not in the cluster" }));
    }

    let mut options = Vec::new();
    for option in held {
        if !options.iter().any(|existing: &AddNodeOption| {
            existing.kind == option.kind
                && existing.start_layer == option.start_layer
                && existing.end_layer == option.end_layer
                && existing.label == option.label
        }) {
            options.push(option);
        }
    }
    options.sort_by_key(|option| {
        (
            option.start_layer,
            option.end_layer,
            matches!(option.kind, DeployKind::Shards),
        )
    });

    let node = target_json(&state, &req.target_node_id, "node").await;
    let defaults = node
        .as_ref()
        .ok()
        .and_then(|body| body.get("config"))
        .and_then(|config| config.as_object())
        .map(|config| AddNodeDefaults {
            n_gpu_layers: config
                .get("n_gpu_layers")
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or_else(|| RuntimeParams::default().n_gpu_layers),
            offload_kqv: config
                .get("offload_kqv")
                .and_then(|value| value.as_bool())
                .unwrap_or_else(|| RuntimeParams::default().offload_kqv),
        })
        .unwrap_or_else(|| AddNodeDefaults {
            n_gpu_layers: RuntimeParams::default().n_gpu_layers,
            offload_kqv: RuntimeParams::default().offload_kqv,
        });
    let cache_types = match target_json(&state, &req.target_node_id, "cache-types").await {
        Ok(value) => value,
        Err(error) => return bad(json!({ "error": error })),
    };
    let survey_now = survey(&state, &req.group, req.kind, None, None).await;
    let record = record_for_group(&state, &req.group).await;
    let deployed = deployment_projector(&state, record.as_ref(), &survey_now.sidecars).await;
    let vision = AddNodeVision {
        available: !machine_projectors(&survey_now, &survey_now.machines, &req.target_node_id, None).is_empty(),
        deployed: deployed.is_some(),
    };

    Json(AddNodeOptions {
        node_id: req.target_node_id,
        options,
        cache_types,
        defaults,
        vision,
    })
    .into_response()
}

/// Ranges this group has on the air now, one per peer serving it.
pub(super) async fn live_ranges(
    state: &GatewayState,
    group: &str,
    deploy_id: Option<&str>,
) -> Vec<(String, u32, u32)> {
    state
        .all_peers()
        .await
        .into_iter()
        .filter(|p| {
            p.model == group
                && p.total_layers > 0
                && deploy_id.is_none_or(|id| id.is_empty() || p.deploy_id == id)
        })
        .map(|p| (p.node_id, p.start_layer, p.end_layer))
        .collect()
}

/// Ranges that belong to the sliced head of a mixed deployment.
///
/// The record preserves the difference between the whole GGUF and a slice even
/// when both cover all the layers. Only the first one leaves this list: the
/// position of the new shard cannot count the whole model as a stage of the
/// sliced pipeline.
fn sliced_route_ranges(
    record: Option<&crate::deployment::Deployment>,
    live: &[(String, u32, u32)],
    total_layers: u32,
) -> Vec<(u32, u32)> {
    live.iter()
        .filter(|(node_id, start, end)| {
            !record.is_some_and(|deployment| {
                deployment.node(node_id).is_some_and(|node| {
                    deployment.kind == crate::deployment::DeploymentKind::Whole
                        || (deployment.kind == crate::deployment::DeploymentKind::Mixed
                            && *start == 0
                            && end.saturating_add(1) == total_layers
                            && node.part_sha256.is_none()
                            && node.peer_count == 1)
                })
            })
        })
        .map(|(_, start, end)| (*start, *end))
        .collect()
}

pub(super) async fn add_node_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<AddNodeRequest>,
) -> Response {
    let profile = state.auth.read().await.profile;
    if let Err(e) = deployment_authority(profile, ADD_NODE_ADMIN_ONLY) {
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(json!({ "error": e })),
        )
            .into_response();
    }

    if state.all_peers().await.iter().any(|peer| {
        peer.node_id == req.target_node_id && crate::gateway::peer_is_visitor(&state, peer)
    }) {
        return bad(json!({
            "reason": "visitor_target",
            "error": "this machine was admitted for inference only and cannot serve a model",
        }));
    }

    if req.start_layer > req.end_layer {
        return bad(json!({ "error": "inverted layer range" }));
    }

    if !req.deploy_id.is_empty() && !crate::deployment::is_deploy_id(&req.deploy_id) {
        return bad(json!({ "error": format!("invalid deploy_id: {}", req.deploy_id) }));
    }

    let existing_record = if req.deploy_id.is_empty() {
        record_for_group(&state, &req.group).await
    } else {
        find_record(&state, &req.deploy_id).await
    };
    let deployment_id = existing_record
        .as_ref()
        .map(|record| record.deploy_id.clone())
        .unwrap_or_else(|| req.deploy_id.clone());
    if !req.deploy_id.is_empty() && existing_record.is_none() {
        return bad(json!({
            "error": format!("unknown deployment {}", req.deploy_id)
        }));
    }
    let params = req
        .params
        .clone()
        .or_else(|| {
            existing_record
                .as_ref()
                .and_then(|record| record.active().map(|version| version.params.clone()))
        })
        .unwrap_or_default();
    let overrides = req.overrides.clone().unwrap_or_default();
    if let Err(message) = overrides.validate() {
        return bad(json!({ "error": message }));
    }
    // The new machine launches with its own parameters; the deployment's version keeps the base ones.
    let node_params = overrides.apply(&params);

    // The machine's engine: an inference service of a plugin (its own `inference_service`), or llama.cpp when none.
    // The same rules as a deployment (`deploy_block`): the machine offers it and it is ready; a service that does not
    // run layer slices takes the whole model on this machine.
    let mut survey_now = survey(&state, &req.group, req.kind, None, None).await;
    let architecture = survey_now
        .model_shape
        .as_ref()
        .and_then(|shape| shape.architecture.clone());
    if let Some(service) = node_params.inference_service.as_deref() {
        let whole = req.kind == DeployKind::Whole;
        if let Some(reason) = crate::plugin_engine::deploy_block(
            &state,
            service,
            whole,
            std::slice::from_ref(&req.target_node_id),
            &[node_params.inference_role],
            architecture.as_deref(),
        )
        .await
        {
            return bad(json!({ "error": reason }));
        }
        if !whole {
            let stages = crate::plugin_engine::services_of(&state, &req.target_node_id)
                .await
                .and_then(|services| {
                    services
                        .into_iter()
                        .find(|entry| entry["plugin_code"] == service && entry["kind"] == "inference")
                })
                .is_some_and(|entry| crate::plugin_engine::runs_slices(&entry, architecture.as_deref()));
            if !stages {
                return bad(json!({
                    "error": format!("the inference service {service} runs the whole model on one machine: add the whole model here"),
                }));
            }
        }
    }

    // Re-surveys instead of trusting the screen: the preview may be a minute old, and a
    // machine that left in the meantime would receive a slice.
    // The slice is looked up in the same surveys the options were built from (`add_node_option_survey_kinds`): a slice
    // the dialog offers is the one this checks, whichever survey found it.
    let requested_kind = match req.kind {
        DeployKind::Whole => HeldKind::Whole,
        _ => HeldKind::Shard,
    };
    let mut target_label = String::new();
    let mut held_shard = None;
    for survey_kind in add_node_option_survey_kinds(req.kind) {
        let machines = survey(&state, &req.group, survey_kind, None, None)
            .await
            .machines;
        if let Some(machine) = machines.iter().find(|m| m.node_id == req.target_node_id) {
            target_label = machine.label();
            if held_shard.is_none() {
                held_shard = machine
                    .holds_kind(req.start_layer, req.end_layer, Some(requested_kind))
                    .cloned();
            }
        }
    }
    if target_label.is_empty() {
        return bad(json!({ "error": "the chosen machine is not in the cluster" }));
    }
    let machines = std::mem::take(&mut survey_now.machines);
    let Some(shard) = held_shard else {
        return bad(json!({
            "error": format!(
                "{} does not have {}; send it there first",
                target_label,
                req.kind.unit(&req.group, req.start_layer, req.end_layer)
            )
        }));
    };
    let source_model_path = shard.path.clone();
    let shard_sha256 = shard.sha256.clone();

    let live = live_ranges(
        &state,
        &req.group,
        (!deployment_id.is_empty()).then_some(deployment_id.as_str()),
    )
    .await;
    let others: Vec<(u32, u32)> = live
        .iter()
        .filter(|(id, _, _)| id != &req.target_node_id)
        .map(|(_, i, f)| (*i, *f))
        .collect();
    let overlapping = overlapping_layers(&others, (req.start_layer, req.end_layer));

    let candidates = candidates(&machines);
    // The slices may not carry their model's layer count (it is read from each GGUF header, which the catalog reads
    // later). A deployment in service already knows it: that is the record's count.
    let known = existing_record
        .as_ref()
        .map(|record| record.model.total_layers)
        .filter(|total| *total > 0);
    let Some(total_layers) = model_size(&candidates).value().or(known) else {
        return bad(json!({
            "error": format!("the slices of {} do not state the total number of layers", req.group)
        }));
    };
    if req.end_layer >= total_layers {
        return bad(json!({
            "error": format!(
                "{} goes past the end of a model with {total_layers} layers",
                req.kind.unit(&req.group, req.start_layer, req.end_layer)
            )
        }));
    }

    if let Err(error) = apply_target_cache_types(
        &state,
        &req.target_node_id,
        req.cache_type_k,
        req.cache_type_v,
    )
    .await
    {
        return bad(json!({ "error": error }));
    }

    // Vision: only the machine that reads the prompt loads the projector, the one the deployment runs with when this
    // machine has it, otherwise a compatible one it holds.
    let deployed = deployment_projector(&state, existing_record.as_ref(), &survey_now.sidecars).await;
    let reads_prompt = vision_reader(
        req.start_layer,
        req.end_layer,
        total_layers,
        node_params.inference_service.is_some(),
    );
    let use_mmproj = req.vision.unwrap_or(deployed.is_some()) && reads_prompt;
    let mmproj_file = if use_mmproj {
        let projectors = machine_projectors(&survey_now, &machines, &req.target_node_id, deployed.flatten().as_ref());
        match projectors.into_iter().next() {
            Some(file) => Some(file),
            None => {
                return bad(json!({
                    "error": format!(
                        "{target_label} has no vision projector for {}: copy one there, or add the machine without vision",
                        req.group
                    ),
                }))
            }
        }
    } else {
        None
    };

    // `peer_index`/`peer_count` feed only two decisions: whether this node exports
    // the residual state and whether it skips the embeddings. Both ask "am I the
    // last?", and the answer comes from the layer order in the cluster - not from the
    // position in a list of targets that this path, existing to touch a single
    // machine, will never build.
    let pipeline_ranges = sliced_route_ranges(existing_record.as_ref(), &live, total_layers);
    let (peer_index, peer_count) = if req.kind == DeployKind::Whole {
        (0, 1)
    } else {
        split_position(&pipeline_ranges, (req.start_layer, req.end_layer))
    };
    let target_node_id = req.target_node_id.clone();
    let response = crate::webui::launch(
        ConnectionAuth,
        State(state.clone()),
        Json(LaunchRequest {
            deploy_id: deployment_id.clone(),
            base_name: req.group.clone(),
            model_label: Some(req.group.clone()),
            deployment_name: req.group.clone(),
            total_layers,
            use_mmproj,
            // A single target: `launch` stops the inference of the targets it receives, and
            // this list has exactly the machine that is joining.
            targets: vec![LaunchTarget {
                node_id: target_node_id.clone(),
                source_model_path,
                start_layer: req.start_layer,
                end_layer: req.end_layer,
                skip_split: true,
                part_sha256: shard_sha256.clone(),
                merge: None,
                n_gpu_layers: req.n_gpu_layers,
                offload_kqv: req.offload_kqv,
                inference_role: None,
                no_embeddings: None,
                // One target per request, but the position is the pipeline's: without
                // this the added node says it is the last of the split.
                peer_index: Some(peer_index),
                peer_count: Some(peer_count),
                tp: None,
                mmproj_path: None,
                mtp_draft_path: None,
                mtp_draft_file: None,
                mmproj_file: mmproj_file.clone(),
                use_mmproj,
            }],
            // Who is the pipeline's entry does not change because of this: only whoever holds
            // layer 0 receives the prompt, and `launch` only marks `primary` on the
            // target it receives.
            primary_node_id: if req.start_layer == 0 {
                Some(target_node_id.clone())
            } else {
                None
            },
            params: node_params.clone(),
        }),
    )
    .await;

    if !response.status().is_success() {
        let reason = refusal(response).await;
        return bad(json!({ "error": reason }));
    }

    let (version, kind) = if let Some(mut record) = existing_record {
        let incoming_kind = record_kind(req.kind);
        if record.kind != incoming_kind && record.kind != crate::deployment::DeploymentKind::Mixed {
            record.kind = crate::deployment::DeploymentKind::Mixed;
        }
        let mut nodes = record.nodes.clone();
        nodes.retain(|node| node.node_id != target_node_id);
        let target = LaunchTarget {
            node_id: target_node_id.clone(),
            source_model_path: shard.path.clone(),
            start_layer: req.start_layer,
            end_layer: req.end_layer,
            skip_split: true,
            part_sha256: shard_sha256.clone(),
            merge: None,
            n_gpu_layers: req.n_gpu_layers,
            offload_kqv: req.offload_kqv,
            inference_role: None,
            no_embeddings: None,
            peer_index: Some(peer_index),
            peer_count: Some(peer_count),
            tp: None,
            mmproj_path: None,
            mtp_draft_path: None,
            mtp_draft_file: None,
            mmproj_file,
            use_mmproj,
        };
        let machines = survey(&state, &req.group, req.kind, None, None)
            .await
            .machines;
        let hostname = machines
            .iter()
            .find(|machine| machine.node_id == target_node_id)
            .map(Machine::label)
            .unwrap_or_default();
        nodes.push(crate::deployment::DeploymentNode {
            node_id: target_node_id.clone(),
            hostname,
            start_layer: req.start_layer,
            end_layer: req.end_layer,
            peer_index,
            peer_count,
            primary: req.start_layer == 0,
            part_sha256: target.part_sha256.clone(),
            source_model_path: (!target.source_model_path.is_empty())
                .then(|| target.source_model_path.clone()),
            tp_rank: None,
            config: crate::webui::params_for(&node_params, &target),
        });
        let version = record.append_version(params, nodes, state.network.node_id.as_str());
        if !overrides.is_empty() {
            if let Err(e) = record.set_node_overrides(&target_node_id, overrides.clone()) {
                warn!("add node: the overrides of {target_node_id} were not recorded: {e}");
            }
        }
        publish_record(&state, &record).await;
        (Some(version), record.kind)
    } else {
        (None, record_kind(req.kind))
    };

    Json(json!({
        "applied": true,
        "overlapping_layers": overlapping,
        "peer_index": peer_index,
        "peer_count": peer_count,
        "deploy_id": (!deployment_id.is_empty()).then_some(deployment_id),
        "version": version,
        "kind": kind,
    }))
    .into_response()
}

pub(super) async fn remove_node_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<RemoveNodeRequest>,
) -> Response {
    if !req.deploy_id.is_empty() && !crate::deployment::is_deploy_id(&req.deploy_id) {
        return bad(json!({ "error": format!("invalid deploy_id: {}", req.deploy_id) }));
    }
    let record = if req.deploy_id.is_empty() {
        record_for_group(&state, &req.group).await
    } else {
        find_record(&state, &req.deploy_id).await
    };
    let deployment_id = record
        .as_ref()
        .map(|deployment| deployment.deploy_id.as_str())
        .or_else(|| (!req.deploy_id.is_empty()).then_some(req.deploy_id.as_str()));
    let live = live_ranges(&state, &req.group, deployment_id).await;
    let Some((_, start, end)) = live
        .iter()
        .find(|(id, _, _)| id == &req.target_node_id)
        .cloned()
    else {
        // The record can still list a machine that left the model (it was given another
        // one, or its slot failed): dropping it from the record is all there is to undo,
        // and nothing is unloaded, since what it serves now is another deployment's.
        if let Some(mut record) = record.filter(|record| record.node(&req.target_node_id).is_some())
        {
            let version = drop_from_record(&state, &mut record, &req.target_node_id).await;
            return Json(json!({
                "removed": true,
                "uncovered_layers": [],
                "deploy_id": record.deploy_id,
                "version": version,
                "serving": !live.is_empty(),
            }))
            .into_response();
        }
        return bad(json!({ "error": "this machine is not serving this model" }));
    };
    let remaining: Vec<(u32, u32)> = live
        .iter()
        .filter(|(id, _, _)| id != &req.target_node_id)
        .map(|(_, i, f)| (*i, *f))
        .collect();
    let discovered = uncovered_after_removal(&remaining, (start, end));

    // Whoever frees the memory is its owner, as in the "Liberar fatia" button: this
    // node only forwards the request.
    if req.target_node_id == state.network.node_id {
        state.runtime.stop().await;
    } else {
        let Some(peer) = state
            .all_peers()
            .await
            .into_iter()
            .find(|peer| peer.node_id == req.target_node_id)
        else {
            return bad(json!({ "error": "the chosen machine is not in the cluster" }));
        };
        match state
            .control_post_empty(&peer, "/api/ui/unload", crate::webui::PEER_STOP_TIMEOUT)
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                return bad(json!({ "error": format!("the peer refused: HTTP {}", r.status()) }));
            }
            Err(e) => return bad(json!({ "error": format!("peer unreachable: {e}") })),
        }
    }

    let (deploy_id, version, serving) = if let Some(mut record) = record {
        if record.node(&req.target_node_id).is_some() {
            let version = drop_from_record(&state, &mut record, &req.target_node_id).await;
            let serving = !record.nodes.is_empty();
            (Some(record.deploy_id), Some(version), serving)
        } else {
            (None, None, !live.is_empty())
        }
    } else {
        (None, None, !live.is_empty())
    };

    Json(json!({
        "removed": true,
        "uncovered_layers": discovered,
        "deploy_id": deploy_id,
        "version": version,
        "serving": serving,
    }))
    .into_response()
}

/// Appends the record's version without `node_id` and publishes it.
async fn drop_from_record(
    state: &GatewayState,
    record: &mut crate::deployment::Deployment,
    node_id: &str,
) -> u32 {
    let params = record
        .active()
        .map(|active| active.params.clone())
        .unwrap_or_default();
    let nodes = record
        .nodes
        .iter()
        .filter(|node| node.node_id != node_id)
        .cloned()
        .collect();
    let version = record.append_version(params, nodes, state.network.node_id.as_str());
    publish_record(state, record).await;
    version
}

// ── Taking the whole pipeline off the air ───────────────────────────────────
//
// The opposite of "Implantar na rede". Removing a machine from the model already existed
// (`/api/ui/deploy/node/remove`), but undoing the whole deployment meant
// clicking machine by machine, in the right order, hoping not to forget
// any - and a forgotten one is RAM and VRAM stuck in a slice that serves
// no purpose anymore, because the pipeline only plans with closed coverage.
//
// **What this does NOT do: touch the disk.** No slice is deleted. Undeploying
// is giving the memory back; the files stay where they are and the same deployment
// comes back with one click instead of 17 minutes of transfer per machine.
// Deleting slices is what the Models screen does, with its own confirmation
// request, and it is a different decision from this one.

/// The profile of the node that received the request decides whether the whole pipeline can come
/// off the air. The unload forwarded to the peers remains the local
/// operation of each machine and does not go through this gate.

#[derive(Debug, Deserialize)]
pub struct UndeployRequest {
    /// Model whose pipeline comes off the air. Empty means "the one that is on the air",
    /// which is the panel's case: it shows one pipeline at a time.
    #[serde(default)]
    pub group: String,
    /// The exact deployment that comes off the air.
    ///
    /// It is the way out of the dead end `resolve_group` hit: with two
    /// deployments on the air - including **of the same model**, which is now
    /// possible - the label does not distinguish, and the id does.
    #[serde(default)]
    pub deploy_id: String,
}

/// What happened to **one** machine. One row per machine, always - including
/// for the one that did not answer.
#[derive(Debug, Serialize)]
pub struct UndeployOutcome {
    pub node_id: String,
    pub hostname: String,
    pub start_layer: u32,
    pub end_layer: u32,
    /// `true` only when the machine confirmed. A silent peer is `false` with the
    /// reason in `error`; counting it as a success would be saying the memory was
    /// freed without anyone having said so.
    pub released: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Unloads the model from all the machines that serve `group`.
///
/// Answers 200 with the report even when some machine fails: half
/// undeployed is the real state of the cluster, and hiding it behind an error
/// would leave the operator not knowing on which machines memory is still stuck.
/// Only an invalid request (no pipeline on the air, or ambiguity about which) is 400.
pub(super) async fn undeploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<UndeployRequest>,
) -> Response {
    {
        let profile = state.auth.read().await.profile;
        if let Err(e) = deployment_authority(profile, UNDEPLOY_ADMIN_ONLY) {
            return (
                axum::http::StatusCode::FORBIDDEN,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    }

    // A deploy still copying or loading stops first, so it cannot bring a
    // machine up behind the undeploy's back.
    let aborted = abort_in_flight(&state, &req.deploy_id, &req.group).await;

    let peers = state.all_peers().await;
    let target = match resolve_target(&req.deploy_id, &req.group, &peers) {
        Ok(a) => a,
        Err(e) if !aborted.is_empty() => {
            return Json(json!({
                "group": req.group,
                "deploy_id": aborted.first(),
                "cancelled": true,
                "released": 0,
                "failed": 0,
                "results": [],
                "detail": e,
            }))
            .into_response();
        }
        Err(e) => return bad(json!({ "error": e })),
    };

    // Re-surveys instead of trusting the screen, like `deploy`: a machine that left
    // in the meantime must not become an invented failure row. A machine of
    // the exact deployment counts even before it announces layers: it may
    // still be loading the model the operator gave up on.
    let exact = matches!(target, UndeployTarget::Deployment { .. });
    let serving: Vec<PeerInfo> = peers
        .into_iter()
        .filter(|p| (p.total_layers > 0 || exact) && target.matches(p))
        .collect();
    if serving.is_empty() && !aborted.is_empty() {
        if let UndeployTarget::Deployment { deploy_id, .. } = &target {
            if let Err(e) = state.database.remove_deployment(deploy_id) {
                warn!("could not forget the deployment {deploy_id}: {e:#}");
            }
        }
        return Json(json!({
            "group": target.label(),
            "deploy_id": aborted.first(),
            "cancelled": true,
            "released": 0,
            "failed": 0,
            "results": [],
        }))
        .into_response();
    }
    if serving.is_empty() {
        return bad(json!({
            "error": format!("no machine is serving {}", target.describe())
        }));
    }

    let group = target.label();
    info!(
        "undeploying {} from {} machine(s)",
        target.describe(),
        serving.len()
    );

    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();

    // In parallel: with eleven peers and the deadline of one control call per peer, in series the
    // screen would sit almost a minute without an answer.
    let mut results: Vec<UndeployOutcome> =
        futures::future::join_all(serving.into_iter().map(|peer| {
            let state = state.clone();
            let key = key.clone();
            let self_id = self_id.clone();
            async move {
                let error = release_one(&state, &peer, &key, &self_id).await.err();
                UndeployOutcome {
                    node_id: peer.node_id.clone(),
                    hostname: if peer.hostname.is_empty() {
                        peer.node_id[..12.min(peer.node_id.len())].to_string()
                    } else {
                        peer.hostname.clone()
                    },
                    start_layer: peer.start_layer,
                    end_layer: peer.end_layer,
                    released: error.is_none(),
                    error,
                }
            }
        }))
        .await;
    results.sort_by_key(|r| r.start_layer);

    let released = results.iter().filter(|r| r.released).count();
    let failed = results.len() - released;
    if failed > 0 {
        warn!("{failed} machine(s) did not confirm the undeploy of {group}");
    }

    // The record is only forgotten afterwards: a record deleted with machines still
    // serving would leave stuck memory with nothing naming it. On this node it is enough to
    // delete the file; the other participants delete theirs when they are
    // unloaded, and one that did not answer keeps its own - which is right, since
    // it is still on the air.
    if let UndeployTarget::Deployment { deploy_id, .. } = &target {
        if let Err(e) = state.database.remove_deployment(deploy_id) {
            warn!("could not forget the deployment {deploy_id}: {e:#}");
        }
    }

    Json(json!({
        "group": group,
        "deploy_id": match &target {
            UndeployTarget::Deployment { deploy_id, .. } => Some(deploy_id.clone()),
            UndeployTarget::Label(_) => None,
        },
        "cancelled": !aborted.is_empty(),
        "released": released,
        "failed": failed,
        "results": results,
    }))
    .into_response()
}

/// Which pipeline the request wants to take down.
///
/// Empty and only one on the air is the obvious intent. Empty with two is an ambiguity that
/// this function refuses to resolve on its own: taking down the wrong one costs
/// a whole redeployment.
pub(super) fn resolve_group(requested: &str, peers: &[PeerInfo]) -> Result<String, String> {
    if !requested.is_empty() {
        return Ok(requested.to_string());
    }
    let mut models: Vec<String> = peers
        .iter()
        .filter(|p| p.total_layers > 0 && !p.model.is_empty())
        .map(|p| p.model.clone())
        .collect();
    models.sort();
    models.dedup();
    match models.len() {
        0 => Err("no pipeline up to undeploy".to_string()),
        1 => Ok(models.remove(0)),
        _ => Err(format!(
            "there is more than one pipeline up ({}); say which one to undeploy",
            models.join(", ")
        )),
    }
}

/// What an undeployment will take down: a deployment, or everything that serves a
/// label.
pub(super) enum UndeployTarget {
    /// Exact choice. It is what distinguishes two deployments of the same model.
    Deployment { deploy_id: String, label: String },
    /// Everyone who serves this label, which is the behavior as always and the
    /// only one possible with peers that do not announce a deployment.
    Label(String),
}

impl UndeployTarget {
    pub(super) fn matches(&self, p: &PeerInfo) -> bool {
        match self {
            UndeployTarget::Deployment { deploy_id, .. } => &p.deploy_id == deploy_id,
            UndeployTarget::Label(l) => &p.model == l,
        }
    }
    pub(super) fn label(&self) -> String {
        match self {
            UndeployTarget::Deployment { label, .. } => label.clone(),
            UndeployTarget::Label(l) => l.clone(),
        }
    }
    pub(super) fn describe(&self) -> String {
        match self {
            UndeployTarget::Deployment { deploy_id, label } if label.is_empty() => {
                format!("the deployment {deploy_id}")
            }
            UndeployTarget::Deployment { deploy_id, label } => {
                format!("{label} (deployment {deploy_id})")
            }
            UndeployTarget::Label(l) => l.clone(),
        }
    }
}

/// Which deployment the request wants to take down.
///
/// With `deploy_id`, there is nothing to deduce - it is that one, and the label serves only for the
/// message. Without it, it falls back to the old rule about labels, which remains
/// the right one in a cluster where each model is on the air only once.
pub(super) fn resolve_target(
    deploy_id: &str,
    group: &str,
    peers: &[PeerInfo],
) -> Result<UndeployTarget, String> {
    if !deploy_id.is_empty() {
        if !crate::deployment::is_deploy_id(deploy_id) {
            return Err(format!("invalid deploy_id: {deploy_id}"));
        }
        let label = peers
            .iter()
            .find(|p| p.deploy_id == deploy_id)
            .map(|p| p.model.clone())
            .unwrap_or_else(|| group.to_string());
        return Ok(UndeployTarget::Deployment {
            deploy_id: deploy_id.to_string(),
            label,
        });
    }
    resolve_group(group, peers).map(UndeployTarget::Label)
}

/// Frees the memory of **one** machine, through the same door as the button that already exists.
///
/// Whoever kills the inference process is always the owner of the memory: this node only
/// forwards, exactly like `remove_node_endpoint`.
pub(super) async fn release_one(
    state: &GatewayState,
    peer: &PeerInfo,
    _key: &str,
    self_id: &str,
) -> Result<(), String> {
    if peer.node_id == self_id {
        state.runtime.stop().await;
        return Ok(());
    }
    match state
        .control_post_empty(peer, "/api/ui/unload", crate::webui::PEER_STOP_TIMEOUT)
        .await
    {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(format!("the machine refused: HTTP {}", r.status())),
        Err(e) => Err(format!("machine unreachable: {e}")),
    }
}

pub(super) fn bad(body: serde_json::Value) -> Response {
    (axum::http::StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// The reason a refused launch gives, not just its status code.
///
/// `launch` answers `{"error": "..."}` and the operator only ever saw
/// "HTTP 400", which is the same dead end this whole screen exists to avoid.
pub(super) async fn refusal(response: Response) -> String {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        });
    match body {
        Some(reason) => format!("the launch refused: {reason}"),
        None => format!("the launch refused: HTTP {status}"),
    }
}

pub(super) async fn gateway_of(state: &GatewayState, node_id: &str) -> Option<String> {
    state
        .all_peers()
        .await
        .iter()
        .find(|p| p.node_id == node_id)
        .and_then(|p| state.gateway_base(p))
}

pub(super) async fn peer_of(
    state: &GatewayState,
    node_id: &str,
) -> Option<crate::network::PeerInfo> {
    state
        .all_peers()
        .await
        .into_iter()
        .find(|p| p.node_id == node_id)
}

/// Poll the machines doing the copying until they are all finished.
///
/// The bar tracks bytes rather than file count, because the slices are wildly
/// unequal — a set of 2 GB and 9,6 GB would otherwise sit at 50% for most of the
/// wait and then finish in a blink.
pub(super) async fn wait_for_transfers(
    state: &GatewayState,
    _key: &str,
    watching: &[(crate::network::PeerInfo, String)],
    job_id: &str,
) -> Result<(), String> {
    loop {
        let mut done = 0usize;
        let mut fraction_sum = 0.0;

        for (peer, id) in watching {
            let body: serde_json::Value = state
                .control_get(peer, "/api/transfer/jobs", PEER_TIMEOUT)
                .await
                .map_err(|e| format!("lost contact with {}: {e:#}", peer.hostname))?
                .json()
                .await
                .unwrap_or(json!({}));

            let job = body
                .get("jobs")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .find(|j| j.get("id").and_then(|v| v.as_str()) == Some(id.as_str()));

            let Some(job) = job else {
                // A job that vanished from the list finished and was pruned.
                done += 1;
                fraction_sum += 1.0;
                continue;
            };

            match job.get("state").and_then(|v| v.as_str()) {
                Some("done") => {
                    done += 1;
                    fraction_sum += 1.0;
                }
                Some("failed") => {
                    return Err(format!(
                        "the copy failed: {}",
                        job.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("no detail")
                    ));
                }
                _ => {
                    let total = job
                        .get("total_bytes")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let got = job
                        .get("received_bytes")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    if total > 0.0 {
                        fraction_sum += (got / total).clamp(0.0, 1.0);
                    }
                }
            }
        }

        // Phase 1 owns the first 90% of the bar; the launch is the rest.
        let pct = fraction_sum / watching.len().max(1) as f64 * 90.0;
        set_job_progress(job_id, pct).await;

        if done == watching.len() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
