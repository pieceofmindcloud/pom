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
    /// Parâmetros usados para cobrar o KV junto com o peso. Ausentes usam os
    /// defaults da tela de runtime e os tipos globais da instalação.
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
            // Resolvido antes de mover `req`: `target` empresta dele.
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
            // Sem prazo próprio antes: vale o do cliente de peer (600 s).
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
// Pré-visualização de VRAM/RAM para uma faixa que este peer já serve *neste
// instante* - nunca fatia, nunca funde, nunca escreve. `crate::deploy` chama
// esta rota uma vez por passo (`Step`) do plano em preparo, com o caminho
// local que a própria varredura já leu do catálogo do peer
// (`HeldShard.path`), então não há resolução por hash aqui: um arquivo que
// não existe no caminho informado é recusado, não procurado.
//
// A prévia é uma estimativa analítica: `plan::range_cost` cobra peso e KV pela
// mesma aritmética do planejador de fatiamento (`gguf::read_layout` +
// `kv_budget::kv_bytes_for_layers`) e a VRAM livre atual distribui o resultado
// entre as GPUs. O fit nativo pertence somente ao carregamento real, porque o
// llama.cpp não oferece uma fronteira de concorrência para sondagens externas.

/// Pedido de uma pré-visualização, para exatamente um passo do plano.
///
/// Um peer que serve duas faixas não-adjacentes carrega dois arquivos em dois
/// processos - a pré-visualização segue essa mesma granularidade, uma
/// chamada por `Step`; `crate::deploy` soma os resultados por máquina.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityPreviewRequest {
    /// Caminho local já resolvido pelo coordenador (`HeldShard.path` do
    /// próprio catálogo deste peer). Nunca é uma faixa virtual: o coordenador
    /// só chama esta rota para um passo `ready`.
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
}

pub(super) fn default_preview_peer_count() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GpuCapacityProjection {
    pub index: u32,
    pub vram_total_mb: u64,
    /// Uso atual medido agora, antes desta implantação.
    pub vram_used_mb: u64,
    /// Uso projetado: `vram_used_mb` mais a fração deste passo que cairia
    /// neste dispositivo.
    pub vram_projected_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapacityPreview {
    /// `vram_projected_mb <= vram_total_mb` em todo dispositivo pela conta
    /// analítica. `true` também numa máquina sem GPU - não há VRAM para faltar.
    pub fits: bool,
    /// `fits`, mas a conta crua não cabe e o `fit_memory` habilitado pode aparar
    /// o pedido porque as camadas foram deixadas no padrão. Amber na tela;
    /// heurística, não resultado do llama.cpp.
    #[serde(default)]
    pub degraded: bool,
    /// Frase pronta para o operador, quando há algo a dizer: ajuste possível,
    /// divisão estimada ou KV na RAM.
    pub note: Option<String>,
    /// Valor pedido pelo operador. O carregamento real pode reduzi-lo.
    pub effective_n_gpu_layers: Option<u32>,
    /// Janela pedida pelo operador. O carregamento real pode reduzi-la.
    pub effective_n_ctx: Option<u32>,
    pub gpus: Vec<GpuCapacityProjection>,
}

/// Distribui `projected_bytes` entre as GPUs da máquina.
///
/// A divisão é proporcional à VRAM livre de cada dispositivo agora. Ela é uma
/// estimativa deliberada: só o carregamento real conhece a decisão final do
/// llama.cpp.
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
        return bad_request(format!(
            "file not found locally: {}",
            req.model_path
        ));
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

    // Este é o pedido, não uma decisão do motor nativo. O carregamento
    // real continua sendo a autoridade sobre camadas efetivas, contexto e
    // divisão entre GPUs.
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

    // Camadas locais ao arquivo desta fatia - `blk.0..`, não a numeração
    // global do modelo (ver `split_model.py`).
    let local_layer_count = layout.layer_count();
    let gpu_layers_local = effective_n_gpu_layers.max(0).min(local_layer_count as i32) as u32;
    let is_last = req.end_layer + 1 >= req.total_layers;
    let (weight_bytes, kv_bytes) = if gpu_layers_local == 0 {
        (0, 0)
    } else {
        // `cache_type_k`/`cache_type_v` vêm da config da instalação (como no
        // launch real), então a quantização do cache já entra na conta: o
        // `kv_budget::row_bytes` tem o tamanho de bloco de cada tipo.
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
            0,
            gpu_layers_local - 1,
            is_last,
        ) {
            Ok((weight_bytes, kv_bytes, _total)) => (weight_bytes, kv_bytes),
            Err(e) => {
                return bad_request(format!(
                    "could not compute the memory projection: {e}"
                ))
            }
        }
    };
    // O cache KV só pesa na VRAM quando fica na GPU (`offload_kqv`). Sem isso
    // ele mora na RAM do host, mesmo para as camadas que estão na GPU, e não
    // deve inflar a projeção de VRAM.
    let kv_in_vram = cfg.offload_kqv;
    let projected_bytes = weight_bytes.saturating_add(if kv_in_vram { kv_bytes } else { 0 });

    let system = sysmon::sample().await;
    let (gpus, raw_fits, split_note) = split_projection(projected_bytes, &system.gpus);
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
    })
    .into_response()
}
