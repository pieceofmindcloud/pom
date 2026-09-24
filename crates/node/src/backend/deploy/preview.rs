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
    if let Some(reason) = survey.metadata_pending {
        p.blocked = Some(reason);
    }
    // Um plano já recusado não ganha nada de saber se caberia na VRAM - e
    // poupa uma rodada de rede por máquina a cada tecla que não muda o
    // resultado.
    if p.blocked.is_none() {
        enrich_capacity_previews(&state, &machines, &mut p).await;
    }
    Json(p).into_response()
}

// ── Pré-visualização de capacidade ───────────────────────────────────────────

/// Depois que o planejador (síncrono) monta os passos, pergunta a cada alvo já
/// materializado localmente como aquele passo ocuparia sua VRAM com os
/// parâmetros deste plano - e soma o resultado por máquina.
///
/// Só passos prontos e não fundidos (`target_path.is_some() && !merged`) são
/// consultados: um passo que ainda depende de cópia ou de junção não tem
/// arquivo local para a conta analítica examinar, e a pré-visualização nunca
/// materializa nada para descobrir - ela só lê. Tensor Parallelism fica de
/// fora desta primeira versão: a divisão real é `split-mode tensor`, que a
/// conta por faixa não representa (o mesmo motivo que desliga `fit_memory` em
/// TP, ver `effective_params_for_kind`).
///
/// Uma máquina que serve duas faixas não-adjacentes soma os dois passos sobre
/// o mesmo inventário de GPUs - são dois processos na mesma placa física.
pub(super) async fn enrich_capacity_previews(
    state: &GatewayState,
    machines: &[Machine],
    plan: &mut Plan,
) {
    if plan.kind == DeployKind::TensorParallel || plan.steps.is_empty() {
        return;
    }
    let peer_count = plan.steps.len() as u32;
    let calls = plan
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let http = state.peer_http.for_peer(&step.target_node_id);
            let base = machines
                .iter()
                .find(|m| m.node_id == step.target_node_id)
                .and_then(|m| m.gateway_url.clone());
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
            (index, http, base, model_path, eligible, total_layers, req)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(
            |(index, http, base, model_path, eligible, total_layers, req)| async move {
                let (Some(base), Ok(http), Some(model_path), Some(_)) =
                    (base, http, model_path, total_layers.filter(|_| eligible))
                else {
                    return (index, None);
                };
                let req = CapacityPreviewRequest { model_path, ..req };
                let Ok(request) = state
                    .control_json_request(
                        http.post(format!("{base}/api/ui/deploy/capacity-preview"))
                            .timeout(PEER_TIMEOUT),
                        "/api/ui/deploy/capacity-preview",
                        &req,
                    )
                    .await
                else {
                    return (index, None);
                };
                let resp = request.send().await.ok();
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
                "{} de {} faixas desta máquina ainda não têm projeção (aguardando cópia)",
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
