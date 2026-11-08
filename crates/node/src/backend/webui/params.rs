use super::*;
// ── Planning ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct PlanRequest {
    /// Path of the source model, as seen on the node named by `layout_node_id`.
    pub model_path: String,
    pub budgets: Vec<PeerBudget>,
    #[serde(default)]
    pub reserve_pct: Option<f64>,
    /// Node whose copy of the GGUF should be measured. The dashboard can run on
    /// a peer that does not have the model — the MacBook's LM Studio library is
    /// nearly empty while the ksia's is not — so the layout is read wherever the
    /// file actually is.
    #[serde(default)]
    pub layout_node_id: Option<String>,
    /// Parameters used to charge the KV together with the weight. Absent ones
    /// use the runtime screen's defaults and the installation's global types.
    #[serde(default)]
    pub n_ctx: Option<u32>,
    #[serde(default)]
    pub max_sessions: Option<u32>,
    #[serde(default)]
    pub cache_type_k: Option<KvCacheType>,
    #[serde(default)]
    pub cache_type_v: Option<KvCacheType>,
}

/// Work out asymmetric layer ranges from each peer's declared memory budget.
pub(super) async fn plan_split_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<PlanRequest>,
) -> Response {
    // Delegate to the peer that holds the file when it is not this one.
    if let Some(ref target) = req.layout_node_id {
        if *target != state.network.node_id {
            // Resolved before moving `req`: `target` borrows from it.
            let peer = state
                .all_peers()
                .await
                .into_iter()
                .find(|p| p.node_id == *target);
            let Some(peer) = peer else {
                return bad_request(format!("peer {target} is not reachable to read the GGUF"));
            };
            let mut forwarded = req;
            // Clear the hop marker so the receiving peer measures locally.
            forwarded.layout_node_id = None;
            // No deadline of its own before: the peer client's one applies (600 s).
            return match state
                .control_post_json(
                    &peer,
                    "/api/ui/plan",
                    &forwarded,
                    std::time::Duration::from_secs(600),
                )
                .await
            {
                Ok(r) => {
                    let status = r.status();
                    match r.json::<serde_json::Value>().await {
                        Ok(v) => (status, Json(v)).into_response(),
                        Err(e) => bad_request(format!("invalid peer response: {e}")),
                    }
                }
                Err(e) => bad_request(format!("peer did not answer: {e:#}")),
            };
        }
    }

    let path = std::path::PathBuf::from(&req.model_path);
    let layout_path = path.clone();
    let layout = match tokio::task::spawn_blocking(move || gguf::read_layout(&layout_path)).await {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => return bad_request(format!("could not read the GGUF: {e}")),
        Err(e) => return bad_request(format!("GGUF read failed: {e}")),
    };

    let info = match tokio::task::spawn_blocking(move || gguf::read_info(&path)).await {
        Ok(Ok(info)) => info,
        Ok(Err(e)) => {
            return bad_request(format!("could not read the KV geometry of the GGUF: {e}"))
        }
        Err(e) => return bad_request(format!("KV geometry read failed: {e}")),
    };

    let reserve = req.reserve_pct.unwrap_or(DEFAULT_RESERVE_PCT);
    let defaults = RuntimeParams::default();
    let installation = state.auth.read().await;
    let kv = plan::KvPlanParams {
        n_ctx: req.n_ctx.unwrap_or(defaults.n_ctx),
        max_sessions: req.max_sessions.unwrap_or(defaults.max_sessions),
        cache_type_k: req.cache_type_k.unwrap_or(installation.cache_type_k),
        cache_type_v: req.cache_type_v.unwrap_or(installation.cache_type_v),
    };
    match plan::plan_split_with_kv(&req.model_path, &layout, &info, &req.budgets, reserve, kv) {
        Ok(p) => Json(p).into_response(),
        Err(e) => bad_request(e.to_string()),
    }
}

// ── Deploy Studio: capacity preview ──────────────────────────────────────────
//
// VRAM/RAM preview for a range this peer already serves *right now* - it
// never slices, never merges, never writes. `crate::deploy` calls this route
// once per step (`Step`) of the plan being prepared, with the local path
// that the survey itself already read from the peer's catalog
// (`HeldShard.path`), so there is no resolution by hash here: a file that
// does not exist at the given path is refused, not searched for.
//
// The preview is an analytical estimate: `plan::range_cost` charges weight and
// KV with the same arithmetic as the slicing planner (`gguf::read_layout` +
// `kv_budget::kv_bytes_for_layers`) and the current free VRAM distributes the
// result among the GPUs. The native fit belongs only to the real load, because
// llama.cpp does not offer a concurrency boundary for external probes.

/// Request for a preview, for exactly one step of the plan.
///
/// A peer that serves two non-adjacent ranges loads two files in two
/// processes - the preview follows that same granularity, one call per
/// `Step`; `crate::deploy` sums the results per machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityPreviewRequest {
    /// Local path already resolved by the coordinator (`HeldShard.path` from
    /// this peer's own catalog). It is never a virtual range: the coordinator
    /// only calls this route for a `ready` step.
    pub model_path: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    #[serde(default = "default_preview_peer_count")]
    pub peer_count: u32,
    #[serde(default)]
    pub peer_index: u32,
    #[serde(default)]
    pub single_peer: bool,
    pub params: RuntimeParams,
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    #[serde(default)]
    pub offload_kqv: Option<bool>,
    /// `model_path` is the whole model and the step serves only
    /// `start_layer..=end_layer` of it (a range the machine points at in its
    /// own copy). Absent on older coordinators: the path is the range's file.
    #[serde(default)]
    pub range_of_whole: bool,
}

pub(super) fn default_preview_peer_count() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GpuCapacityProjection {
    pub index: u32,
    pub vram_total_mb: u64,
    /// Current usage measured now, before this deployment.
    pub vram_used_mb: u64,
    /// Projected usage: `vram_used_mb` plus the fraction of this step that
    /// would land on this device.
    pub vram_projected_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityPreview {
    /// `vram_projected_mb <= vram_total_mb` on every device by the analytical
    /// arithmetic. `true` also on a machine without a GPU - there is no VRAM to
    /// run short of.
    pub fits: bool,
    /// `fits`, but the raw arithmetic does not fit and the enabled `fit_memory`
    /// may trim the request because the layers were left at the default. Amber
    /// on the screen; a heuristic, not a llama.cpp result.
    #[serde(default)]
    pub degraded: bool,
    /// Ready-made sentence for the operator, when there is something to say:
    /// possible fit, estimated split or KV in RAM.
    pub note: Option<String>,
    /// Value requested by the operator. The real load may reduce it.
    pub effective_n_gpu_layers: Option<u32>,
    /// Window requested by the operator. The real load may reduce it.
    pub effective_n_ctx: Option<u32>,
    pub gpus: Vec<GpuCapacityProjection>,
    /// How many of the requested GPU layers fit in the free VRAM now, when
    /// fewer than requested: the rest would run on the CPU. `None` when they
    /// all fit, or on a machine with no dedicated VRAM (unified memory).
    #[serde(default)]
    pub gpu_layers_fit: Option<u32>,
    /// Layers of this step that would run on the CPU from system RAM.
    #[serde(default)]
    pub cpu_layers: Option<u32>,
    /// RAM the CPU layers' weights need.
    #[serde(default)]
    pub cpu_ram_bytes: Option<u64>,
}

/// The most of the first `requested` layers whose VRAM cost (`in_vram`,
/// cumulative from the range's first layer) stays within `free_vram`. A layer
/// count whose cost cannot be computed counts as not fitting.
pub(super) fn gpu_layers_that_fit(
    requested: u32,
    free_vram: u64,
    in_vram: impl Fn(u32) -> Option<u64>,
) -> u32 {
    let mut fit = requested;
    while fit > 0 && in_vram(fit).is_none_or(|bytes| bytes > free_vram) {
        fit -= 1;
    }
    fit
}

/// Distributes `projected_bytes` among the machine's GPUs.
///
/// The split is proportional to each device's free VRAM right now. It is a
/// deliberate estimate: only the real load knows llama.cpp's final
/// decision.
pub(super) fn split_projection(
    projected_bytes: u64,
    gpus: &[sysmon::GpuStat],
) -> (Vec<GpuCapacityProjection>, bool, Option<String>) {
    let devices: Vec<(u32, u64, u64)> = gpus
        .iter()
        .filter_map(|g| {
            let total = g.vram_total_mb?.max(0.0) as u64;
            let used = g.vram_used_mb.unwrap_or(0.0).max(0.0) as u64;
            Some((g.index, total, used))
        })
        .collect();
    if devices.is_empty() {
        return (Vec::new(), true, None);
    }
    let free: Vec<f64> = devices
        .iter()
        .map(|(_, total, used)| total.saturating_sub(*used) as f64)
        .collect();
    let sum: f64 = free.iter().sum();
    let ratios = if sum > 0.0 {
        free.iter().map(|v| v / sum).collect()
    } else {
        vec![1.0 / devices.len() as f64; devices.len()]
    };
    let mib = 1024u64 * 1024;
    let mut fits = true;
    let projections = devices
        .iter()
        .zip(ratios.iter())
        .map(|((index, total, used), ratio)| {
            let added_mb = ((projected_bytes as f64 * ratio) / mib as f64).ceil() as u64;
            let projected = used.saturating_add(added_mb);
            if projected > *total {
                fits = false;
            }
            GpuCapacityProjection {
                index: *index,
                vram_total_mb: *total,
                vram_used_mb: *used,
                vram_projected_mb: projected,
            }
        })
        .collect();
    let note = (devices.len() > 1).then(|| {
        "split between GPUs estimated from the current free VRAM; loading confirms the final layout"
            .to_string()
    });
    (projections, fits, note)
}

pub(super) fn capacity_verdict(
    raw_fits: bool,
    fit_memory: bool,
    requested_gpu_layers: u32,
    local_layer_count: u32,
) -> (bool, bool) {
    if raw_fits {
        return (true, false);
    }
    let auto_fit_would_adjust = fit_memory && requested_gpu_layers >= local_layer_count;
    if auto_fit_would_adjust {
        (true, true)
    } else {
        (false, false)
    }
}

pub(super) async fn capacity_preview_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<CapacityPreviewRequest>,
) -> Response {
    let path = std::path::PathBuf::from(&req.model_path);
    if !path.is_file() {
        return bad_request(format!("file not found locally: {}", req.model_path));
    }
    let Some(_preview_slot) = try_capacity_preview_slot() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many capacity previews in progress",
        )
            .into_response();
    };

    let mut cfg = NodeConfig {
        deploy_id: String::new(),
        model_path: req.model_path.clone(),
        model_label: String::new(),
        deployment_name: String::new(),
        start_layer: req.start_layer,
        end_layer: req.end_layer,
        total_layers: req.total_layers,
        peer_count: req.peer_count.max(1),
        peer_index: req.peer_index,
        primary: req.start_layer == 0,
        tp: None,
        inference_mode: req.params.inference_mode,
        inference_port: 0,
        single_peer: req.single_peer,
        no_embeddings: req.params.no_embeddings,
        ..NodeConfig::from_params(&req.params, true)
    };
    {
        let installation = state.auth.read().await;
        cfg.cache_type_k = installation.cache_type_k;
        cfg.cache_type_v = installation.cache_type_v;
    }
    if let Some(n) = req.n_gpu_layers {
        cfg.n_gpu_layers = n;
    }
    if let Some(offload) = req.offload_kqv {
        cfg.offload_kqv = offload;
    }

    let need_embeddings = cfg.needs_residual_export();
    let engine_cfg =
        match crate::runtime::engine_config_from_node(&cfg, need_embeddings, false, Vec::new()) {
            Ok(engine_cfg) => engine_cfg,
            Err(e) => return bad_request(format!("invalid configuration: {e}")),
        };

    // This is the request, not a decision of the native engine. The real
    // load remains the authority on effective layers, context and the
    // split among GPUs.
    let effective_n_gpu_layers = engine_cfg.n_gpu_layers;
    let effective_n_ctx = engine_cfg.n_ctx;

    let layout_path = path.clone();
    let layout = match tokio::task::spawn_blocking(move || gguf::read_layout(&layout_path)).await {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => return bad_request(format!("could not read the GGUF: {e}")),
        Err(e) => return bad_request(format!("GGUF read failed: {e}")),
    };
    let info_path = path.clone();
    let info = match tokio::task::spawn_blocking(move || gguf::read_info(&info_path)).await {
        Ok(Ok(info)) => info,
        Ok(Err(e)) => {
            return bad_request(format!("could not read the KV geometry of the GGUF: {e}"))
        }
        Err(e) => return bad_request(format!("KV geometry read failed: {e}")),
    };

    // Layers local to this slice's file - `blk.0..`, not the model's global
    // numbering (see `split_model.py`).
    // A range of the whole model counts its own layers, at their global
    // indices in the whole file's layout.
    let (first_layer, local_layer_count) = if req.range_of_whole {
        if req.end_layer < req.start_layer || req.end_layer >= layout.layer_count() {
            return bad_request(format!(
                "the range L{}-{} is outside the {} layers of the GGUF",
                req.start_layer,
                req.end_layer,
                layout.layer_count()
            ));
        }
        (req.start_layer, req.end_layer - req.start_layer + 1)
    } else {
        (0, layout.layer_count())
    };
    let gpu_layers_local = effective_n_gpu_layers.max(0).min(local_layer_count as i32) as u32;
    let is_last = req.end_layer + 1 >= req.total_layers;
    let (weight_bytes, kv_bytes) = if gpu_layers_local == 0 {
        (0, 0)
    } else {
        // `cache_type_k`/`cache_type_v` come from the installation's config (as
        // in the real launch), so the cache quantization is already in the
        // arithmetic: `kv_budget::row_bytes` has each type's block size.
        let kv = plan::KvPlanParams {
            n_ctx: effective_n_ctx,
            max_sessions: req.params.max_sessions.max(1),
            cache_type_k: cfg.cache_type_k,
            cache_type_v: cfg.cache_type_v,
        };
        match plan::range_cost(
            &layout,
            Some(&info),
            Some(kv),
            first_layer,
            first_layer + gpu_layers_local - 1,
            is_last,
        ) {
            Ok((weight_bytes, kv_bytes, _total)) => (weight_bytes, kv_bytes),
            Err(e) => return bad_request(format!("could not compute the memory projection: {e}")),
        }
    };
    // The KV cache only weighs on VRAM when it stays on the GPU (`offload_kqv`).
    // Without that it lives in host RAM, even for the layers that are on the
    // GPU, and must not inflate the VRAM projection.
    let kv_in_vram = cfg.offload_kqv;
    let projected_bytes = weight_bytes.saturating_add(if kv_in_vram { kv_bytes } else { 0 });

    let system = sysmon::sample().await;
    let (gpus, raw_fits, split_note) = split_projection(projected_bytes, &system.gpus);

    // Overcommit: how many of the requested GPU layers fit in the free VRAM
    // (minus the fit margin per device). The rest would run on the CPU.
    let margin = u64::from(engine_cfg.fit_memory_margin_mb) * 1024 * 1024;
    let dedicated: Vec<(u64, u64)> = system
        .gpus
        .iter()
        .filter_map(|g| {
            let total = (g.vram_total_mb?.max(0.0) * 1024.0 * 1024.0) as u64;
            let used = (g.vram_used_mb.unwrap_or(0.0).max(0.0) * 1024.0 * 1024.0) as u64;
            Some((total, used))
        })
        .collect();
    let free_vram: u64 = dedicated
        .iter()
        .map(|(total, used)| total.saturating_sub(*used).saturating_sub(margin))
        .sum();
    let kv_plan = plan::KvPlanParams {
        n_ctx: effective_n_ctx,
        max_sessions: req.params.max_sessions.max(1),
        cache_type_k: cfg.cache_type_k,
        cache_type_v: cfg.cache_type_v,
    };
    let cost_of = |gpu_layers: u32| -> Option<(u64, u64)> {
        if gpu_layers == 0 {
            return Some((0, 0));
        }
        plan::range_cost(
            &layout,
            Some(&info),
            Some(kv_plan),
            first_layer,
            first_layer + gpu_layers - 1,
            is_last,
        )
        .ok()
        .map(|(weights, kv, _)| (weights, kv))
    };
    let (gpu_layers_fit, cpu_layers, cpu_ram_bytes) = if dedicated.is_empty() || gpu_layers_local == 0 {
        (None, None, None)
    } else {
        let in_vram = |gpu_layers: u32| {
            cost_of(gpu_layers).map(|(weights, kv)| weights + if kv_in_vram { kv } else { 0 })
        };
        let fit = gpu_layers_that_fit(gpu_layers_local, free_vram, in_vram);
        if fit >= gpu_layers_local {
            (None, None, None)
        } else {
            let on_cpu = local_layer_count - fit;
            let all = cost_of(local_layer_count).map(|(weights, _)| weights).unwrap_or(0);
            let on_gpu = cost_of(fit).map(|(weights, _)| weights).unwrap_or(0);
            (Some(fit), Some(on_cpu), Some(all.saturating_sub(on_gpu)))
        }
    };
    let (fits, degraded) = capacity_verdict(
        raw_fits,
        cfg.fit_memory,
        cfg.n_gpu_layers,
        local_layer_count,
    );
    let mut notes = vec![
        "analytic preview; the real load confirms the layers, the context and the split between GPUs"
            .to_string(),
    ];
    if degraded {
        notes.push(
            "the raw estimate does not fit; with fit_memory and default layers, the real load may trim the request"
                .to_string(),
        );
    }
    if let Some(note) = split_note {
        notes.push(note);
    }
    if let (Some(fit), Some(on_cpu)) = (gpu_layers_fit, cpu_layers) {
        notes.push(format!(
            "{fit} layers fit on the GPU, {on_cpu} on the CPU: generation will be much slower"
        ));
    }
    if !kv_in_vram && kv_bytes > 0 {
        notes.push(format!(
            "KV cache in RAM (offload off): ~{} MiB outside VRAM",
            kv_bytes / (1024 * 1024)
        ));
    }

    Json(CapacityPreview {
        fits,
        degraded,
        note: (!notes.is_empty()).then(|| notes.join("; ")),
        effective_n_gpu_layers: Some(effective_n_gpu_layers.max(0) as u32),
        effective_n_ctx: Some(effective_n_ctx),
        gpus,
        gpu_layers_fit,
        cpu_layers,
        cpu_ram_bytes,
    })
    .into_response()
}
