use super::*;
impl RecordPlan {
    pub(super) fn deploy_id(&self) -> &str {
        match self {
            RecordPlan::New { deploy_id } => deploy_id,
            RecordPlan::ReuseVersion(d) => &d.deploy_id,
        }
    }
}

/// A deploy still being carried out, keyed by its `deploy_id`: what an
/// undeploy needs to stop it before any machine announces the model. The
/// operator may give up while the slices are still copying or loading.
pub(super) struct InFlightDeploy {
    pub(super) job_id: String,
    pub(super) group: String,
    pub(super) task: Option<tokio::task::AbortHandle>,
    /// Copies already asked of the targets: the target and its transfer job.
    pub(super) transfers: Vec<(PeerInfo, String)>,
}

pub(super) fn in_flight() -> &'static std::sync::Mutex<HashMap<String, InFlightDeploy>> {
    static IN_FLIGHT: std::sync::OnceLock<std::sync::Mutex<HashMap<String, InFlightDeploy>>> =
        std::sync::OnceLock::new();
    IN_FLIGHT.get_or_init(Default::default)
}

fn lock_in_flight() -> std::sync::MutexGuard<'static, HashMap<String, InFlightDeploy>> {
    in_flight().lock().unwrap_or_else(|error| error.into_inner())
}

/// Stops the deploys in flight that match: the task is aborted, the copies it
/// asked for are cancelled on their targets and its job says why. Returns the
/// `deploy_id`s it stopped. An empty `deploy_id` matches by `group`.
pub(super) async fn abort_in_flight(state: &GatewayState, deploy_id: &str, group: &str) -> Vec<String> {
    let stopped: Vec<(String, InFlightDeploy)> = {
        let mut all = lock_in_flight();
        let ids: Vec<String> = all
            .iter()
            .filter(|(id, entry)| {
                if deploy_id.is_empty() {
                    !group.is_empty() && entry.group == group
                } else {
                    id.as_str() == deploy_id
                }
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| all.remove(&id).map(|entry| (id, entry)))
            .collect()
    };
    for (id, entry) in &stopped {
        if let Some(task) = &entry.task {
            task.abort();
        }
        for (peer, transfer) in &entry.transfers {
            let path = format!("/api/transfer/jobs/{transfer}/cancel");
            if let Err(e) = state.control_post_empty(peer, &path, PEER_TIMEOUT).await {
                warn!("could not cancel the copy {transfer} on {}: {e:#}", peer.hostname);
            }
        }
        set_job(&entry.job_id, "failed", "cancelled", "undeployed by the operator before it finished").await;
        info!("deploy {id} stopped before it finished");
    }
    stopped.into_iter().map(|(id, _)| id).collect()
}

/// Forgets a deploy that ended on its own.
pub(super) fn finish_in_flight(deploy_id: &str) {
    lock_in_flight().remove(deploy_id);
}

pub(super) fn set_in_flight_task(deploy_id: &str, task: tokio::task::AbortHandle) {
    if let Some(entry) = lock_in_flight().get_mut(deploy_id) {
        entry.task = Some(task);
    }
}

/// Copy what has to be copied, then bind every peer to its range.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    state: GatewayState,
    plan: Plan,
    params: RuntimeParams,
    identity: RequestedModelIdentity,
    record_plan: RecordPlan,
    use_mmproj: bool,
    keep_existing: bool,
    job_id: &str,
) -> Result<(), String> {
    let deploy_id = record_plan.deploy_id().to_string();
    let deploy_id = deploy_id.as_str();
    let plan_nodes: Vec<String> = plan
        .steps
        .iter()
        .map(|step| step.target_node_id.clone())
        .chain(
            plan.tp_targets
                .iter()
                .map(|target| target.target_node_id.clone()),
        )
        .collect();
    prepare_existing_model(
        &state,
        &plan.group,
        &identity,
        keep_existing.then_some(plan_nodes.as_slice()),
        job_id,
    )
    .await?;
    let key = state.auth.read().await.connection_key.clone();

    // One unit of movement: a slice that has to reach a target. A merged step
    // moves one file per part; a plain step moves its one slice.
    struct Pull {
        target_node_id: String,
        target_host: String,
        source_node_id: String,
        source_path: String,
        /// Hash of the part, when the source has it content-addressed.
        source_sha256: Option<String>,
        source_needs_split: bool,
        name: String,
        start_layer: u32,
        end_layer: u32,
    }
    let mut pulls: Vec<Pull> = Vec::new();
    if !plan.tp_targets.is_empty() {
        for target in &plan.tp_targets {
            if target.ready {
                continue;
            }
            let (Some(source_node), Some(source_path)) =
                (&target.source_node_id, &target.source_path)
            else {
                return Err(format!(
                    "rank {}of{} has nowhere to come from",
                    target.rank + 1,
                    target.size
                ));
            };
            pulls.push(Pull {
                target_node_id: target.target_node_id.clone(),
                target_host: target.target_host.clone(),
                source_node_id: source_node.clone(),
                source_path: source_path.clone(),
                source_sha256: target.source_sha256.clone(),
                source_needs_split: false,
                name: target.name.clone(),
                start_layer: target.start_layer,
                end_layer: target.end_layer,
            });
        }
    } else {
        for step in &plan.steps {
            if step.merged {
                for part in &step.parts {
                    if part.ready {
                        continue;
                    }
                    let (Some(source_node), Some(source_path)) =
                        (&part.source_node_id, &part.source_path)
                    else {
                        return Err(format!(
                            "slice L{}-{} has nowhere to come from",
                            part.start_layer, part.end_layer
                        ));
                    };
                    pulls.push(Pull {
                        target_node_id: step.target_node_id.clone(),
                        target_host: step.target_host.clone(),
                        source_node_id: source_node.clone(),
                        source_path: source_path.clone(),
                        source_sha256: part.source_sha256.clone(),
                        source_needs_split: part.source_needs_split,
                        name: part.name.clone(),
                        start_layer: part.start_layer,
                        end_layer: part.end_layer,
                    });
                }
            } else if !step.ready {
                let (Some(source_node), Some(source_path)) =
                    (&step.source_node_id, &step.source_path)
                else {
                    return Err(format!(
                        "slice L{}-{} has nowhere to come from",
                        step.start_layer, step.end_layer
                    ));
                };
                pulls.push(Pull {
                    target_node_id: step.target_node_id.clone(),
                    target_host: step.target_host.clone(),
                    source_node_id: source_node.clone(),
                    source_path: source_path.clone(),
                    source_sha256: step.source_sha256.clone(),
                    source_needs_split: step.source_needs_split,
                    name: step.name.clone(),
                    start_layer: step.start_layer,
                    end_layer: step.end_layer,
                });
            }
        }
    }

    // Virtual ranges are backed by the proposer's whole GGUF. Materialize all
    // of them on that peer before any destination asks the transfer service to
    // pull a path. Grouping keeps one source read/manifest operation per
    // source model even when the selected pipeline has several ranges.
    // Projector for the head and MTP draft for the tail, when the machine that
    // loads them does not have them yet. They go by path, like any file outside
    // a manifest, and land in the target's `recebidos/` folder, which its
    // catalog scans: the post-copy survey finds them there by name and size.
    for slot in [&plan.sidecars.vision, &plan.sidecars.mtp]
        .into_iter()
        .flatten()
        .chain(&plan.sidecars.vision_heads)
    {
        if !slot.needs_copy() {
            continue;
        }
        let (Some(file), Some(source_node), Some(source_path)) =
            (&slot.file, &slot.source_node_id, &slot.source_path)
        else {
            continue;
        };
        pulls.push(Pull {
            target_node_id: slot.target_node_id.clone(),
            target_host: slot.target_host.clone(),
            source_node_id: source_node.clone(),
            source_path: source_path.clone(),
            source_sha256: None,
            source_needs_split: false,
            name: file.file_name.clone(),
            start_layer: 0,
            end_layer: 0,
        });
    }

    let mut materializations: HashMap<(String, String, u32, String), Vec<SliceRange>> =
        HashMap::new();
    for pull in &pulls {
        if pull.source_needs_split {
            let ranges = materializations
                .entry((
                    pull.source_node_id.clone(),
                    pull.source_path.clone(),
                    plan.total_layers.ok_or_else(|| {
                        format!(
                            "the slices of {} do not state the total number of layers",
                            plan.group
                        )
                    })?,
                    plan.group.clone(),
                ))
                .or_default();
            if !ranges.iter().any(|range| {
                range.start_layer == pull.start_layer && range.end_layer == pull.end_layer
            }) {
                ranges.push(SliceRange {
                    start_layer: pull.start_layer,
                    end_layer: pull.end_layer,
                });
            }
        }
    }
    for ((source_node_id, source_path, total_layers, name), ranges) in materializations {
        set_job(
            job_id,
            "running",
            "slicing the whole model",
            &format!(
                "materializing {} range(s) on the machine that proposed the model",
                ranges.len()
            ),
        )
        .await;
        let response = materialize_slices_on_peer(
            &state,
            &source_node_id,
            &SliceRangesRequest {
                model_path: source_path.clone(),
                name: name.clone(),
                total_layers,
                ranges,
            },
        )
        .await?;
        for materialized in response.slices {
            let Some(pull) = pulls.iter_mut().find(|pull| {
                pull.source_needs_split
                    && pull.source_node_id == source_node_id
                    && pull.source_path == source_path
                    && pull.start_layer == materialized.start_layer
                    && pull.end_layer == materialized.end_layer
            }) else {
                return Err(format!(
                    "the slicer returned a range L{}-{} that was not in the plan",
                    materialized.start_layer, materialized.end_layer
                ));
            };
            pull.source_path = materialized.path;
            pull.source_sha256 = Some(materialized.sha256);
            pull.source_needs_split = false;
        }
    }
    if pulls.iter().any(|pull| pull.source_needs_split) {
        return Err("the local slicing did not return all the requested ranges".to_string());
    }

    // ── Phase 1: transfers ───────────────────────────────────────────────────
    //
    // Each target pulls for itself, through the transfer path that already has
    // resume, MD5 checking and a disk-space guard. Nothing new moves bytes here.
    if !pulls.is_empty() {
        set_job(
            job_id,
            "running",
            "copying slices",
            &format!(
                "{} slice(s), {:.1} GB in total",
                pulls.len(),
                plan.transfer_bytes as f64 / 1_073_741_824.0
            ),
        )
        .await;

        // (target peer, job id). The whole peer and not a URL: the request and
        // the polling go by every path the peer announces and, last, by the
        // p2p tunnel - a target behind NAT has no address that answers.
        let mut watching: Vec<(crate::network::PeerInfo, String)> = Vec::new();
        for pull in &pulls {
            if pull.target_node_id == pull.source_node_id {
                continue;
            }
            let target = peer_of(&state, &pull.target_node_id)
                .await
                .ok_or_else(|| format!("{} left the network", pull.target_host))?;
            let pull_body = json!({
                "node_id": pull.source_node_id,
                "sha256": pull.source_sha256,
                "model": plan.group,
                "remote_path": pull.source_path,
                "name": pull.name,
            });
            // By hash whenever the source knows its own. That is what makes the
            // slice land in `<shard_dir>/<model>/<sha>.gguf` with the `.pom`
            // beside it, instead of loose at the root looking like a whole
            // model. The request by path is left for the slice without a hash -
            // a file nobody has addressed by content yet.
            let body: serde_json::Value = state
                .control_post_json(&target, "/api/transfer/pull", &pull_body, PEER_TIMEOUT * 4)
                .await
                .map_err(|e| {
                    format!(
                        "{} did not accept pulling the slice: {e:#}",
                        pull.target_host
                    )
                })?
                .json()
                .await
                .unwrap_or(json!({}));

            match body.get("job_id").and_then(|v| v.as_str()) {
                Some(tid) => {
                    info!(
                        "{} pulling L{}-{} from {}",
                        pull.target_host, pull.start_layer, pull.end_layer, pull.source_node_id
                    );
                    if let Some(entry) = lock_in_flight().get_mut(deploy_id) {
                        entry.transfers.push((target.clone(), tid.to_string()));
                    }
                    watching.push((target, tid.to_string()));
                }
                None => {
                    return Err(format!(
                        "{} refused the copy: {}",
                        pull.target_host,
                        body.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("no reason given")
                    ));
                }
            }
        }

        wait_for_transfers(&state, &key, &watching, job_id).await?;
    }

    // ── Phase 2: bind every peer to its range ────────────────────────────────
    set_job(
        job_id,
        "running",
        "starting the nodes",
        "assigning the ranges",
    )
    .await;
    set_job_progress(job_id, 92.0).await;

    // Where each file ended up is the target's own business, so ask it rather
    // than assume a path on someone else's filesystem.
    let launch_selection = (plan.kind == DeployKind::Whole).then(|| {
        plan.steps
            .iter()
            .map(|step| SelectedSlice {
                start_layer: step.start_layer,
                end_layer: step.end_layer,
                target_node_id: step.target_node_id.clone(),
                n_gpu_layers: None,
                offload_kqv: None,
                inference_role: step.inference_role,
            })
            .collect::<Vec<_>>()
    });
    let fresh = survey(
        &state,
        &plan.group,
        plan.kind,
        launch_selection.as_deref(),
        plan.tp.as_ref(),
    )
    .await;
    let machines = fresh.machines;
    let mut targets = launch_targets(
        &plan.steps,
        &machines,
        plan.kind,
        plan.tp.as_ref(),
        &plan.tp_targets,
    )?;
    if use_mmproj {
        let head = targets
            .iter_mut()
            .find(|target| target.start_layer == 0)
            .ok_or_else(|| {
                "a deployment with vision needs a target that starts at layer 0".to_string()
            })?;
        head.use_mmproj = true;
        head.mmproj_path = plan.mmproj_path.clone();
        if let Some(slot) = plan.sidecars.vision.as_ref() {
            if let Some(file) = slot
                .file
                .as_ref()
                .filter(|_| slot.mode == crate::catalog::SidecarMode::File)
            {
                // Known path when the survey already sees it; otherwise the
                // head finds the copy on its own disk by name and size.
                head.mmproj_path = local_sidecar_path(
                    &fresh.sidecars,
                    &slot.target_node_id,
                    crate::catalog::SidecarKind::Mmproj,
                    file,
                );
                head.mmproj_file = Some(file.clone());
            }
        }
        // a whole model on several machines: each other head reads images with its own copy of the projector
        for slot in &plan.sidecars.vision_heads {
            let Some(file) = slot
                .file
                .as_ref()
                .filter(|_| slot.mode == crate::catalog::SidecarMode::File)
            else {
                continue;
            };
            if let Some(target) = targets
                .iter_mut()
                .find(|target| target.start_layer == 0 && target.node_id == slot.target_node_id)
            {
                target.use_mmproj = true;
                target.mmproj_path = local_sidecar_path(
                    &fresh.sidecars,
                    &slot.target_node_id,
                    crate::catalog::SidecarKind::Mmproj,
                    file,
                );
                target.mmproj_file = Some(file.clone());
            }
        }
    }
    if let Some(slot) = plan.sidecars.mtp.as_ref() {
        if let Some(file) = slot
            .file
            .as_ref()
            .filter(|_| slot.mode == crate::catalog::SidecarMode::File)
        {
            let path = local_sidecar_path(
                &fresh.sidecars,
                &slot.target_node_id,
                crate::catalog::SidecarKind::Mtp,
                file,
            );
            let tail_end = plan.total_layers.map(|total| total.saturating_sub(1));
            for target in targets.iter_mut().filter(|target| {
                target.node_id == slot.target_node_id && Some(target.end_layer) == tail_end
            }) {
                target.mtp_draft_path = path.clone();
                target.mtp_draft_file = Some(file.clone());
            }
        }
    }
    if params.no_embeddings {
        // The planner already validated the topology atomically. Marking each target
        // explicitly keeps a global switch from being reinterpreted
        // differently by a peer.
        for target in &mut targets {
            target.no_embeddings = Some(true);
        }
    }

    // The pipeline is the layer order, so the entry point is whoever holds
    // layer 0 — the same machine the screen marks "processa o prompt".
    let primary = plan
        .steps
        .iter()
        .find(|s| s.primary)
        .map(|s| s.target_node_id.clone());

    // `plan` refuses an unknown size before anything is copied, so this is a
    // fact by the time we get here rather than a default worth inventing.
    let total_layers = plan.total_layers.ok_or_else(|| {
        format!(
            "the slices of {} do not state the total number of layers",
            plan.group
        )
    })?;

    // The deployment record is born here, with the targets already resolved: it is the
    // last point where it is known, all at once, who received which range and with
    // what configuration. After `launch` that intent dissolved into N
    // scattered `NodeConfig`s, and nobody in the cluster could say anymore that
    // those nodes were *the same* deployment.
    //
    // `New` builds v1; `ReuseVersion` appends (or reactivates) a version under the
    // `deploy_id` that already exists, so redeploying the same shape does not mint a new id
    // every time.
    let record = match record_plan {
        RecordPlan::New { .. } => build_record(
            deploy_id,
            &plan,
            &targets,
            &machines,
            total_layers,
            &params,
            &identity,
            &state.network.node_id,
        ),
        RecordPlan::ReuseVersion(mut existing) => {
            existing.ensure_versions();
            let nodes = build_version_nodes(&plan, &targets, &machines, &params);
            existing.upsert_version(params.clone(), nodes, state.network.node_id.as_str());
            *existing
        }
    };

    let response = launch(
        ConnectionAuth,
        State(state.clone()),
        Json(LaunchRequest {
            deploy_id: deploy_id.to_string(),
            base_name: plan.group.clone(),
            // `group` is the selected value from the deployment model picker.
            // Its name is the cluster identity, not a heuristic profile name.
            model_label: Some(plan.group.clone()),
            deployment_name: plan.deployment_name.clone(),
            total_layers,
            targets,
            primary_node_id: primary,
            params,
            use_mmproj,
        }),
    )
    .await;

    if !response.status().is_success() {
        return Err(refusal(response).await);
    }

    // Only after the launch succeeds: a deployment record that did not come up
    // would be a phantom deployment in the listing, and the undeploy screen
    // would offer something that is not on the air.
    publish_record(&state, &record).await;
    Ok(())
}

/// Builds the record from what the plan and the targets already say.
///
/// Nothing here is recomputed: the ranges, the position in the pipeline and the per-machine
/// adjustments are exactly the ones that went to `launch`. Storing the **result**
/// is what makes "with what configuration did that machine come up?" a read
/// instead of a reconstruction.
/// The nodes of a version: who received which range and with what effective
/// configuration. The same for the v1 of a new deployment and for a version appended to
/// a reused `deploy_id`.
pub(super) fn build_version_nodes(
    plan: &Plan,
    targets: &[LaunchTarget],
    machines: &[Machine],
    params: &RuntimeParams,
) -> Vec<crate::deployment::DeploymentNode> {
    let total_targets = targets.len() as u32;
    targets
        .iter()
        .enumerate()
        .map(|(i, t)| crate::deployment::DeploymentNode {
            node_id: t.node_id.clone(),
            hostname: machines
                .iter()
                .find(|m| m.node_id == t.node_id)
                .map(|m| m.label())
                .unwrap_or_default(),
            start_layer: t.start_layer,
            end_layer: t.end_layer,
            peer_index: t.peer_index.unwrap_or(i as u32),
            peer_count: t.peer_count.unwrap_or(total_targets),
            primary: plan
                .steps
                .iter()
                .any(|s| s.primary && s.target_node_id == t.node_id),
            part_sha256: t.part_sha256.clone(),
            source_model_path: (!t.source_model_path.is_empty())
                .then(|| t.source_model_path.clone()),
            tp_rank: t.tp.as_ref().map(|tp| tp.rank),
            // The same function that `launch` uses to decide what each machine
            // receives, so the record cannot diverge from what came up.
            config: crate::webui::params_for(params, t),
        })
        .collect()
}

/// The record's kind from the plan's kind.
pub(super) fn record_kind(kind: DeployKind) -> crate::deployment::DeploymentKind {
    match kind {
        DeployKind::Shards => crate::deployment::DeploymentKind::Shards,
        DeployKind::Whole => crate::deployment::DeploymentKind::Whole,
        DeployKind::TensorParallel => crate::deployment::DeploymentKind::TensorParallel,
    }
}

/// Builds the record of a **new** deployment (v1), from what the plan and
/// the targets already say.
///
/// Nothing here is recomputed: the ranges, the position in the pipeline and the per-machine
/// adjustments are exactly the ones that went to `launch`. Storing the **result**
/// is what makes "with what configuration did that machine come up?" a read
/// instead of a reconstruction.
pub(super) fn build_record(
    deploy_id: &str,
    plan: &Plan,
    targets: &[LaunchTarget],
    machines: &[Machine],
    total_layers: u32,
    params: &RuntimeParams,
    identity: &RequestedModelIdentity,
    created_by: &str,
) -> crate::deployment::Deployment {
    let nodes = build_version_nodes(plan, targets, machines, params);

    crate::deployment::Deployment::new(
        deploy_id.to_string(),
        record_kind(plan.kind),
        crate::deployment::DeploymentModel {
            label: plan.group.clone(),
            total_layers,
            pom_id: match identity.proof.as_ref() {
                Some(crate::deployment::DeploymentIdentity::PomId { pom_id }) => {
                    Some(pom_id.clone())
                }
                _ => None,
            },
            identity: identity.proof.clone(),
        },
        created_by,
        nodes,
    )
    .with_params(params.clone())
}

/// Writes the record on this node and sends a copy to each participant.
///
/// Each node keeps its own copy: there is no single owner that, on going down, takes
/// the deployment with it, and a node that restarts reads from disk which deployment it
/// was part of. A copy that does not arrive does not bring the deployment down - it is already
/// on the air; what is lost is that node being able to explain itself after a restart.
pub(super) async fn publish_record(state: &GatewayState, record: &crate::deployment::Deployment) {
    if let Err(e) = state.database.save_deployment(record) {
        warn!(
            "could not record the deployment {}: {e:#}",
            record.deploy_id
        );
    }

    let self_id = state.network.node_id.clone();
    let peers = state.all_peers().await;

    // Topology updates must also reach whoever left. The peer
    // only accepts the copy if it already knew the deployment, so the fan-out does not create
    // records on machines that never took part.
    for peer in peers.iter().filter(|peer| peer.node_id != self_id) {
        let send_result = state
            .control_post_json(peer, "/api/ui/deployments", record, PEER_TIMEOUT)
            .await;
        if let Err(e) = send_result {
            warn!(
                "{} did not accept the deployment record: {e:#}",
                peer.node_id
            );
        }
    }
}

// ── Touching a deployment that is already on the air ─────────────────────────

/// Finds the record of a deployment, here or on whoever has it.
///
/// Not every node keeps every record - only the participants do -, so the
/// coordinator of a `redeploy` may well not have the copy.
pub(super) async fn find_record(
    state: &GatewayState,
    deploy_id: &str,
) -> Option<crate::deployment::Deployment> {
    let local = state.database.deployment(deploy_id).ok().flatten();

    let self_id = state.network.node_id.clone();
    let searches = state.all_peers().await.into_iter().filter_map(|peer| {
        if peer.node_id == self_id {
            return None;
        }
        let peer_state = state.clone();
        let target = deploy_id.to_string();
        Some(async move {
            let body: serde_json::Value = peer_state
                .control_get(&peer, "/api/ui/deployments?local=true", PEER_TIMEOUT)
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            let records: Vec<crate::deployment::Deployment> =
                serde_json::from_value(body.get("deployments")?.clone()).ok()?;
            records
                .into_iter()
                .find(|d| d.deploy_id == target)
                .map(|mut d| {
                    d.ensure_versions();
                    d
                })
        })
    });

    let remote = futures::future::join_all(searches)
        .await
        .into_iter()
        .flatten();
    newest_deployment(local.into_iter().chain(remote))
}

/// Makes this the deployment that serves whoever asks only by model name.
///
/// Touches `activated_at` and re-spreads. Since the choice of the default is **derived**
/// (the most recently activated one), there is no election or quorum: it is enough for the new
/// stamp to arrive, and whoever receives it converges on its own. See `deployment::active`.
pub(super) async fn activate_deployment(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Path(id): Path<String>,
) -> Response {
    if !crate::deployment::is_deploy_id(&id) {
        return bad(json!({ "error": format!("invalid deploy_id: {id}") }));
    }
    let Some(mut record) = find_record(&state, &id).await else {
        return bad(json!({ "error": format!("unknown deployment {id}") }));
    };

    record.activated_at = chrono::Utc::now().to_rfc3339();
    publish_record(&state, &record).await;

    Json(json!({
        "activated": true,
        "deploy_id": record.deploy_id,
        "model": record.model.label,
    }))
    .into_response()
}

/// Per-machine adjustment, for the two knobs that are truly per machine.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PerNodeParams {
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    #[serde(default)]
    pub offload_kqv: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct RedeployRequest {
    /// New parameters of the whole deployment. Absent requires `version`.
    /// Present and equal to those of an existing version -> that version becomes
    /// the active one again; different -> new version appended to the same `deploy_id`.
    #[serde(default)]
    pub params: Option<RuntimeParams>,
    /// Redeploys an already registered version, without appending another. Mutually
    /// exclusive with `params` in practice; if both come, `version` wins.
    #[serde(default)]
    pub version: Option<u32>,
    /// Per-machine adjustments, by `node_id`. Absent = everyone with the same ones.
    #[serde(default)]
    pub per_node: HashMap<String, PerNodeParams>,
}

impl RedeployRequest {
    /// Resolves the request against the record: picks/creates the version, mutates the
    /// record to reflect it and returns the base parameters of each machine.
    pub(super) fn resolve(
        &self,
        record: &mut crate::deployment::Deployment,
        author: &str,
    ) -> Result<RuntimeParams, String> {
        record.ensure_versions();
        if let Some(v) = self.version {
            record.set_active_version(v).map_err(|e| e.to_string())?;
            let active_version = record.active_version;
            let (params, nodes) = {
                let version = record
                    .versions
                    .iter_mut()
                    .find(|ver| ver.version == active_version)
                    .expect("set_active_version guaranteed the active version");
                (version.params.clone(), version.nodes.clone())
            };
            record.nodes = nodes;
            return Ok(params);
        }
        let Some(params) = self.params.clone() else {
            return Err("provide `version` or `params`".to_string());
        };
        let mut nodes = record.nodes.clone();
        for n in &mut nodes {
            let mut cfg = params.clone();
            if let Some(adj) = self.per_node.get(&n.node_id) {
                if let Some(g) = adj.n_gpu_layers {
                    cfg.n_gpu_layers = g;
                }
                if let Some(k) = adj.offload_kqv {
                    cfg.offload_kqv = k;
                }
            }
            n.config = cfg;
        }
        record.upsert_version(params.clone(), nodes, author);
        Ok(params)
    }
}

/// What happened to **one** machine in a reconfiguration.
#[derive(Debug, Serialize)]
pub struct RedeployOutcome {
    pub node_id: String,
    pub hostname: String,
    pub reconfigured: bool,
    pub error: Option<String>,
}

/// Redeploys a `deploy_id` that already exists: `version` reactivates a registered
/// version, `params` redeploys by appending - or reactivating - a version.
///
/// The same deployment, the same `deploy_id`, the same machines and the same
/// ranges. That is why it does **not** re-plan: re-planning would mint a new
/// deployment, breaking every client tied to `model@deploy_id`.
///
/// Unloads every machine that still announces this `deploy_id` and brings each one up
/// again, one at a time, waiting for each to serve again before the
/// next. Works even if a machine was not serving anything (the old bug):
/// the slice's identity comes from the record plus the snapshot of what
/// each node was running, not from a live `NodeConfig`. Tensor Parallelism still uses
/// the old path (`reconfigure` in place). The record is rewritten
/// **after** the nodes accept.
pub(super) async fn redeploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Path(id): Path<String>,
    Json(req): Json<RedeployRequest>,
) -> Response {
    if !crate::deployment::is_deploy_id(&id) {
        return bad(json!({ "error": format!("invalid deploy_id: {id}") }));
    }
    let Some(mut record) = find_record(&state, &id).await else {
        return bad(json!({ "error": format!("unknown deployment {id}") }));
    };
    record.ensure_versions();
    if req.version.is_none() && req.params.is_none() {
        return bad(json!({ "error": "provide `version` or `params`" }));
    }
    if let Some(v) = req.version {
        if !record.versions.iter().any(|ver| ver.version == v) {
            let available: Vec<u32> = record.versions.iter().map(|ver| ver.version).collect();
            return bad(json!({
                "error": format!("version {v} does not exist (available: {available:?})")
            }));
        }
    }
    info!(
        "redeploy requested for {} ({}), {} node(s), version={:?}",
        id,
        record.model.label,
        record.nodes.len(),
        req.version
    );

    let job_id = uuid::Uuid::new_v4().to_string();
    let title = format!(
        "Reconfigure {} on {} {}",
        record.model.label,
        record.nodes.len(),
        if record.nodes.len() == 1 {
            "node"
        } else {
            "nodes"
        }
    );
    jobs().write().await.insert(
        job_id.clone(),
        new_job(&job_id, "redeploy", title, "reloading"),
    );
    state.plugins.publish_event(
        "deployment.started",
        json!({
            "kind": "redeploy",
            "deploy_id": id,
            "job_id": job_id,
            "model": record.model.label,
            "nodes": record.nodes.len(),
            "version": req.version,
        }),
    );

    let id_job = job_id.clone();
    tokio::spawn(async move {
        match run_redeploy(state, record, req, &id_job).await {
            Ok(failures) if failures == 0 => {
                set_job(&id_job, "done", "pronto", "the machines reloaded").await
            }
            Ok(failures) => {
                set_job(
                    &id_job,
                    "failed",
                    "partial",
                    &format!("{failures} machine(s) did not reload"),
                )
                .await
            }
            Err(e) => {
                warn!("reconfiguration failed: {e}");
                set_job(&id_job, "failed", "failed", &e).await
            }
        }
    });

    Json(json!({ "job_id": job_id, "deploy_id": id })).into_response()
}

pub(super) async fn run_redeploy(
    state: GatewayState,
    mut record: crate::deployment::Deployment,
    req: RedeployRequest,
    job_id: &str,
) -> Result<usize, String> {
    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();
    let peers = state.all_peers().await;
    let total = record.nodes.len().max(1);

    info!(
        "redeploy {} ({}): {} node(s), kind={:?}",
        record.deploy_id,
        record.model.label,
        record.nodes.len(),
        record.kind
    );

    // Picks/creates the requested version and mutates `record` to reflect it. `base` is
    // the `RuntimeParams` of the whole deployment of that version; the per-machine
    // adjustments of `req.per_node` go on top at reconfigure time.
    let base = req.resolve(&mut record, &self_id)?;

    // Re-deploy does not build a new plan, but changing the wire precision is
    // still a cluster-wide protocol change. Probe every participant before
    // touching the first process, otherwise a later incompatible peer could
    // leave one part of the same deployment on f8 and another on f16.
    validate_redeploy_wire_dtype(
        &state,
        &peers,
        &record.nodes,
        &self_id,
        base.hidden_state_dtype,
    )
    .await?;

    // Tensor parallelism: the collective does not survive an unload and the record
    // does not keep the whole `TpLaunch`, only the `tp_rank` of each node. Keeps the
    // old path (reconfigure in place), which requires the rank to be already serving.
    if record.kind == crate::deployment::DeploymentKind::TensorParallel {
        return run_redeploy_in_place(&state, &peers, &mut record, &req, &self_id, &key, job_id)
            .await;
    }

    // ── 1. Snapshot each node's live identity before taking anything down ─────
    //
    // The record has range, position and `part_sha256`, but not the file path
    // nor the operational name of the deployment. Those come from the live
    // `NodeConfig`, read now, while it is still possible.
    let mut identity: HashMap<String, NodeIdentitySnapshot> = HashMap::new();
    for no in &record.nodes {
        let Some(peer) = peers.iter().find(|p| p.node_id == no.node_id) else {
            continue;
        };
        if let Ok(snapshot) = peer_node_snapshot(&state, peer).await {
            let config = snapshot.get("config");
            identity.insert(
                no.node_id.clone(),
                NodeIdentitySnapshot {
                    model_path: config
                        .and_then(|c| c.get("model_path"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    mmproj_path: config
                        .and_then(|c| c.get("mmproj_path"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    mtp_draft_path: config
                        .and_then(|c| c.get("mtp_draft_path"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    model_label: config
                        .and_then(|c| c.get("model_label"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    deployment_name: config
                        .and_then(|c| c.get("deployment_name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    single_peer: config
                        .and_then(|c| c.get("single_peer"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(no.peer_count <= 1),
                    no_embeddings: config
                        .and_then(|c| c.get("no_embeddings"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                },
            );
        }
    }

    // ── 2. Unload every machine that announces this deploy_id ────────────────
    let target = UndeployTarget::Deployment {
        deploy_id: record.deploy_id.clone(),
        label: record.model.label.clone(),
    };
    let serving: Vec<PeerInfo> = peers
        .iter()
        .filter(|p| p.total_layers > 0 && target.matches(p))
        .cloned()
        .collect();
    if serving.is_empty() {
        info!(
            "redeploy {}: nothing serving, going straight to the start",
            record.deploy_id
        );
    } else {
        set_job(
            job_id,
            "running",
            "unloading",
            &format!("{} machine(s)", serving.len()),
        )
        .await;
        let unloads = futures::future::join_all(serving.iter().map(|peer| {
            let state = state.clone();
            let key = key.clone();
            let self_id = self_id.clone();
            async move {
                (
                    peer.hostname.clone(),
                    release_one(&state, peer, &key, &self_id).await,
                )
            }
        }))
        .await;
        for (host, result) in unloads {
            match result {
                Ok(()) => info!("redeploy: {host} unloaded"),
                Err(e) => warn!("redeploy: {host} did not unload: {e}"),
            }
        }
    }

    // ── 3. Bring up one machine at a time, waiting for each to come back ──────
    //
    // Sequential on purpose: reloading the model is the node's memory peak,
    // and telling the whole cluster to do it together is the shortest path to
    // several crashes from lack of VRAM at the same time.
    let mut results: Vec<RedeployOutcome> = Vec::new();
    let mut accepted: HashMap<String, RuntimeParams> = HashMap::new();
    for (i, no) in record.nodes.iter().enumerate() {
        set_job(
            job_id,
            "running",
            "starting",
            &format!("{} ({}/{})", no.hostname, i + 1, total),
        )
        .await;
        set_job_progress(job_id, 100.0 * i as f64 / total as f64).await;

        // `no.config` is already the effective configuration of the active version (the
        // deployment parameters with the per-machine adjustments embedded), so
        // a version redeploy gives each machine back exactly what
        // it was running. `req.per_node` re-applies on top what the screen changed.
        let mut params = no.config.clone();
        // This machine's own overrides (the version keeps them) apply on top of what it runs with.
        let own = record.node_overrides(&no.node_id);
        if !own.is_empty() {
            params = own.apply(&params);
        }
        if let Some(adjustment) = req.per_node.get(&no.node_id) {
            if let Some(g) = adjustment.n_gpu_layers {
                params.n_gpu_layers = g;
            }
            if let Some(k) = adjustment.offload_kqv {
                params.offload_kqv = k;
            }
        }

        let snap = identity.get(&no.node_id);
        let slice = ReconfigureSlice {
            model_path: snap
                .and_then(|s| s.model_path.clone())
                .or_else(|| no.source_model_path.clone()),
            mmproj_path: snap.and_then(|s| s.mmproj_path.clone()),
            mtp_draft_path: snap.and_then(|s| s.mtp_draft_path.clone()),
            part_sha256: no.part_sha256.clone(),
            model_label: snap
                .and_then(|s| s.model_label.clone())
                .unwrap_or_else(|| record.model.label.clone()),
            deployment_name: snap
                .map(|s| s.deployment_name.clone())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| record.model.label.clone()),
            deploy_id: record.deploy_id.clone(),
            start_layer: no.start_layer,
            end_layer: no.end_layer,
            total_layers: record.model.total_layers,
            peer_count: no.peer_count,
            peer_index: no.peer_index,
            primary: no.primary,
            single_peer: snap.map(|s| s.single_peer).unwrap_or(no.peer_count <= 1),
            no_embeddings: Some(snap.map(|s| s.no_embeddings).unwrap_or(false)),
            tp: None,
        };

        match relaunch_one(&state, &peers, no, &self_id, &key, &params, slice).await {
            None => {
                info!("redeploy: {} started", no.hostname);
                accepted.insert(no.node_id.clone(), params);
                results.push(RedeployOutcome {
                    node_id: no.node_id.clone(),
                    hostname: no.hostname.clone(),
                    reconfigured: true,
                    error: None,
                });
            }
            Some(e) => {
                warn!("redeploy: {} did not start: {e}", no.hostname);
                results.push(RedeployOutcome {
                    node_id: no.node_id.clone(),
                    hostname: no.hostname.clone(),
                    reconfigured: false,
                    error: Some(e),
                });
            }
        }
    }

    finish_redeploy(&state, &mut record, accepted, results, job_id).await
}

/// The snapshot of what a node was running, taken before the unload.
pub(super) struct NodeIdentitySnapshot {
    pub(super) model_path: Option<String>,
    pub(super) mmproj_path: Option<String>,
    /// The external MTP draft the tail had loaded. It comes from the model's
    /// sidecar link and rarely sits next to the slice, so the slice's own
    /// folder cannot find it again.
    pub(super) mtp_draft_path: Option<String>,
    pub(super) model_label: Option<String>,
    pub(super) deployment_name: String,
    pub(super) single_peer: bool,
    pub(super) no_embeddings: bool,
}

/// Writes the accepted parameters to the record, publishes and builds the job's result.
pub(super) async fn finish_redeploy(
    state: &GatewayState,
    record: &mut crate::deployment::Deployment,
    mut accepted: HashMap<String, RuntimeParams>,
    results: Vec<RedeployOutcome>,
    job_id: &str,
) -> Result<usize, String> {
    // Only what the machine actually accepted goes into the record. One that refused
    // stays described with the old configuration - the record has to say what
    // **is**, not what was asked for.
    for no in record.nodes.iter_mut() {
        if let Some(p) = accepted.remove(&no.node_id) {
            no.config = p;
        }
    }
    // What each machine actually accepted also goes into the snapshot of the
    // active version, so the history does not diverge from the list the runtime reads.
    record.commit_active_nodes();
    publish_record(state, record).await;

    let failures = results.iter().filter(|r| !r.reconfigured).count();
    set_job_progress(job_id, 100.0).await;
    {
        let mut map = jobs().write().await;
        if let Some(j) = map.get_mut(job_id) {
            j.outputs = results
                .iter()
                .map(|r| match &r.error {
                    Some(e) => format!("{}: {e}", r.hostname),
                    None => format!("{}: reloaded", r.hostname),
                })
                .collect();
        }
    }
    Ok(failures)
}

/// Brings **one** machine up for the range it already has, with the new parameters,
/// and waits for it to serve again before returning.
pub(super) async fn relaunch_one(
    state: &GatewayState,
    peers: &[PeerInfo],
    no: &crate::deployment::DeploymentNode,
    self_id: &str,
    _key: &str,
    params: &RuntimeParams,
    slice: ReconfigureSlice,
) -> Option<String> {
    if no.node_id == self_id {
        if let Err(e) =
            crate::webui::reconfigure_local(state.clone(), params.clone(), Some(slice)).await
        {
            return Some(e);
        }
    } else {
        let Some(peer) = peers.iter().find(|p| p.node_id == no.node_id) else {
            return Some("peer not found".to_string());
        };
        let body = json!({ "params": params, "slice": slice });
        // Reloading weights takes minutes, not seconds.
        match state
            .control_post_json(peer, "/api/ui/reconfigure", &body, REDEPLOY_READY_TIMEOUT)
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                let status = r.status();
                let detail = r.text().await.unwrap_or_default();
                return Some(format!("the machine refused: HTTP {status} {detail}"));
            }
            Err(e) => return Some(format!("machine unreachable: {e}")),
        }
    }

    // The call only triggers the load; wait for the machine to report `ready`.
    if let Some(peer) = peers.iter().find(|p| p.node_id == no.node_id) {
        if let Err(e) =
            wait_for_peer_serving_with_timeout(state, peer, REDEPLOY_READY_TIMEOUT).await
        {
            return Some(e);
        }
    }
    None
}

/// The old `redeploy`: reconfigures each node in place, without unload. Only Tensor
/// Parallelism still uses this path - see `run_redeploy`.
pub(super) async fn run_redeploy_in_place(
    state: &GatewayState,
    peers: &[PeerInfo],
    record: &mut crate::deployment::Deployment,
    req: &RedeployRequest,
    self_id: &str,
    key: &str,
    job_id: &str,
) -> Result<usize, String> {
    let total = record.nodes.len().max(1);
    let mut results: Vec<RedeployOutcome> = Vec::new();
    let mut accepted: HashMap<String, RuntimeParams> = HashMap::new();

    for (i, no) in record.nodes.iter().enumerate() {
        set_job(
            job_id,
            "running",
            "reloading",
            &format!("{} ({}/{})", no.hostname, i + 1, total),
        )
        .await;
        set_job_progress(job_id, 100.0 * i as f64 / total as f64).await;

        // See `run_redeploy`: `no.config` is the effective configuration of the version.
        let mut params = no.config.clone();
        // This machine's own overrides (the version keeps them) apply on top of what it runs with.
        let own = record.node_overrides(&no.node_id);
        if !own.is_empty() {
            params = own.apply(&params);
        }
        if let Some(adjustment) = req.per_node.get(&no.node_id) {
            if let Some(g) = adjustment.n_gpu_layers {
                params.n_gpu_layers = g;
            }
            if let Some(k) = adjustment.offload_kqv {
                params.offload_kqv = k;
            }
        }

        match reconfigure_one(state, peers, &no.node_id, self_id, key, &params).await {
            None => {
                info!("redeploy (tp): {} reloaded", no.hostname);
                accepted.insert(no.node_id.clone(), params);
                results.push(RedeployOutcome {
                    node_id: no.node_id.clone(),
                    hostname: no.hostname.clone(),
                    reconfigured: true,
                    error: None,
                });
            }
            Some(e) => {
                warn!("redeploy (tp): {} did not reload: {e}", no.hostname);
                results.push(RedeployOutcome {
                    node_id: no.node_id.clone(),
                    hostname: no.hostname.clone(),
                    reconfigured: false,
                    error: Some(e),
                });
            }
        }
    }

    finish_redeploy(state, record, accepted, results, job_id).await
}

pub(super) async fn validate_redeploy_wire_dtype(
    state: &GatewayState,
    peers: &[PeerInfo],
    nodes: &[crate::deployment::DeploymentNode],
    self_id: &str,
    dtype: crate::wire::WireDtype,
) -> Result<(), String> {
    let Some(required_capability) = crate::pipeline::wire_dtype_capability(dtype) else {
        return Ok(());
    };

    let mut blocked = Vec::new();
    for node in nodes {
        if node.node_id == self_id {
            continue;
        }
        let Some(peer) = peers.iter().find(|peer| peer.node_id == node.node_id) else {
            blocked.push(format!("{}: peer not found", node.hostname));
            continue;
        };
        let response = match state.control_get(peer, "/api/ui/node", PEER_TIMEOUT).await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                blocked.push(format!(
                    "{}: /api/ui/node answered HTTP {}",
                    node.hostname,
                    response.status()
                ));
                continue;
            }
            Err(error) => {
                blocked.push(format!(
                    "{}: could not query /api/ui/node: {error}",
                    node.hostname
                ));
                continue;
            }
        };
        let body = match response.json::<serde_json::Value>().await {
            Ok(body) => body,
            Err(error) => {
                blocked.push(format!(
                    "{}: invalid /api/ui/node response: {error}",
                    node.hostname
                ));
                continue;
            }
        };
        let supports_dtype = body
            .get("inference_frame_capabilities")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|capabilities| {
                capabilities
                    .iter()
                    .any(|capability| capability.as_str() == Some(required_capability))
            });
        if !supports_dtype {
            blocked.push(format!(
                "{}: does not announce {}",
                node.hostname, required_capability
            ));
        }
    }

    if blocked.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Hidden state precision {} was requested, but the negotiation failed: {}",
            dtype.as_str(),
            blocked.join("; ")
        ))
    }
}

/// Sends **one** machine to reload with other parameters.
pub(super) async fn reconfigure_one(
    state: &GatewayState,
    peers: &[PeerInfo],
    node_id: &str,
    self_id: &str,
    _key: &str,
    params: &RuntimeParams,
) -> Option<String> {
    let body = json!({ "params": params });

    if node_id == self_id {
        let response = crate::webui::reconfigure_local(state.clone(), params.clone(), None).await;
        return response.err();
    }

    let peer = peers.iter().find(|p| p.node_id == node_id)?;
    // Reloading weights takes minutes, not seconds: the `PEER_TIMEOUT` of a
    // control call would kill the connection in the middle of a load that
    // will end fine.
    match state
        .control_post_json(
            peer,
            "/api/ui/reconfigure",
            &body,
            Duration::from_secs(1800),
        )
        .await
    {
        Ok(r) if r.status().is_success() => None,
        Ok(r) => {
            let status = r.status();
            let detail = r.text().await.unwrap_or_default();
            Some(format!("the machine refused: HTTP {status} {detail}"))
        }
        Err(e) => Some(format!("machine unreachable: {e}")),
    }
}
