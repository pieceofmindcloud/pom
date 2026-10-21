use super::*;

pub(super) async fn plan_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeployRequest>,
) -> Response {
    let deployment_name =
        match normalize_deployment_name(req.deployment_name.as_deref(), &req.group) {
            Ok(name) => name,
            Err(error) => return bad(json!({ "error": error })),
        };
    let survey = survey(
        &state,
        &req.group,
        req.kind,
        req.selection.as_deref(),
        req.tp.as_ref(),
    )
    .await;
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
    let params = effective_params_for_kind(req.kind, &req.params);
    let mut p = plan_with_rtt_options(
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
        None,
    );
    if req.kind == DeployKind::TensorParallel && p.blocked.is_none() && !has_tp_parts(&machines) {
        if let Some(tp) = &req.tp {
            if let Some(reason) = tp_shardability_check(&state, &machines, tp).await {
                p.blocked = Some(reason);
            }
        }
    }
    let (links, links_stale) = crate::webui::load_sidecar_links(&state).await;
    attach_sidecars(
        &mut p,
        &machines,
        &survey.sidecars,
        survey.model_shape.as_ref(),
        &links,
        links_stale,
        req.vision,
        req.vision_choice.as_ref(),
        true,
        req.mtp_choice.as_ref(),
    );
    if let Some(reason) = survey.metadata_pending {
        p.blocked = Some(reason);
    }
    if p.blocked.is_none() {
        if let Some(service) = req.params.inference_service.as_deref() {
            let targets: Vec<String> = p
                .steps
                .iter()
                .map(|step| step.target_node_id.clone())
                .collect();
            let roles: Vec<crate::webui::InferenceRole> = p
                .steps
                .iter()
                .map(|step| step.inference_role.unwrap_or(req.params.inference_role))
                .collect();
            p.blocked = crate::plugin_engine::deploy_block(
                &state,
                service,
                req.kind == DeployKind::Whole,
                &targets,
                &roles,
            )
            .await;
        }
    }
    // A plan already refused gains nothing from knowing whether it would fit
    // in VRAM - and it saves one network round per machine on every keystroke
    // that does not change the result.
    if p.blocked.is_none() {
        enrich_capacity_previews(&state, &machines, &mut p).await;
    }
    Json(p).into_response()
}

// ── Capacity preview ─────────────────────────────────────────────────────────

/// After the (synchronous) planner builds the steps, asks each target already
/// materialized locally how that step would occupy its VRAM with this plan's
/// parameters - and sums the result per machine.
///
/// Only steps that are ready and not merged (`target_path.is_some() && !merged`)
/// are queried: a step that still depends on a copy or a merge has no local
/// file for the analytic calculation to examine, and the preview never
/// materializes anything to find out - it only reads. Tensor Parallelism stays
/// out of this first version: the real split is `split-mode tensor`, which the
/// per-range calculation does not represent (the same reason that turns
/// `fit_memory` off in TP, see `effective_params_for_kind`).
///
/// A machine that serves two non-adjacent ranges sums the two steps over the
/// same GPU inventory - they are two processes on the same physical card.
pub(super) async fn enrich_capacity_previews(
    state: &GatewayState,
    machines: &[Machine],
    plan: &mut Plan,
) {
    if plan.kind == DeployKind::TensorParallel || plan.steps.is_empty() {
        return;
    }
    let peer_count = plan.steps.len() as u32;
    let peers = state.all_peers().await;
    let calls = plan
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let peer = machines
                .iter()
                .any(|m| m.node_id == step.target_node_id)
                .then(|| {
                    peers
                        .iter()
                        .find(|peer| peer.node_id == step.target_node_id)
                        .cloned()
                })
                .flatten();
            let model_path = step.target_path.clone();
            let eligible = !step.merged;
            let total_layers = step.total_layers.or(plan.total_layers);
            let req = CapacityPreviewRequest {
                model_path: String::new(),
                start_layer: step.start_layer,
                end_layer: step.end_layer,
                total_layers: total_layers.unwrap_or(0),
                peer_count,
                peer_index: index as u32,
                single_peer: peer_count <= 1,
                params: plan.params.clone(),
                n_gpu_layers: step.n_gpu_layers,
                offload_kqv: step.offload_kqv,
            };
            (index, peer, model_path, eligible, total_layers, req)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(
            |(index, peer, model_path, eligible, total_layers, req)| async move {
                let (Some(peer), Some(model_path), Some(_)) =
                    (peer, model_path, total_layers.filter(|_| eligible))
                else {
                    return (index, None);
                };
                let req = CapacityPreviewRequest { model_path, ..req };
                let resp = state
                    .control_post_json(&peer, "/api/ui/deploy/capacity-preview", &req, PEER_TIMEOUT)
                    .await
                    .ok();
                let Some(resp) = resp.filter(|r| r.status().is_success()) else {
                    return (index, None);
                };
                (index, resp.json::<CapacityPreview>().await.ok())
            },
        );

    let previews = futures::stream::iter(calls)
        .buffer_unordered(crate::webui::CAPACITY_PREVIEW_MAX_PEER_REQUESTS)
        .collect::<Vec<_>>()
        .await;
    for (index, preview) in previews {
        if let Some(step) = plan.steps.get_mut(index) {
            step.capacity = preview;
        }
    }

    for machine in &mut plan.machines {
        let mut by_device: std::collections::BTreeMap<u32, crate::webui::GpuCapacityProjection> =
            std::collections::BTreeMap::new();
        let mut notes = Vec::new();
        let mut checked = 0u32;
        let mut assigned = 0u32;
        let mut fits = true;
        let mut degraded = false;
        for step in plan
            .steps
            .iter()
            .filter(|s| s.target_node_id == machine.node_id)
        {
            assigned += 1;
            let Some(capacity) = &step.capacity else {
                continue;
            };
            checked += 1;
            fits &= capacity.fits;
            degraded |= capacity.degraded;
            if let Some(note) = &capacity.note {
                notes.push(note.clone());
            }
            for gpu in &capacity.gpus {
                by_device
                    .entry(gpu.index)
                    .and_modify(|acc| {
                        acc.vram_projected_mb = acc
                            .vram_projected_mb
                            .saturating_add(gpu.vram_projected_mb.saturating_sub(gpu.vram_used_mb));
                    })
                    .or_insert(*gpu);
            }
        }
        if checked == 0 {
            continue;
        }
        if checked < assigned {
            notes.push(format!(
                "{} of {} ranges of this machine have no projection yet (waiting for the copy)",
                assigned - checked,
                assigned
            ));
        }
        machine.capacity = Some(MachineCapacity {
            fits,
            degraded,
            note: (!notes.is_empty()).then(|| notes.join("; ")),
            gpus: by_device.into_values().collect(),
        });
    }
}
