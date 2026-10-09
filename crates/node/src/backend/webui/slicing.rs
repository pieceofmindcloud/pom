use super::*;
// ── Slicing a whole model, here, on demand ───────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SliceMode {
    Pipeline,
    TensorParallel,
}

impl Default for SliceMode {
    fn default() -> Self {
        Self::Pipeline
    }
}

#[derive(Debug, Serialize)]
pub struct TpSliceRank {
    pub rank: u32,
    pub start_layer: u32,
    pub end_layer: u32,
}

#[derive(Debug, Serialize)]
pub struct TpSlicePlan {
    pub mode: SliceMode,
    pub model_bytes: u64,
    pub total_layers: u32,
    pub tp_size: u32,
    pub ranks: Vec<TpSliceRank>,
    pub complete: bool,
    pub blocked: bool,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_free_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_margin_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space_status: Option<&'static str>,
}

/// Validate a TP cut once and give the UI a useful preview without writing
/// any shard. Every rank contains the complete layer range; the tensor
/// columns and rows, not the layers, are what differ between ranks.
pub(super) fn tensor_slice_plan(
    path: &std::path::Path,
    tp_size: u32,
) -> anyhow::Result<TpSlicePlan> {
    if !(2..=64).contains(&tp_size) {
        anyhow::bail!("escolha entre 2 e 64 ranks TP");
    }
    crate::splitter::validate_tensor_parallel(path, tp_size)?;
    let layout = crate::gguf::read_layout(path)?;
    let output_bytes = crate::splitter::estimate_tensor_parallel_output_bytes(path, tp_size)?;
    let total_layers = layout.layer_count();
    if total_layers == 0 {
        anyhow::bail!("este arquivo não declara camadas");
    }
    Ok(TpSlicePlan {
        mode: SliceMode::TensorParallel,
        model_bytes: output_bytes,
        total_layers,
        tp_size,
        ranks: (0..tp_size)
            .map(|rank| TpSliceRank {
                rank,
                start_layer: 0,
                end_layer: total_layers - 1,
            })
            .collect(),
        complete: true,
        blocked: false,
        warnings: Vec::new(),
        destination_path: None,
        destination_free_bytes: None,
        space_margin_bytes: None,
        space_status: None,
    })
}

pub(super) fn tensor_slice_manifest(
    base_name: String,
    label: String,
    total_layers: u32,
    node_id: String,
    source_sha256: String,
    quant: Option<String>,
    tp_parts: Vec<crate::pom::PomPart>,
) -> crate::pom::PomManifest {
    let mut manifest = crate::pom::PomManifest::new(
        crate::pom::PomModel {
            name: base_name,
            label,
            total_layers,
            quant,
        },
        crate::pom::PomKind::TensorParallel,
        node_id,
        source_sha256,
        Vec::new(),
    );
    manifest.tp_parts = tp_parts;
    manifest
}

#[derive(Debug, Deserialize)]
pub struct SliceRequest {
    /// Whole model on *this* machine.
    pub model_path: String,
    /// Target size of each slice, in order. A slice with no size takes what is
    /// left of the model. Tensor Parallelism does not use this list.
    #[serde(default)]
    pub slices: Vec<plan::SizedSlice>,
    /// One manually selected range. When present, `slices` is ignored.
    #[serde(default)]
    pub range: Option<SliceRange>,
    /// Catalog name of the model, used to name the slices. Without it the name
    /// comes from the file, which for an Ollama blob is a digest.
    #[serde(default)]
    pub name: Option<String>,
    /// Pipeline layers or tensor-parallel ranks.
    #[serde(default)]
    pub mode: SliceMode,
    /// Number of TP ranks when `mode` is `tensor_parallel`.
    #[serde(default)]
    pub tp_size: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct SlicePlanRequest {
    pub model_path: String,
    #[serde(default)]
    pub slices: Vec<plan::SizedSlice>,
    /// One manually selected range. When present, `slices` is ignored.
    #[serde(default)]
    pub range: Option<SliceRange>,
    /// Shortcut: fill the list with `n` slices of equal layer count, each
    /// carrying its real size as the target.
    #[serde(default)]
    pub equal_parts: Option<u32>,
    #[serde(default)]
    pub mode: SliceMode,
    #[serde(default)]
    pub tp_size: Option<u32>,
}

/// Read the tensor table of a local model, once per file version.
///
/// The builder screen asks for a preview on every keystroke; re-reading the
/// header each time would make typing a size feel heavy.
pub(super) async fn layout_of(
    path: &std::path::Path,
) -> Result<Arc<crate::gguf::GgufLayout>, String> {
    type Key = (String, u64, u64);
    static CACHE: std::sync::OnceLock<Mutex<HashMap<Key, Arc<crate::gguf::GgufLayout>>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("modelo não encontrado nesta máquina: {e}"))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = (path.to_string_lossy().into_owned(), meta.len(), mtime);

    if let Some(hit) = cache.lock().await.get(&key).cloned() {
        return Ok(hit);
    }

    let p = path.to_path_buf();
    let layout = match tokio::task::spawn_blocking(move || gguf::read_layout(&p)).await {
        Ok(Ok(l)) => Arc::new(l),
        Ok(Err(e)) => return Err(format!("não foi possível ler o GGUF: {e}")),
        Err(e) => return Err(format!("leitura do GGUF falhou: {e}")),
    };
    cache.lock().await.insert(key, layout.clone());
    Ok(layout)
}

/// Checa, sem escrever nada, se um modelo local dá para partir por tensor
/// parallelism no caminho C (classificação dos tensores, divisibilidade das
/// dimensões, weight-tying). O coordenador do deploy chama isto no primary
/// antes de aceitar o grupo; a resposta é sempre 200, com `ok: false` + `reason`
/// quando o modelo não serve — a checagem em si é uma resposta válida, não um
/// erro HTTP.
#[derive(Debug, Deserialize)]
pub struct TpCheckRequest {
    pub model_path: String,
    pub tp_size: u32,
}

pub(super) async fn tp_check(_auth: ConnectionAuth, Json(req): Json<TpCheckRequest>) -> Response {
    let path = std::path::Path::new(&req.model_path);
    if !path.is_file() {
        return Json(json!({
            "ok": false,
            "reason": format!("modelo não encontrado nesta máquina: {}", req.model_path),
        }))
        .into_response();
    }
    match crate::splitter::validate_tensor_parallel(path, req.tp_size) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => Json(json!({ "ok": false, "reason": e.to_string() })).into_response(),
    }
}

/// What a list of target sizes turns into: layer ranges, real sizes, coverage.
pub(super) const DISK_HEADROOM_PCT: u64 = 10;
pub(super) const DISK_HEADROOM_FLOOR: u64 = 1 << 30;

pub(super) fn signed_space_margin(free_bytes: u64, required_bytes: u64) -> i64 {
    (i128::from(free_bytes) - i128::from(required_bytes))
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

pub(super) fn valid_range_error(
    layout: &crate::gguf::GgufLayout,
    range: &SliceRange,
) -> Option<String> {
    let total = layout.layer_count();
    if range.start_layer > range.end_layer {
        return Some(format!(
            "faixa L{}-{} está invertida; escolha início entre L0 e L{} e um fim igual ou maior",
            range.start_layer,
            range.end_layer,
            total.saturating_sub(1)
        ));
    }
    if range.end_layer >= total {
        return Some(format!(
            "faixa L{}-{} está fora do modelo de {} camadas; escolha entre L0 e L{}",
            range.start_layer,
            range.end_layer,
            total,
            total.saturating_sub(1)
        ));
    }
    if !layout.is_valid_slice_start(range.start_layer) {
        let starts = std::iter::once(0)
            .chain(layout.arch_boundaries().into_iter())
            .map(|layer| format!("L{layer}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!(
            "a faixa L{}-{} começa no meio de um grupo arquitetural; inícios válidos: {}",
            range.start_layer, range.end_layer, starts
        ));
    }
    let snapped = layout.snap_end_layer(range.start_layer, range.end_layer, total);
    if snapped != range.end_layer {
        return Some(format!(
            "a faixa L{}-{} termina no meio de um grupo arquitetural; para esse início, termine em L{} ou antes",
            range.start_layer, range.end_layer, snapped
        ));
    }
    None
}

pub(super) fn single_range_plan(
    layout: &crate::gguf::GgufLayout,
    range: &SliceRange,
) -> anyhow::Result<plan::SizePlan> {
    if let Some(error) = valid_range_error(layout, range) {
        anyhow::bail!(error);
    }
    let is_last = range.end_layer + 1 >= layout.layer_count();
    layout.validate_slice_ranges(&[(range.start_layer, range.end_layer)])?;
    let bytes = layout.slice_bytes(range.start_layer, range.end_layer, is_last);
    let required_head_bytes = (!is_last)
        .then_some(layout.required_head_bytes())
        .filter(|bytes| *bytes > 0);
    Ok(plan::SizePlan {
        total_layers: layout.layer_count(),
        model_bytes: bytes,
        source_model_bytes: layout.source_file_bytes.max(layout.total_bytes),
        required_head_bytes,
        ranges: vec![plan::SizedRange {
            index: 0,
            start_layer: range.start_layer,
            end_layer: range.end_layer,
            layers: range.end_layer - range.start_layer + 1,
            bytes,
            target_mb: None,
            is_rest: false,
            is_last,
            too_small: false,
            empty: false,
        }],
        covered_layers: range.end_layer - range.start_layer + 1,
        complete: range.start_layer == 0 && is_last,
        missing_range: None,
        missing_bytes: 0,
        blocked: false,
        warnings: Vec::new(),
        destination_path: None,
        destination_free_bytes: None,
        space_margin_bytes: None,
        space_status: None,
    })
}

pub(super) fn classify_disk_space(free_bytes: u64, required_bytes: u64) -> &'static str {
    if free_bytes == 0 {
        return "unknown";
    }
    if free_bytes < required_bytes {
        return "insufficient";
    }
    let margin = free_bytes - required_bytes;
    let headroom = (required_bytes / 100 * DISK_HEADROOM_PCT).max(DISK_HEADROOM_FLOOR);
    if margin < headroom {
        "tight"
    } else {
        "ok"
    }
}

pub(super) fn disk_space_warning(
    status: &str,
    destination: &std::path::Path,
    free_bytes: Option<u64>,
    required_bytes: u64,
) -> Option<String> {
    let free = free_bytes?;
    let free_gib = free as f64 / 1_073_741_824.0;
    let required_gib = required_bytes as f64 / 1_073_741_824.0;
    match status {
        "insufficient" => Some(format!(
            "destino {} tem {:.1} GiB livres, mas o plano precisa de cerca de {:.1} GiB; o fatiamento pode falhar por falta de espaço",
            destination.display(), free_gib, required_gib
        )),
        "tight" => Some(format!(
            "destino {} tem {:.1} GiB livres para cerca de {:.1} GiB de saída; a folga é apertada",
            destination.display(), free_gib, required_gib
        )),
        _ => None,
    }
}

pub(super) async fn disk_free_at(path: &std::path::Path) -> Option<u64> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let free_mb = sysmon::sample_disk_at(&path).free_mb;
        (free_mb > 0).then_some(free_mb.saturating_mul(1_048_576))
    })
    .await
    .ok()
    .flatten()
}

pub(super) async fn slice_plan(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SlicePlanRequest>,
) -> Response {
    let destination = state.shard_dir().await;
    if matches!(req.mode, SliceMode::TensorParallel) {
        let Some(tp_size) = req.tp_size else {
            return bad_request("informe o tamanho do grupo TP");
        };
        return match tensor_slice_plan(std::path::Path::new(&req.model_path), tp_size) {
            Ok(mut plan) => {
                let free = disk_free_at(&destination).await;
                let status = classify_disk_space(free.unwrap_or(0), plan.model_bytes);
                plan.destination_path = Some(destination.to_string_lossy().into_owned());
                plan.destination_free_bytes = free;
                plan.space_margin_bytes =
                    free.map(|value| signed_space_margin(value, plan.model_bytes));
                plan.space_status = Some(status);
                if let Some(warning) =
                    disk_space_warning(status, &destination, free, plan.model_bytes)
                {
                    plan.warnings.push(warning);
                }
                Json(plan).into_response()
            }
            Err(error) => bad_request(error.to_string()),
        };
    }
    let layout = match layout_of(std::path::Path::new(&req.model_path)).await {
        Ok(l) => l,
        Err(e) => return bad_request(e),
    };
    let planned = match req.range.as_ref() {
        Some(range) => single_range_plan(&layout, range),
        None => match req.equal_parts {
            Some(n) if (1..=64).contains(&n) => plan::plan_equal(&layout, n),
            Some(_) => return bad_request("escolha entre 1 e 64 fatias"),
            None => plan::plan_by_size(&layout, &req.slices),
        },
    };
    match planned {
        Ok(mut plan) => {
            let free = disk_free_at(&destination).await;
            let status = classify_disk_space(free.unwrap_or(0), plan.model_bytes);
            plan.destination_path = Some(destination.to_string_lossy().into_owned());
            plan.destination_free_bytes = free;
            plan.space_margin_bytes =
                free.map(|value| signed_space_margin(value, plan.model_bytes));
            plan.space_status = Some(status);
            if let Some(warning) = disk_space_warning(status, &destination, free, plan.model_bytes)
            {
                plan.warnings.push(warning);
            }
            Json(plan).into_response()
        }
        Err(e) => bad_request(e.to_string()),
    }
}

/// Cut a local model into `parts` slices, in the background.
///
/// Nothing about the split itself is new: it is the same `split_model.py` the
/// cluster launch already runs, one range at a time, writing into the
/// configured slice directory. What is new is that the operator can ask for it
/// without planning a whole cluster, and can watch it happen.
pub(super) async fn slice_locally(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SliceRequest>,
) -> Response {
    let source = std::path::PathBuf::from(&req.model_path);
    if !source.is_file() {
        return bad_request(format!(
            "modelo não encontrado nesta máquina: {}",
            req.model_path
        ));
    }
    if matches!(req.mode, SliceMode::Pipeline) && req.range.is_none() && req.slices.is_empty() {
        return bad_request("nenhuma fatia na lista");
    }
    if matches!(req.mode, SliceMode::Pipeline) && req.range.is_none() && req.slices.len() > 64 {
        return bad_request("no máximo 64 fatias");
    }

    let layout = match layout_of(&source).await {
        Ok(l) => l,
        Err(e) => return bad_request(e),
    };

    if matches!(req.mode, SliceMode::TensorParallel) {
        if req.range.is_some() {
            return bad_request("uma faixa específica só pode ser fatiada no modo pipeline");
        }
        let Some(tp_size) = req.tp_size else {
            return bad_request("informe o tamanho do grupo TP");
        };
        if let Err(error) = tensor_slice_plan(&source, tp_size) {
            return bad_request(error.to_string());
        }
        let tp_output_bytes =
            match crate::splitter::estimate_tensor_parallel_output_bytes(&source, tp_size) {
                Ok(bytes) => bytes,
                Err(error) => return bad_request(error.to_string()),
            };
        let destination = state.shard_dir().await;
        let destination_free = disk_free_at(&destination).await;
        let destination_status =
            classify_disk_space(destination_free.unwrap_or(0), tp_output_bytes);
        let destination_warning = disk_space_warning(
            destination_status,
            &destination,
            destination_free,
            tp_output_bytes,
        );
        let total_layers = layout.layer_count();
        let base_name = req
            .name
            .as_deref()
            .and_then(plan::base_name_from_label)
            .unwrap_or_else(|| plan::base_name_of(&req.model_path));
        let shard_dir = state.shard_dir().await;
        let scratch = crate::pom::scratch_dir(&shard_dir);
        let id = uuid::Uuid::new_v4().simple().to_string();
        let label = req.name.clone().unwrap_or_else(|| base_name.clone());
        jobs().write().await.insert(
            id.clone(),
            new_job(
                &id,
                "slice",
                format!("Fatiando {base_name} em {tp_size} shards TP"),
                "preparando",
            ),
        );
        let job_id = id.clone();
        let node_id = state.network.node_id.clone();
        let root = shard_dir.clone();
        tokio::spawn(async move {
            let result = async {
                if let Some(warning) = destination_warning {
                    set_job_phase(
                        &job_id,
                        "verificando espaço",
                        &warning,
                        Some(destination.to_string_lossy().into_owned()),
                    )
                    .await;
                }
                let source_sha256 = source_model_sha256_for_job(&source, &job_id).await?;
                run_tp_slice(
                    state,
                    source,
                    shard_dir,
                    scratch,
                    base_name,
                    label,
                    total_layers,
                    node_id,
                    source_sha256,
                    tp_size,
                    &job_id,
                )
                .await
            }
            .await;
            match result {
                Ok(manifest) => {
                    let msg = format!(
                        "{} shards TP em {} · pom {}",
                        manifest.tp_parts.len(),
                        crate::pom::dir_for(&root, &manifest).display(),
                        crate::pom::short_id(&manifest.pom_id)
                    );
                    set_job(&job_id, "done", "pronto", &msg).await;
                }
                Err(error) => {
                    warn!("fatiamento TP falhou: {error:#}");
                    set_job(&job_id, "failed", "erro", &format!("{error:#}")).await;
                }
            }
        });
        return Json(json!({ "job_id": id })).into_response();
    }

    // The screen previews with the same function, but the decision is remade
    // here: a stale preview must not become a slicing run nobody approved.
    let sized = match match req.range.as_ref() {
        Some(range) => single_range_plan(&layout, range),
        None => plan::plan_by_size(&layout, &req.slices),
    } {
        Ok(p) => p,
        Err(e) => return bad_request(e.to_string()),
    };
    if sized.blocked {
        return bad_request(
            sized
                .warnings
                .first()
                .cloned()
                .unwrap_or_else(|| "há uma fatia impossível na lista".to_string()),
        );
    }

    // Repeat the preview's check at the point of action: another process may
    // have consumed the destination between the last keystroke and the click.
    // This is an alert-only guard by design; the operator can still choose to
    // proceed and the resulting failure will be recorded on the job.
    let destination = state.shard_dir().await;
    let destination_free = disk_free_at(&destination).await;
    let destination_status = classify_disk_space(destination_free.unwrap_or(0), sized.model_bytes);
    let destination_warning = disk_space_warning(
        destination_status,
        &destination,
        destination_free,
        sized.model_bytes,
    );

    let total_layers = sized.total_layers;
    let ranges: Vec<(u32, u32)> = sized
        .ranges
        .iter()
        .filter(|r| !r.empty)
        .map(|r| (r.start_layer, r.end_layer))
        .collect();
    if ranges.is_empty() {
        return bad_request("este arquivo não declara camadas — não há o que fatiar");
    }
    let base_name = req
        .name
        .as_deref()
        .and_then(plan::base_name_from_label)
        .unwrap_or_else(|| plan::base_name_of(&req.model_path));
    // A pasta definitiva leva o início do hash do modelo original. Corta-se
    // aqui e a rodada se muda para o nome definitivo no fim, em
    // `pom::finalize_run`.
    let shard_dir = state.shard_dir().await;
    let out_dir = crate::pom::scratch_dir(&shard_dir);
    let id = uuid::Uuid::new_v4().simple().to_string();
    let title = format!(
        "Fatiando {base_name} em {} {}",
        ranges.len(),
        if ranges.len() == 1 { "fatia" } else { "fatias" }
    );
    jobs()
        .write()
        .await
        .insert(id.clone(), new_job(&id, "slice", title, "preparando"));

    let job_id = id.clone();
    let label = req.name.clone().unwrap_or_else(|| base_name.clone());
    let node_id = state.network.node_id.clone();
    let root = shard_dir.clone();
    tokio::spawn(async move {
        let result = async {
            if let Some(warning) = destination_warning {
                set_job_phase(
                    &job_id,
                    "verificando espaço",
                    &warning,
                    Some(destination.to_string_lossy().into_owned()),
                )
                .await;
            }
            let source_sha256 = source_model_sha256_for_job(&source, &job_id).await?;
            run_slice(
                state,
                source,
                shard_dir,
                out_dir,
                base_name,
                label,
                ranges,
                total_layers,
                node_id,
                source_sha256,
                &job_id,
            )
            .await
        }
        .await;
        match result {
            Ok(m) => {
                let msg = format!(
                    "{} fatia{} em {} · pom {}",
                    m.parts.len(),
                    if m.parts.len() == 1 { "" } else { "s" },
                    crate::pom::dir_for(&root, &m).display(),
                    crate::pom::short_id(&m.pom_id)
                );
                set_job(&job_id, "done", "pronto", &msg).await;
            }
            Err(e) => {
                warn!("fatiamento falhou: {e:#}");
                set_job(&job_id, "failed", "erro", &format!("{e:#}")).await;
            }
        }
    });

    Json(json!({ "job_id": id })).into_response()
}

/// Extract every tensor-parallel rank from one local GGUF and seal the whole
/// group in the source model's unified manifest.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_tp_slice(
    state: GatewayState,
    source: std::path::PathBuf,
    shard_dir: std::path::PathBuf,
    scratch: std::path::PathBuf,
    base_name: String,
    label: String,
    total_layers: u32,
    node_id: String,
    source_sha256: String,
    tp_size: u32,
    job_id: &str,
) -> anyhow::Result<crate::pom::PomManifest> {
    let source_path = source.to_string_lossy().into_owned();
    let mut shards = Vec::with_capacity(tp_size as usize);

    for rank in 0..tp_size {
        set_job_phase(
            job_id,
            "empacotando cabeçalho",
            &format!("preparando o shard TP rank {}/{}", rank + 1, tp_size),
            Some(source.to_string_lossy().into_owned()),
        )
        .await;

        let progress_job = job_id.to_string();
        let outcome =
            plan::extract_tp_shard(&source_path, &scratch, None, tp_size, rank, move |pct| {
                let id = progress_job.clone();
                tokio::spawn(async move { set_job_progress(&id, pct).await });
            })
            .await?;

        set_job_phase(
            job_id,
            "verificando",
            &format!("shard TP rank {} pronto; verificando e selando", rank + 1),
            Some(outcome.output_path.clone()),
        )
        .await;

        shards.push(crate::pom::PomPart {
            sha256: outcome.sha256.clone(),
            bytes: outcome.bytes,
            start_layer: 0,
            end_layer: total_layers - 1,
            is_first: true,
            is_last: true,
            tp: Some(crate::pom::PomTp {
                rank,
                size: tp_size,
            }),
        });
        invalidate_catalog(state.database.clone()).await;
        if let Some(job) = jobs().write().await.get_mut(job_id) {
            job.outputs.push(outcome.output_path);
        }
    }

    let manifest = tensor_slice_manifest(
        base_name,
        label,
        total_layers,
        node_id,
        source_sha256,
        source
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(crate::catalog::parse_quant),
        shards,
    );
    let destination = crate::pom::finalize_run(&shard_dir, &scratch, &manifest)?;
    info!("fatiamento TP guardado em {}", destination.display());
    invalidate_catalog(state.database.clone()).await;
    Ok(manifest)
}

pub(super) fn existing_layer_slice(
    shard_dir: &std::path::Path,
    source_sha256: &str,
    start_layer: u32,
    end_layer: u32,
) -> Option<plan::SliceOutcome> {
    let directory = crate::pom::locate_source_dir(shard_dir, source_sha256)?;
    let part = crate::pom::load_dir(&directory)
        .into_iter()
        .flat_map(|manifest| manifest.parts)
        .find(|part| {
            part.start_layer == start_layer
                && part.end_layer == end_layer
                && crate::pom::part_path(&directory, &part.sha256)
                    .ok()
                    .is_some_and(|path| path.is_file())
        })?;
    let path = crate::pom::part_path(&directory, &part.sha256).ok()?;
    let (actual_sha256, bytes) = crate::pom::hash_file(&path).ok()?;
    (actual_sha256 == part.sha256 && bytes == part.bytes && bytes > 1024).then_some(
        plan::SliceOutcome {
            output_path: path.to_string_lossy().into_owned(),
            bytes,
            sha256: part.sha256,
            reused: true,
        },
    )
}

/// Corta o modelo inteiro nas faixas pedidas e mescla as partes no manifesto v1.
///
/// É **aqui** que um fatiamento vira uma coisa só: as partes saem nomeadas pelo
/// próprio hash e o manifesto, escrito no fim, é o que diz de que modelo cada
/// parte saiu. Sem ele as partes seriam arquivos soltos de nome
/// ilegível - o manifesto é o que substitui a faixa que antes vivia no nome.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_slice(
    state: GatewayState,
    source: std::path::PathBuf,
    shard_dir: std::path::PathBuf,
    scratch: std::path::PathBuf,
    base_name: String,
    label: String,
    ranges: Vec<(u32, u32)>,
    total_layers: u32,
    node_id: String,
    source_sha256: String,
    job_id: &str,
) -> anyhow::Result<crate::pom::PomManifest> {
    let parts = ranges.len() as u32;
    let mut pieces: Vec<crate::pom::PomPart> = Vec::new();

    for (index, (start, end)) in ranges.iter().copied().enumerate() {
        let index = index as u32;
        // Reading the source and packing the new header comes before a single
        // byte of weights is written, and on a model with a large tokenizer it
        // is most of the wall time. Saying which phase we are in beats a bar
        // that sits still with no explanation.
        let label = format!(
            "fatia {}/{} · camadas {start}–{end} de {total_layers}",
            index + 1,
            parts
        );
        set_job_phase(
            job_id,
            "empacotando cabeçalho",
            &format!("{label} · preparando os metadados da fatia"),
            Some(source.to_string_lossy().into_owned()),
        )
        .await;

        let progress_job = job_id.to_string();
        let writing_label = label.clone();
        let first_report = Arc::new(AtomicBool::new(true));
        let outcome =
            if let Some(existing) = existing_layer_slice(&shard_dir, &source_sha256, start, end) {
                set_job_phase(
                    job_id,
                    "verificando",
                    &format!("{label} · fatia idêntica já existe; conferida por hash"),
                    Some(existing.output_path.clone()),
                )
                .await;
                existing
            } else {
                plan::extract_shard(
                    &source.to_string_lossy(),
                    &scratch,
                    None,
                    start,
                    end,
                    start == 0,
                    end + 1 >= total_layers,
                    move |pct| {
                        let id = progress_job.clone();
                        let overall = overall_progress(index, parts, pct);
                        let announce = first_report
                            .swap(false, Ordering::Relaxed)
                            .then(|| format!("{writing_label} · gravando"));
                        tokio::spawn(async move {
                            if let Some(message) = announce {
                                set_job(&id, "running", "escrevendo", &message).await;
                            }
                            set_job_progress(&id, overall).await;
                        });
                    },
                )
                .await?
            };

        set_job_progress(job_id, overall_progress(index + 1, parts, 0.0)).await;
        set_job_phase(
            job_id,
            "verificando",
            &format!("{label} · saída pronta; verificando e selando"),
            Some(outcome.output_path.clone()),
        )
        .await;
        info!(
            "fatia {}/{parts} pronta: {} ({:.1} GB, {})",
            index + 1,
            outcome.output_path,
            outcome.bytes as f64 / 1_073_741_824.0,
            if outcome.reused {
                "reaproveitada"
            } else {
                "gerada"
            }
        );
        pieces.push(crate::pom::PomPart {
            sha256: outcome.sha256.clone(),
            bytes: outcome.bytes,
            start_layer: start,
            end_layer: end,
            is_first: start == 0,
            is_last: end + 1 >= total_layers,
            tp: None,
        });

        // Show up in the catalog as each one lands, not only at the end.
        invalidate_catalog(state.database.clone()).await;
        let mut map = jobs().write().await;
        if let Some(job) = map.get_mut(job_id) {
            job.outputs.push(outcome.output_path);
        }
    }

    let quant = source
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(crate::catalog::parse_quant);
    let manifesto = crate::pom::PomManifest::new(
        crate::pom::PomModel {
            name: base_name,
            label,
            total_layers,
            quant,
        },
        crate::pom::PomKind::Layers,
        node_id,
        source_sha256,
        pieces,
    );
    // Cobrir o modelo inteiro é o caso normal, mas fatiar só um pedaço é
    // legítimo (o operador escolhe as faixas). Dizer isso em voz alta evita a
    // descoberta lá na frente, na implantação, de que o conjunto não fecha.
    if !manifesto.complete() {
        warn!(
            "as fatias de {} não cobrem as {total_layers} camadas do modelo",
            manifesto.model.name
        );
    }

    // A rodada inteira se muda de uma vez para `<modelo>-<hash do modelo>`, e o
    // `.pom` é selado lá por último: um manifesto na pasta é, por construção,
    // um fatiamento inteiro que existe em disco.
    let destination = crate::pom::finalize_run(&shard_dir, &scratch, &manifesto)?;
    info!("fatiamento guardado em {}", destination.display());
    invalidate_catalog(state.database.clone()).await;

    Ok(manifesto)
}

/// Materialize the exact ranges selected by deploy from a local whole model.
/// The deploy coordinator calls this on the peer that proposed the model before
/// asking the destination to pull the resulting slice files.
pub(super) async fn slice_ranges(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<SliceRangesRequest>,
) -> Response {
    match materialize_slice_ranges(&state, &req).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(crate) async fn materialize_slice_ranges(
    state: &GatewayState,
    req: &SliceRangesRequest,
) -> Result<SliceRangesResponse, String> {
    if req.ranges.is_empty() || req.ranges.len() > 64 {
        return Err("o fatiamento precisa de entre 1 e 64 faixas".to_string());
    }
    if req.name.trim().is_empty() {
        return Err("o fatiamento precisa do nome do modelo".to_string());
    }
    let options = model_layer_options_local(state, &req.model_path).await?;
    let source = std::path::Path::new(&req.model_path)
        .canonicalize()
        .map_err(|error| format!("não foi possível resolver o modelo inteiro: {error}"))?;
    let layout = layout_of(&source).await?;
    if req.total_layers != options.total_layers {
        return Err(format!(
            "o modelo declara {} camadas, mas a implantação pediu {}",
            options.total_layers, req.total_layers
        ));
    }
    for range in &req.ranges {
        if range.start_layer > range.end_layer || range.end_layer >= req.total_layers {
            return Err(format!(
                "faixa L{}-{} fora do modelo de {} camadas",
                range.start_layer, range.end_layer, req.total_layers
            ));
        }
        if !layout.is_valid_slice_start(range.start_layer) {
            return Err(format!(
                "a faixa L{}-{} começa fora de uma fronteira arquitetural real do modelo",
                range.start_layer, range.end_layer
            ));
        }
        if layout.snap_end_layer(range.start_layer, range.end_layer, req.total_layers)
            != range.end_layer
        {
            return Err(format!(
                "a faixa L{}-{} não respeita as fronteiras arquiteturais reais do modelo",
                range.start_layer, range.end_layer
            ));
        }
    }
    let requested_ranges = req
        .ranges
        .iter()
        .map(|range| (range.start_layer, range.end_layer))
        .collect::<Vec<_>>();
    layout
        .validate_slice_ranges(&requested_ranges)
        .map_err(|error| error.to_string())?;
    let source = source;
    let shard_dir = state.shard_dir().await;
    let scratch = crate::pom::scratch_dir(&shard_dir);
    let job_id = uuid::Uuid::new_v4().simple().to_string();
    jobs().write().await.insert(
        job_id.clone(),
        new_job(
            &job_id,
            "slice",
            format!("Fatiando {} para a implantação", req.name),
            "preparando",
        ),
    );
    let source_sha256 = match source_model_sha256_for_job(&source, &job_id).await {
        Ok(hash) => hash,
        Err(error) => {
            set_job(&job_id, "failed", "erro", &format!("{error:#}")).await;
            return Err(format!(
                "não foi possível identificar o modelo inteiro: {error:#}"
            ));
        }
    };
    let ranges = req
        .ranges
        .iter()
        .map(|range| (range.start_layer, range.end_layer))
        .collect();
    let result = run_slice(
        state.clone(),
        source,
        shard_dir.clone(),
        scratch,
        req.name.clone(),
        req.name.clone(),
        ranges,
        req.total_layers,
        state.network.node_id.clone(),
        source_sha256,
        &job_id,
    )
    .await;
    let manifest = match result {
        Ok(manifest) => manifest,
        Err(error) => {
            set_job(&job_id, "failed", "erro", &format!("{error:#}")).await;
            return Err(format!(
                "não foi possível fatiar o modelo inteiro: {error:#}"
            ));
        }
    };
    let directory = crate::pom::dir_for(&shard_dir, &manifest);
    let slices = manifest
        .parts
        .iter()
        .map(|part| {
            crate::pom::part_path(&directory, &part.sha256)
                .map(|path| MaterializedSlice {
                    start_layer: part.start_layer,
                    end_layer: part.end_layer,
                    path: path.to_string_lossy().into_owned(),
                    sha256: part.sha256.clone(),
                })
                .map_err(|error| {
                    format!("não foi possível localizar a fatia materializada: {error}")
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    set_job(
        &job_id,
        "done",
        "pronto",
        &format!(
            "{} fatia(s) materializada(s) para a implantação",
            slices.len()
        ),
    )
    .await;
    Ok(SliceRangesResponse { slices })
}
