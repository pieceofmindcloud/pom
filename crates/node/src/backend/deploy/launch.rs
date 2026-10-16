use super::*;
// ── Carrying it out ──────────────────────────────────────────────────────────

pub(super) async fn deploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeployRequest>,
) -> Response {
    // The cluster is re-surveyed rather than trusted from the preview: the
    // screen's copy may be a minute old, and a machine that has left in the
    // meantime would otherwise be handed a slice. What is *not* recomputed is
    // the operator's choice — it is validated against the fresh survey and
    // refused if it no longer holds.
    let survey = survey(
        &state,
        &req.group,
        req.kind,
        req.selection.as_deref(),
        req.tp.as_ref(),
    )
    .await;
    if let Some(reason) = survey.metadata_pending {
        return bad(json!({ "error": reason }));
    }
    let mut machines = survey.machines;
    let pairwise_rtt = survey.pairwise_rtt;
    let layer_options = if req.kind == DeployKind::Whole && req.whole_slice.is_some() {
        whole_layer_options_for_plan(&state, &machines, &req.group).await
    } else {
        None
    };
    if let (Some(selection), Some(options)) = (&req.whole_slice, &layer_options) {
        if let Err(error) = add_virtual_whole_slices(&mut machines, &req.group, selection, options)
        {
            return bad(json!({ "error": error }));
        }
    }
    let deployment_name =
        match normalize_deployment_name(req.deployment_name.as_deref(), &req.group) {
            Ok(name) => name,
            Err(error) => return bad(json!({ "error": error })),
        };
    let params = effective_params_for_kind(req.kind, &req.params);
    let mut plan = plan_with_rtt_options(
        &req.group,
        &machines,
        req.selection.as_deref(),
        &params,
        req.merge,
        req.kind,
        req.tp.clone(),
        req.strategy,
        &deployment_name,
        &req.preferred_node_ids,
        &pairwise_rtt,
        layer_options,
        // The projector only matters with vision on: without it, a path the
        // screen still holds must not refuse the deployment.
        req.mmproj_path.as_deref().filter(|_| req.vision),
    );
    let (links, links_stale) = crate::webui::load_sidecar_links(&state).await;
    attach_sidecars(
        &mut plan,
        &machines,
        &survey.sidecars,
        survey.model_shape.as_ref(),
        &links,
        links_stale,
        req.vision,
        req.vision_choice.as_ref(),
        params.mtp_enabled,
        req.mtp_choice.as_ref(),
    );
    // A projector still to be copied to the head has no path there yet; it is
    // resolved on the post-copy survey in `lifecycle::run`.
    let vision_arriving = plan
        .sidecars
        .vision
        .as_ref()
        .is_some_and(|slot| slot.needs_copy());
    if req.vision && plan.mmproj_path.is_none() && !vision_arriving {
        return bad(json!({
            "error": plan.mmproj_reason.as_deref().unwrap_or("this model has no vision: no compatible projector was proven for the GGUF")
        }));
    }

    // The shardability check reads the GGUF on the primary; the deploy redoes everything
    // on the fresh survey, so it also re-validates here - it does not trust the
    // preview the screen showed.
    if req.kind == DeployKind::TensorParallel && plan.blocked.is_none() && !has_tp_parts(&machines)
    {
        if let Some(tp) = &req.tp {
            if let Some(reason) = tp_shardability_check(&state, &machines, tp).await {
                plan.blocked = Some(reason);
            }
        }
    }

    if plan.blocked.is_none() {
        if let Some(service) = params.inference_service.as_deref() {
            let targets: Vec<String> = plan
                .steps
                .iter()
                .map(|step| step.target_node_id.clone())
                .collect();
            plan.blocked = crate::plugin_engine::deploy_block(
                &state,
                service,
                req.kind == DeployKind::Whole,
                &targets,
            )
            .await;
        }
    }
    if let Some(reason) = plan.blocked {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": reason })),
        )
            .into_response();
    }
    if plan.steps.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "nothing to deploy" })),
        )
            .into_response();
    }

    // The identity is resolved before the job is created and before touching a
    // peer. If the GGUF cannot be proven by hash, or the slice carries no
    // pom_id, the operator gets the refusal on the deploy action itself.
    let identity = match resolve_requested_model_identity(&state, &plan, &machines).await {
        Ok(identity) => identity,
        Err(error) => return bad(json!({ "error": error })),
    };

    // Reuse the `deploy_id` of a deployment of the same **shape**: same
    // `kind`, same proven model identity and the same set of
    // `(node_id, range, rank)`. Without proof of identity it does not deduplicate - it mints a
    // new id, as before. Changing machine or range changes the shape and also
    // mints a new id (that is a Deploy Studio matter, not a version).
    let ranges: Vec<(String, u32, u32, Option<u32>)> = if !plan.tp_targets.is_empty() {
        plan.tp_targets
            .iter()
            .map(|t| {
                (
                    t.target_node_id.clone(),
                    t.start_layer,
                    t.end_layer,
                    Some(t.rank),
                )
            })
            .collect()
    } else {
        plan.steps
            .iter()
            .map(|s| (s.target_node_id.clone(), s.start_layer, s.end_layer, None))
            .collect()
    };
    let reuse_id = match identity.proof.as_ref() {
        Some(proof) => state
            .database
            .deployments()
            .ok()
            .as_deref()
            .and_then(|all| {
                crate::deployment::matching_shape(all, record_kind(req.kind), proof, &ranges)
                    .map(|d| d.deploy_id.clone())
            }),
        None => None,
    };

    let job_id = uuid::Uuid::new_v4().to_string();
    // A whole model is one machine, and "em 1 nós" is the kind of seam that
    // makes an operator wonder what else was written for the other case.
    let title = format!(
        "Deploy {} on {} {}",
        deployment_name,
        plan.steps.len(),
        if plan.steps.len() == 1 {
            "node"
        } else {
            "nodes"
        }
    );
    jobs().write().await.insert(
        job_id.clone(),
        new_job(&job_id, "deploy", title, "preparing"),
    );

    // Minted before anything is brought up: it is the name of this deployment, and it goes along
    // with every `apply` so the node knows which set it became part of.
    let record_plan = match reuse_id {
        Some(id) => match find_record(&state, &id).await {
            Some(existing) => RecordPlan::ReuseVersion(Box::new(existing)),
            // The record vanished between `matching_shape` and now: treat it as new.
            None => RecordPlan::New {
                deploy_id: crate::deployment::new_deploy_id(),
            },
        },
        None => RecordPlan::New {
            deploy_id: crate::deployment::new_deploy_id(),
        },
    };
    let deploy_id = record_plan.deploy_id().to_string();
    let reused = matches!(record_plan, RecordPlan::ReuseVersion(_));
    state.plugins.publish_event(
        "deployment.started",
        json!({
            "kind": "deploy",
            "deploy_id": deploy_id,
            "job_id": job_id,
            "deployment_name": deployment_name,
            "model": req.group,
            "nodes": plan.steps.len(),
            "reused": reused,
        }),
    );
    let id = job_id.clone();
    tokio::spawn(async move {
        match run(
            state,
            plan,
            params,
            identity,
            record_plan,
            req.vision,
            req.keep_existing,
            &id,
        )
        .await
        {
            Ok(()) => set_job(&id, "done", "pronto", "the model is serving on the network").await,
            Err(e) => {
                warn!("deployment failed: {e}");
                set_job(&id, "failed", "failed", &e).await;
            }
        }
    });

    Json(json!({ "started": true, "job_id": job_id, "deploy_id": deploy_id, "reused": reused }))
        .into_response()
}

/// The plan's steps as `launch` wants them, resolved against a fresh survey.
///
/// Split out of `run` because this is the whole of phase 2's decision-making
/// and the only part of the deploy that can be checked without a cluster: what
/// each machine is told to load, and from which of *its own* paths. The survey
/// it reads is deliberately the one taken after the copies, so a file that
/// landed somewhere unexpected is still found where it actually is.
///
/// A whole model needs nothing special here and gets nothing: it is a step
/// whose range happens to be the entire model, so it comes out as a plain
/// target with `skip_split` — which is also what makes the receiving peer a
/// `single_peer`, running without hidden-state extraction.
/// Static validations of a tensor parallelism group, before the shardability
/// check (which reads the GGUF). Returns `Some(reason)` when the deploy cannot
/// even start: invalid size, primary/workers outside the cluster, someone
/// without the whole model, or a primary without a derivable inference URL.
pub(super) fn tp_group_checks(tp: &TpLaunch, machines: &[Machine], group: &str) -> Option<String> {
    if tp.size < 2 {
        return Some("a tensor-parallel group needs at least 2 ranks".to_string());
    }
    let Some(primary) = machines.iter().find(|m| m.node_id == tp.primary_node_id) else {
        return Some(format!(
            "the primary {} is not in the cluster",
            tp.primary_node_id
        ));
    };
    if primary.inference_url.is_none() {
        return Some(format!(
            "could not find the inference URL of the primary {} (it must announce the inference port)",
            primary.label()
        ));
    }

    let expected_workers = (tp.size - 1) as usize;
    if tp.worker_node_ids.len() != expected_workers {
        return Some(format!(
            "there are {} ranks, so the group needs {} worker{} (you marked {})",
            tp.size,
            expected_workers,
            if expected_workers == 1 { "" } else { "s" },
            tp.worker_node_ids.len(),
        ));
    }

    let pre_sliced = has_tp_parts(machines);

    let mut seen = std::collections::HashSet::new();
    for worker_id in &tp.worker_node_ids {
        if worker_id == &tp.primary_node_id {
            return Some("a worker cannot be the primary itself".to_string());
        }
        if !seen.insert(worker_id.clone()) {
            return Some(format!("worker {worker_id} was marked twice"));
        }
        let Some(worker) = machines.iter().find(|m| &m.node_id == worker_id) else {
            return Some(format!("worker {worker_id} is not in the cluster"));
        };
        if worker.inference_url.is_none() {
            return Some(format!(
                "could not find the inference URL of worker {}",
                worker.label()
            ));
        }
    }

    if pre_sliced {
        let total = model_size(&candidates(machines)).value().unwrap_or(0);
        let end = total.saturating_sub(1);
        for rank in 0..tp.size {
            if !machines
                .iter()
                .any(|machine| machine.holds_tp(0, end, rank, tp.size).is_some())
            {
                return Some(format!(
                    "rank {}of{} of TP group {group} exists on no machine of the cluster",
                    rank + 1,
                    tp.size
                ));
            }
        }
        return None;
    }

    if !primary.held.iter().any(|s| s.start_layer == 0) {
        return Some(format!(
            "the primary {} does not have the whole model {group}",
            primary.label()
        ));
    }
    for worker_id in &tp.worker_node_ids {
        let worker = machines
            .iter()
            .find(|m| &m.node_id == worker_id)
            .expect("worker validated");
        if !worker.held.iter().any(|s| s.start_layer == 0) {
            return Some(format!(
                "worker {} does not have the whole model {group} to extract the shard from",
                worker.label()
            ));
        }
    }
    None
}

pub(super) fn has_tp_parts(machines: &[Machine]) -> bool {
    machines
        .iter()
        .any(|machine| machine.held.iter().any(|shard| shard.tp_size.is_some()))
}

/// Resolves the file of each pre-sliced TP rank. The target may already have the rank;
/// in that case there is no transfer. If it does not yet, we pick any
/// peer that has exactly the same `tp_rank` and carry that peer's hash and path
/// over for the content-based transfer.
pub(super) fn tp_targets_for(
    tp: &TpLaunch,
    machines: &[Machine],
    group: &str,
    total_layers: Option<u32>,
    n_gpu_layers: Option<u32>,
    offload_kqv: Option<bool>,
) -> (Vec<TpTarget>, u64, Option<String>) {
    let total = total_layers.unwrap_or(0);
    let end_layer = total.saturating_sub(1);
    let mut transfer_bytes: u64 = 0;
    let mut blocked = None;
    let mut targets = Vec::with_capacity(tp.size as usize);

    for rank in 0..tp.size {
        let target_node_id = if rank == 0 {
            tp.primary_node_id.clone()
        } else {
            tp.worker_node_ids
                .get(rank as usize - 1)
                .cloned()
                .unwrap_or_default()
        };
        let target = machines.iter().find(|m| m.node_id == target_node_id);
        let held = target.and_then(|m| m.holds_tp(0, end_layer, rank, tp.size));
        let source = if held.is_none() {
            machines.iter().find(|m| {
                m.node_id != target_node_id && m.holds_tp(0, end_layer, rank, tp.size).is_some()
            })
        } else {
            None
        };
        let file = held.or_else(|| source.and_then(|m| m.holds_tp(0, end_layer, rank, tp.size)));
        let ready = held.is_some();
        let problem = if target_node_id.is_empty() {
            Some(format!("rank {}of{} has no machine", rank + 1, tp.size))
        } else if target.is_none() {
            Some(format!("machine {target_node_id} is not in the cluster"))
        } else if file.is_none() {
            Some(format!(
                "rank {}of{} of TP group {group} exists on no machine of the cluster",
                rank + 1,
                tp.size
            ))
        } else {
            None
        };
        if let Some(reason) = &problem {
            blocked.get_or_insert(reason.clone());
        }
        if !ready && file.is_some() {
            transfer_bytes = transfer_bytes.saturating_add(file.map(|s| s.size_bytes).unwrap_or(0));
        }

        let source_shard = source.and_then(|m| m.holds_tp(0, end_layer, rank, tp.size));
        let target_shard = held;
        let shard = target_shard.or(source_shard).or(file);
        targets.push(TpTarget {
            rank,
            size: tp.size,
            start_layer: 0,
            end_layer,
            total_layers,
            name: format!("{group} · {}of{}", rank + 1, tp.size),
            size_bytes: shard.map(|s| s.size_bytes).unwrap_or(0),
            target_node_id,
            target_host: target.map(|m| m.label()).unwrap_or_default(),
            ready,
            source_node_id: source.map(|m| m.node_id.clone()),
            source_host: source.map(|m| m.label()),
            source_path: source_shard.map(|s| s.path.clone()),
            source_sha256: source_shard.and_then(|s| s.sha256.clone()),
            target_path: target_shard.map(|s| s.path.clone()),
            problem,
            n_gpu_layers,
            offload_kqv,
        });
    }

    (targets, transfer_bytes, blocked)
}

pub(super) fn tp_identity(
    size: u32,
    group_id: &str,
    rank: u32,
    role: &str,
    members: &[crate::network::TpMember],
) -> crate::network::TpIdentity {
    let rank0 = members.first();
    crate::network::TpIdentity {
        role: role.into(),
        group_id: group_id.into(),
        size,
        rank,
        rank0_url: rank0.map(|m| m.url.clone()),
        // The collective's certificate is pinned by this id; without it the default client
        // refuses the cluster's self-signed one and the all-reduce fails silently.
        rank0_node_id: rank0.map(|m| m.node_id.clone()),
        // Every member receives the whole list: rank 0 uses it to broadcast the
        // decode, and the others to know who they are.
        members: members.to_vec(),
    }
}

/// The members of the TP group in rank order (0 = primary), with the
/// inference URL of each.
///
/// Refuses early whoever does not announce an inference port: without it rank 0 would have no
/// way to trigger that rank's decode, and the group would hang on the first barrier
/// instead of failing here.
pub(super) fn tp_members(
    tp: &TpLaunch,
    machines: &[Machine],
) -> Result<Vec<crate::network::TpMember>, String> {
    let de = |node_id: &str| -> Result<crate::network::TpMember, String> {
        let m = machines
            .iter()
            .find(|m| m.node_id == node_id)
            .ok_or_else(|| format!("{node_id} is not in the cluster"))?;
        let url = m.inference_url.clone().ok_or_else(|| {
            format!(
                "could not find the inference URL of {} (it must announce the inference port)",
                m.label()
            )
        })?;
        let mut url_candidates = if m.inference_candidates.is_empty() {
            vec![url.clone()]
        } else {
            m.inference_candidates.clone()
        };
        if !url_candidates.iter().any(|candidate| candidate == &url) {
            url_candidates.insert(0, url.clone());
        }
        Ok(crate::network::TpMember {
            node_id: node_id.to_string(),
            url,
            url_candidates,
            addr_candidates: if m.addr_candidates.is_empty() {
                Vec::new()
            } else {
                m.addr_candidates.clone()
            },
            libp2p_peer_id: m.libp2p_peer_id.clone(),
            supports_tunnel: m.supports_tunnel,
        })
    };
    let mut out = vec![de(&tp.primary_node_id)?];
    for w in &tp.worker_node_ids {
        out.push(de(w)?);
    }
    Ok(out)
}

pub(super) fn launch_targets(
    steps: &[Step],
    machines: &[Machine],
    kind: DeployKind,
    tp: Option<&TpLaunch>,
    tp_targets: &[TpTarget],
) -> Result<Vec<LaunchTarget>, String> {
    // The collective's URL (rank 0) is derived from the primary, not typed: it is the inference
    // port the node itself announces. `tp.is_some()` ⟺ `rank0_url.is_some()`.
    let rank0_url = tp
        .map(|t| {
            machines
                .iter()
                .find(|m| m.node_id == t.primary_node_id)
                .and_then(|m| m.inference_url.clone())
                .ok_or_else(|| {
                    format!(
                        "could not find the inference URL of the primary {}",
                        t.primary_node_id
                    )
                })
        })
        .transpose()?;
    let _ = rank0_url;
    let members = match tp {
        Some(t) => tp_members(t, machines)?,
        None => Vec::new(),
    };

    if let Some(tp) = tp.filter(|_| !tp_targets.is_empty()) {
        let mut targets = Vec::with_capacity(tp_targets.len());
        for target in tp_targets {
            let machine = machines
                .iter()
                .find(|machine| machine.node_id == target.target_node_id)
                .ok_or_else(|| format!("{} is not in the cluster", target.target_node_id))?;
            let held = machine
                .holds_tp(
                    target.start_layer,
                    target.end_layer,
                    target.rank,
                    target.size,
                )
                .ok_or_else(|| {
                    format!(
                        "{} did not find rank {}of{} after the copy",
                        machine.label(),
                        target.rank + 1,
                        target.size
                    )
                })?;
            targets.push(LaunchTarget {
                node_id: target.target_node_id.clone(),
                source_model_path: held.path.clone(),
                start_layer: target.start_layer,
                end_layer: target.end_layer,
                skip_split: true,
                part_sha256: held.sha256.clone(),
                merge: None,
                n_gpu_layers: target.n_gpu_layers,
                offload_kqv: target.offload_kqv,
                no_embeddings: None,
                peer_index: None,
                peer_count: None,
                tp: Some(tp_identity(
                    tp.size,
                    &target.name,
                    target.rank,
                    if target.rank == 0 {
                        "primary"
                    } else {
                        "worker"
                    },
                    &members,
                )),
                mmproj_path: (target.rank == 0)
                    .then(|| held.mmproj_path.clone())
                    .flatten(),
                mtp_draft_path: None,
                mtp_draft_file: None,
                mmproj_file: None,
                use_mmproj: false,
            });
        }
        return Ok(targets);
    }

    // Replicas of the same range occupy the same logical pipeline stage. Each
    // copy must receive the same peer_index/peer_count to export the
    // residual exactly like that stage's main route.
    let mut pipeline_ranges: Vec<(u32, u32)> = Vec::new();
    for step in steps {
        let range = (step.start_layer, step.end_layer);
        if !pipeline_ranges.contains(&range) {
            pipeline_ranges.push(range);
        }
    }
    let pipeline_peer_count = pipeline_ranges.len() as u32;
    let pipeline_fields = |step: &Step| {
        if kind != DeployKind::Shards && !(kind == DeployKind::Whole && steps.len() > 1) {
            return (None, None);
        }
        let index = pipeline_ranges
            .iter()
            .position(|range| *range == (step.start_layer, step.end_layer))
            .unwrap_or(0) as u32;
        (Some(index), Some(pipeline_peer_count))
    };

    let mut targets = Vec::new();
    for step in steps {
        let target_machine = machines.iter().find(|m| m.node_id == step.target_node_id);

        if step.merged {
            // Each part's path on the target is resolved the same way; the
            // apply merges them into one file there before serving.
            let mut merge: Vec<MergeSource> = Vec::new();
            for part in &step.parts {
                let path = target_machine
                    .and_then(|m| m.holds_ready(part.start_layer, part.end_layer))
                    .map(|s| s.path.clone())
                    .ok_or_else(|| {
                        format!(
                            "{} did not find slice L{}-{} after the copy",
                            step.target_host, part.start_layer, part.end_layer
                        )
                    })?;
                merge.push(MergeSource {
                    source_model_path: path,
                    start_layer: part.start_layer,
                    end_layer: part.end_layer,
                });
            }
            let anchor = merge
                .first()
                .map(|m| m.source_model_path.clone())
                .ok_or_else(|| format!("{} has no slices to merge", step.target_host))?;
            let (peer_index, peer_count) = pipeline_fields(step);

            targets.push(LaunchTarget {
                node_id: step.target_node_id.clone(),
                // Unused when `merge` is set: the apply validates the parts and
                // never reads this. Present only because `LaunchTarget` requires
                // the field; the first part names a real file for the record.
                source_model_path: anchor,
                start_layer: step.start_layer,
                end_layer: step.end_layer,
                // Nothing to cut: the merge produces the exact range.
                skip_split: true,
                // The merge produces a new file, whose hash nobody knows
                // before merging: whoever merges is the one that names it.
                part_sha256: None,
                merge: Some(merge),
                n_gpu_layers: step.n_gpu_layers,
                offload_kqv: step.offload_kqv,
                no_embeddings: None,
                peer_index,
                peer_count,
                tp: tp.map(|t| tp_identity(t.size, &step.name, 0, "primary", &members)),
                mmproj_path: if step.start_layer == 0 {
                    target_machine.and_then(|machine| {
                        step.parts.first().and_then(|part| {
                            machine
                                .holds_ready(part.start_layer, part.end_layer)
                                .and_then(|shard| shard.mmproj_path.clone())
                        })
                    })
                } else {
                    None
                },
                mtp_draft_path: None,
                mtp_draft_file: None,
                mmproj_file: None,
                use_mmproj: false,
            });
            continue;
        }

        let held = target_machine
            .and_then(|m| m.holds_ready(step.start_layer, step.end_layer))
            .ok_or_else(|| {
                format!(
                    "{} did not find {} after the copy",
                    step.target_host,
                    kind.unit(&step.name, step.start_layer, step.end_layer)
                )
            })?;
        let (peer_index, peer_count) = pipeline_fields(step);

        targets.push(LaunchTarget {
            node_id: step.target_node_id.clone(),
            source_model_path: held.path.clone(),
            start_layer: step.start_layer,
            end_layer: step.end_layer,
            // Already a file of exactly this range: nothing to cut. A whole
            // model is the same statement about the whole range.
            skip_split: true,
            part_sha256: held.sha256.clone(),
            merge: None,
            n_gpu_layers: step.n_gpu_layers,
            offload_kqv: step.offload_kqv,
            no_embeddings: None,
            peer_index,
            peer_count,
            tp: tp.map(|t| tp_identity(t.size, &step.name, 0, "primary", &members)),
            mmproj_path: (step.start_layer == 0)
                .then(|| held.mmproj_path.clone())
                .flatten(),
            mtp_draft_path: None,
            mtp_draft_file: None,
            mmproj_file: None,
            use_mmproj: false,
        });
    }

    // Tensor parallelism group: besides the primary (the whole-model step),
    // each worker becomes a target that extracts its own tensor shard.
    if let Some(tp) = tp {
        let head = steps.first().ok_or("TP plan without a step")?;
        for (i, worker_id) in tp.worker_node_ids.iter().enumerate() {
            let rank = (i + 1) as u32;
            let worker_machine = machines
                .iter()
                .find(|m| m.node_id == *worker_id)
                .ok_or_else(|| format!("worker {worker_id} is not in the cluster"))?;
            let path = worker_machine
                .holds_ready(head.start_layer, head.end_layer)
                .map(|s| s.path.clone())
                .ok_or_else(|| {
                    format!(
                        "{} does not have the model to extract the tensor shard from",
                        worker_machine.hostname
                    )
                })?;
            targets.push(LaunchTarget {
                node_id: worker_id.clone(),
                source_model_path: path,
                start_layer: head.start_layer,
                end_layer: head.end_layer,
                skip_split: true,
                // This rank's tensor shard is still to be extracted here; the
                // hash comes out of the cut.
                part_sha256: None,
                merge: None,
                n_gpu_layers: head.n_gpu_layers,
                offload_kqv: head.offload_kqv,
                no_embeddings: None,
                peer_index: None,
                peer_count: None,
                tp: Some(tp_identity(tp.size, &head.name, rank, "worker", &members)),
                mmproj_path: None,
                mtp_draft_path: None,
                mtp_draft_file: None,
                mmproj_file: None,
                use_mmproj: false,
            });
        }
    }

    Ok(targets)
}

pub(super) fn planned_model_paths(plan: &Plan) -> Vec<(String, String)> {
    let mut paths = Vec::new();
    let mut add = |node_id: &str, path: Option<&String>| {
        if !node_id.is_empty() {
            if let Some(path) = path.filter(|path| !path.is_empty()) {
                if !paths
                    .iter()
                    .any(|(node, existing)| node == node_id && existing == path)
                {
                    paths.push((node_id.to_string(), path.clone()));
                }
            }
        }
    };
    for step in &plan.steps {
        add(&step.target_node_id, step.target_path.as_ref());
        add(
            &step.source_node_id.clone().unwrap_or_default(),
            step.source_path.as_ref(),
        );
        for part in &step.parts {
            add(
                &part.source_node_id.clone().unwrap_or_default(),
                part.source_path.as_ref(),
            );
        }
    }
    for target in &plan.tp_targets {
        add(&target.target_node_id, target.target_path.as_ref());
        add(
            &target.source_node_id.clone().unwrap_or_default(),
            target.source_path.as_ref(),
        );
    }
    paths
}

pub(super) async fn whole_layer_options_for_plan(
    state: &GatewayState,
    machines: &[Machine],
    group: &str,
) -> Option<WholeLayerOptions> {
    let source = machines.iter().find_map(|machine| {
        machine
            .held
            .iter()
            .find(|shard| shard.start_layer == 0 && shard.total_layers == Some(shard.end_layer + 1))
            .map(|shard| (machine, shard.path.clone(), shard.total_layers.unwrap_or(0)))
    })?;
    let (machine, path, total_layers) = source;
    let result: Result<ModelLayerOptions, String> = if machine.node_id == state.network.node_id {
        crate::webui::model_layer_options_local(state, &path).await
    } else {
        let Some(peer) = state
            .all_peers()
            .await
            .into_iter()
            .find(|peer| peer.node_id == machine.node_id)
        else {
            return Some(WholeLayerOptions {
                source_node_id: machine.node_id.clone(),
                total_layers,
                cuts: Vec::new(),
                granularity: None,
                rule: "could not confirm the real rules of this GGUF".to_string(),
                reason: Some(format!("{} is not in the cluster", machine.label())),
            });
        };
        let request_body = crate::webui::ModelLayerOptionsRequest { path };
        let response = state
            .control_post_json(
                &peer,
                "/api/ui/model-layer-options",
                &request_body,
                PEER_TIMEOUT * 4,
            )
            .await
            .map_err(|error| {
                format!(
                    "could not read the slicing rules on {}: {error:#}",
                    machine.label()
                )
            });
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                return Some(WholeLayerOptions {
                    source_node_id: machine.node_id.clone(),
                    total_layers,
                    cuts: Vec::new(),
                    granularity: None,
                    rule: "could not confirm the real rules of this GGUF".to_string(),
                    reason: Some(error),
                });
            }
        };
        let status = response.status();
        let body = match response.json::<serde_json::Value>().await {
            Ok(body) => body,
            Err(error) => {
                return Some(WholeLayerOptions {
                    source_node_id: machine.node_id.clone(),
                    total_layers,
                    cuts: Vec::new(),
                    granularity: None,
                    rule: "could not confirm the real rules of this GGUF".to_string(),
                    reason: Some(format!("invalid slicing rules response: {error}")),
                });
            }
        };
        if !status.is_success() {
            Err(body
                .get("error")
                .and_then(|value| value.as_str())
                .unwrap_or("the peer refused the reading of the slicing rules")
                .to_string())
        } else {
            serde_json::from_value::<ModelLayerOptions>(body)
                .map_err(|error| format!("invalid slicing rules response: {error}"))
        }
    };
    match result {
        Ok(options) => Some(WholeLayerOptions {
            source_node_id: machine.node_id.clone(),
            total_layers: options.total_layers,
            cuts: options.cuts,
            granularity: options.granularity,
            rule: options.rule,
            reason: None,
        }),
        Err(error) => Some(WholeLayerOptions {
            source_node_id: machine.node_id.clone(),
            total_layers,
            cuts: Vec::new(),
            granularity: None,
            rule: "could not confirm the real rules of this GGUF".to_string(),
            reason: Some(format!("{}: {}", group, error)),
        }),
    }
}

pub(super) fn add_virtual_whole_slices(
    machines: &mut [Machine],
    group: &str,
    selection: &WholeSliceSelection,
    options: &WholeLayerOptions,
) -> Result<(), String> {
    if let Some(reason) = &options.reason {
        return Err(format!("cannot choose layers of {}: {}", group, reason));
    }
    let cut = options
        .cuts
        .iter()
        .find(|cut| cut.end_layer == selection.end_layer)
        .ok_or_else(|| {
            format!(
                "the cut at layer L{} does not respect the real rules of the GGUF",
                selection.end_layer
            )
        })?;
    if selection.source_node_id == selection.target_node_id {
        return Err("the source machine and the destination machine must be different".to_string());
    }
    let source = machines
        .iter()
        .find(|machine| machine.node_id == selection.source_node_id)
        .ok_or_else(|| "the machine proposing the model is not in the cluster".to_string())?;
    let whole = source
        .held
        .iter()
        .find(|shard| {
            shard.start_layer == 0
                && shard.end_layer + 1 == options.total_layers
                && !shard.virtual_slice
        })
        .cloned()
        .ok_or_else(|| {
            format!(
                "{} does not have the selected whole model to slice",
                source.label()
            )
        })?;
    if !machines
        .iter()
        .any(|machine| machine.node_id == selection.target_node_id)
    {
        return Err("the destination machine is not in the cluster".to_string());
    }

    let mut ranges = vec![(0, cut.end_layer, cut.size_bytes)];
    if cut.end_layer + 1 < options.total_layers {
        ranges.push((
            cut.end_layer + 1,
            options.total_layers - 1,
            cut.remainder_bytes,
        ));
    }
    let source = machines
        .iter_mut()
        .find(|machine| machine.node_id == selection.source_node_id)
        .expect("source validated");
    for (start_layer, end_layer, size_bytes) in ranges {
        if source.holds_ready(start_layer, end_layer).is_some()
            || source
                .holds(start_layer, end_layer)
                .is_some_and(|shard| shard.virtual_slice)
        {
            continue;
        }
        source.held.push(HeldShard {
            kind: HeldKind::Shard,
            start_layer,
            end_layer,
            total_layers: Some(options.total_layers),
            name: format!("{group} L{start_layer}-{end_layer}"),
            path: whole.path.clone(),
            mmproj_path: whole.mmproj_path.clone(),
            mmproj_candidates: whole.mmproj_candidates.clone(),
            mmproj_reason: whole.mmproj_reason.clone(),
            size_bytes,
            sha256: None,
            pom_id: None,
            virtual_slice: true,
            tp_rank: None,
            tp_size: None,
            is_moe: whole.is_moe,
            model_capabilities: whole.model_capabilities.clone(),
        });
    }
    Ok(())
}

/// Cut virtual ranges on the peer that owns the whole GGUF. The actual
/// fatiador remains in `webui`, so deploy only coordinates the local operation
/// and receives the content-addressed paths that transfer already understands.
pub(super) async fn materialize_slices_on_peer(
    state: &GatewayState,
    source_node_id: &str,
    request: &SliceRangesRequest,
) -> Result<crate::webui::SliceRangesResponse, String> {
    if source_node_id == state.network.node_id {
        return crate::webui::materialize_slice_ranges(state, request).await;
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == source_node_id)
        .ok_or_else(|| format!("the source machine {source_node_id} is not in the cluster"))?;
    let response = state
        .control_post_json(
            &peer,
            "/api/ui/slice-ranges",
            request,
            Duration::from_secs(3600),
        )
        .await
        .map_err(|error| format!("could not slice the model on {}: {error:#}", peer.hostname))?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.map_err(|error| {
        format!(
            "{} answered an invalid materialization: {error}",
            peer.hostname
        )
    })?;
    if !status.is_success() {
        return Err(body
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("the peer refused the local slicing")
            .to_string());
    }
    serde_json::from_value(body)
        .map_err(|error| format!("invalid slicing response from {}: {error}", peer.hostname))
}

pub(super) async fn model_identity_on_peer(
    state: &GatewayState,
    node_id: &str,
    path: &str,
    hash: bool,
) -> Result<crate::webui::ModelIdentityResponse, String> {
    if node_id == state.network.node_id {
        return crate::webui::model_identity_local(state, path, hash).await;
    }
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == node_id)
        .ok_or_else(|| format!("peer {node_id} is not in the cluster"))?;
    let request_body = crate::webui::ModelIdentityRequest {
        path: path.to_string(),
        hash,
    };
    let response = state
        .control_post_json(&peer, "/api/ui/model-identity", &request_body, PEER_TIMEOUT)
        .await
        .map_err(|e| format!("could not query the identity on {node_id}: {e:#}"))?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("invalid identity response from {node_id}: {e}"))?;
    if !status.is_success() {
        return Err(body
            .get("error")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("peer answered HTTP {status}")));
    }
    serde_json::from_value(body)
        .map_err(|e| format!("invalid identity response from {node_id}: {e}"))
}

pub(super) async fn resolve_requested_model_identity(
    state: &GatewayState,
    plan: &Plan,
    machines: &[Machine],
) -> Result<RequestedModelIdentity, String> {
    let mut pom_ids = Vec::new();
    for machine in machines {
        for shard in &machine.held {
            if let Some(pom_id) = shard.pom_id.as_deref() {
                if !pom_ids.iter().any(|known| known == pom_id) {
                    pom_ids.push(pom_id.to_string());
                }
            }
        }
    }

    let paths = planned_model_paths(plan);
    if paths.is_empty() {
        return Err(format!(
            "identity unavailable: the plan of {} does not name the selected file",
            plan.group
        ));
    }

    let mut paths_by_peer = HashMap::new();
    let needs_hash = if pom_ids.is_empty() {
        match plan.kind {
            DeployKind::Whole => true,
            DeployKind::TensorParallel if plan.tp_targets.is_empty() => true,
            DeployKind::Shards | DeployKind::TensorParallel => {
                return Err(format!(
                    "identity unavailable: the plan of {} uses slices without a pom_id; it is not safe to prove the same model by the hash of a slice",
                    plan.group
                ));
            }
        }
    } else {
        false
    };
    let mut proof = if pom_ids.len() == 1 {
        Some(crate::deployment::DeploymentIdentity::PomId {
            pom_id: pom_ids.remove(0),
        })
    } else if pom_ids.len() > 1 {
        return Err(format!(
            "identity unavailable: the plan of {} mixes more than one pom_id",
            plan.group
        ));
    } else {
        None
    };

    let mut first_hash_error = None;
    for (index, (node_id, path)) in paths.iter().enumerate() {
        let ask_hash = needs_hash && index == 0;
        let identity = match model_identity_on_peer(state, node_id, path, ask_hash).await {
            Ok(identity) => identity,
            Err(error) if ask_hash => {
                first_hash_error = Some(error);
                model_identity_on_peer(state, node_id, path, false)
                    .await
                    .map_err(|fallback| {
                        format!(
                            "identity unavailable for {}: {} (the path fallback also failed: {})",
                            plan.group,
                            first_hash_error.as_deref().unwrap_or("hash not computed"),
                            fallback
                        )
                    })?
            }
            Err(error) => {
                return Err(format!(
                    "identity unavailable for {} on {}: {}",
                    plan.group, node_id, error
                ));
            }
        };
        if let Some(sha256) = identity.sha256 {
            proof = Some(crate::deployment::DeploymentIdentity::Sha256 { sha256 });
        }
        paths_by_peer.insert(node_id.clone(), identity.canonical_path);
    }

    if proof.is_none() && needs_hash {
        info!(
            model = %plan.group,
            error = ?first_hash_error,
            "using the canonical path as fallback proof on the source peer"
        );
    }

    Ok(RequestedModelIdentity {
        proof,
        paths_by_peer,
    })
}

pub(super) async fn peer_node_snapshot(
    state: &GatewayState,
    peer: &PeerInfo,
) -> Result<serde_json::Value, String> {
    if peer.node_id == state.network.node_id {
        let config = state.runtime.config().await;
        let active_deployment = config
            .as_ref()
            .map(|config| config.deployment_name.clone())
            .unwrap_or_default();
        return Ok(json!({
            "config": config,
            "status": state.runtime.status().await,
            "active_model": state.active_model().await,
            "active_deployment": active_deployment,
        }));
    }
    let response = state
        .control_get(peer, "/api/ui/node", PEER_TIMEOUT)
        .await
        .map_err(|e| format!("{} did not answer: {e:#}", peer.hostname))?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("{} answered an invalid state: {e}", peer.hostname))?;
    if !status.is_success() {
        return Err(format!("{} answered HTTP {status}", peer.hostname));
    }
    Ok(body)
}

pub(super) async fn canonical_current_path(
    state: &GatewayState,
    peer: &PeerInfo,
    snapshot: &serde_json::Value,
) -> Result<Option<String>, String> {
    let Some(path) = snapshot
        .pointer("/config/model_path")
        .and_then(|value| value.as_str())
        .filter(|path| !path.is_empty())
    else {
        return Ok(None);
    };
    Ok(Some(
        model_identity_on_peer(state, &peer.node_id, path, false)
            .await?
            .canonical_path,
    ))
}

pub(super) async fn wait_for_peer_idle_with_timeout(
    state: &GatewayState,
    peer: &PeerInfo,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let last = match peer_node_snapshot(state, peer).await {
            Ok(snapshot) if node_is_idle(&snapshot) => return Ok(()),
            Ok(snapshot) => node_state_detail(&snapshot),
            Err(error) => error,
        };
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(format!(
                "{} did not become idle within the limit of {}s; it stayed busy: {}",
                if peer.hostname.is_empty() {
                    &peer.node_id
                } else {
                    &peer.hostname
                },
                timeout.as_secs(),
                last
            ));
        }
        tokio::time::sleep(DEPLOY_IDLE_POLL.min(deadline - now)).await;
    }
}

/// True when the node has a loaded `NodeConfig` and the inference server
/// is `ready` - the inverse of `node_is_idle`, so `redeploy` waits for each
/// machine to *serve again* before touching the next.
pub(super) fn node_is_serving_ready(snapshot: &serde_json::Value) -> bool {
    !snapshot
        .get("config")
        .map(|config| config.is_null())
        .unwrap_or(true)
        && snapshot
            .pointer("/status/state")
            .and_then(|state| state.as_str())
            == Some("ready")
}

pub(super) async fn wait_for_peer_serving_with_timeout(
    state: &GatewayState,
    peer: &PeerInfo,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let last = match peer_node_snapshot(state, peer).await {
            Ok(snapshot) if node_is_serving_ready(&snapshot) => return Ok(()),
            Ok(snapshot) => node_state_detail(&snapshot),
            Err(error) => error,
        };
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(format!(
                "{} did not resume serving within the limit of {}s; {}",
                if peer.hostname.is_empty() {
                    &peer.node_id
                } else {
                    &peer.hostname
                },
                timeout.as_secs(),
                last
            ));
        }
        tokio::time::sleep(DEPLOY_IDLE_POLL.min(deadline - now)).await;
    }
}

pub(super) async fn prepare_existing_model(
    state: &GatewayState,
    group: &str,
    desired: &RequestedModelIdentity,
    // `Some`: only these machines are released; the others that serve the
    // same model keep their deployment.
    only: Option<&[String]>,
    job_id: &str,
) -> Result<(), String> {
    let peers = state.all_peers().await;
    let mut to_release = Vec::new();

    for peer in peers.into_iter().filter(|peer| peer.total_layers > 0) {
        if peer.model.is_empty() {
            return Err(format!(
                "could not prove the identity before deploying {}: {} is busy, but does not announce which model it is serving",
                group, peer.hostname
            ));
        }
        if peer.model != group {
            continue;
        }
        if only.is_some_and(|ids| !ids.contains(&peer.node_id)) {
            continue;
        }

        let record = if peer.deploy_id.is_empty() {
            None
        } else {
            find_record(state, &peer.deploy_id).await
        };
        let known_identity = record.as_ref().and_then(|record| {
            record.model.identity.clone().or_else(|| {
                record
                    .model
                    .pom_id
                    .clone()
                    .map(|pom_id| crate::deployment::DeploymentIdentity::PomId { pom_id })
            })
        });
        if desired.proof.is_some() && desired.proof == known_identity {
            to_release.push(peer);
            continue;
        }
        if desired.proof.is_some() && known_identity.is_some() && desired.proof != known_identity {
            continue;
        }

        let snapshot = peer_node_snapshot(state, &peer).await.map_err(|error| {
            format!(
                "could not prove the identity of the model on {}: {}",
                peer.hostname, error
            )
        })?;
        let current_path = canonical_current_path(state, &peer, &snapshot)
            .await
            .map_err(|error| {
                format!(
                    "could not prove the identity of the model on {}: {}",
                    peer.hostname, error
                )
            })?;
        let current_model = record
            .as_ref()
            .map(|record| record.model.clone())
            .unwrap_or_else(|| crate::deployment::DeploymentModel {
                label: peer.model.clone(),
                total_layers: peer.total_layers,
                pom_id: None,
                identity: None,
            });
        if deployment_identity_matches(
            desired,
            &current_model,
            &peer.node_id,
            current_path.as_deref(),
        ) {
            to_release.push(peer);
        } else {
            return Err(format!(
                "could not prove that the active model on {} is the same as {}: the record has no hash/pom_id and the canonical path does not match",
                peer.hostname, group
            ));
        }
    }

    if to_release.is_empty() {
        return Ok(());
    }

    set_job(
        job_id,
        "running",
        "undeploying",
        &format!(
            "releasing {} machine(s) that already serve this model",
            to_release.len()
        ),
    )
    .await;
    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();
    let releases = futures::future::join_all(to_release.iter().map(|peer| {
        let key = key.clone();
        let self_id = self_id.clone();
        async move { (peer, release_one(state, peer, &key, &self_id).await) }
    }))
    .await;
    let failures: Vec<String> = releases
        .iter()
        .filter_map(|(peer, result)| {
            result
                .as_ref()
                .err()
                .map(|error| format!("{}: {}", peer.hostname, error))
        })
        .collect();
    if !failures.is_empty() {
        return Err(format!(
            "the new deployment was refused because the undeploy failed: {}",
            failures.join("; ")
        ));
    }

    set_job(
        job_id,
        "running",
        "waiting for idle resources",
        "waiting for the machines to free memory and stop inference",
    )
    .await;
    let waits = futures::future::join_all(to_release.iter().map(|peer| async move {
        (
            peer,
            wait_for_peer_idle_with_timeout(state, peer, DEPLOY_IDLE_TIMEOUT).await,
        )
    }))
    .await;
    let failures: Vec<String> = waits
        .iter()
        .filter_map(|(peer, result)| {
            result
                .as_ref()
                .err()
                .map(|error| format!("{}: {}", peer.hostname, error))
        })
        .collect();
    if !failures.is_empty() {
        return Err(format!(
            "the new deployment was refused because the machines did not become idle: {}",
            failures.join("; ")
        ));
    }
    Ok(())
}

/// Where the record that `run` publishes at the end of a successful `launch` comes from.
pub(super) enum RecordPlan {
    /// New `deploy_id`: builds v1 from the plan and the targets.
    New { deploy_id: String },
    /// Reused `deploy_id` (same shape, same proven identity):
    /// appends - or reactivates - a version in this record.
    ReuseVersion(Box<crate::deployment::Deployment>),
}
