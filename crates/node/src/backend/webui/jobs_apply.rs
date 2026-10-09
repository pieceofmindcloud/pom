use super::*;
// ── Job tracking ─────────────────────────────────────────────────────────────
//
// Slicing a multi-GB model takes minutes, far longer than an HTTP request
// should hold open, so `apply` returns a job id the UI polls.

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: String,
    /// "running" | "done" | "failed"
    pub state: &'static str,
    /// "slice" — cutting a model here — or "apply" — binding this node to a
    /// range. The notification list says which, because one is the operator's
    /// own doing and the other can arrive from a peer's launch.
    pub kind: &'static str,
    /// One line naming the work, for a list that has no room for the detail.
    pub title: String,
    pub step: String,
    pub message: String,
    /// 0..100 while `split_model.py` reports it; `None` before the first line
    /// arrives, which the screen shows as an indeterminate bar rather than a
    /// zero that looks stuck.
    pub progress: Option<f64>,
    /// Bytes processed in the current phase, when that phase has a known
    /// total. This is deliberately separate from `progress`: reading the
    /// source happens before output bytes exist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_processed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_file: Option<String>,
    /// Timestamp used by the UI to show how long the current phase has lived.
    pub phase_started_at: String,
    /// Files the job has produced so far, in order.
    pub outputs: Vec<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
    /// Optional fields used by remote model downloads. Slicing and apply
    /// jobs keep these fields absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_downloaded: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_bytes_per_sec: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub(crate) fn jobs() -> &'static RwLock<HashMap<String, Job>> {
    static JOBS: std::sync::OnceLock<RwLock<HashMap<String, Job>>> = std::sync::OnceLock::new();
    JOBS.get_or_init(|| RwLock::new(HashMap::new()))
}

pub(crate) fn new_job(id: &str, kind: &'static str, title: impl Into<String>, step: &str) -> Job {
    Job {
        id: id.to_string(),
        state: "running",
        kind,
        title: title.into(),
        step: step.to_string(),
        message: String::new(),
        progress: None,
        bytes_processed: None,
        current_file: None,
        phase_started_at: chrono::Utc::now().to_rfc3339(),
        outputs: Vec::new(),
        started_at: chrono::Utc::now().to_rfc3339(),
        finished_at: None,
        repo_id: None,
        revision: None,
        filename: None,
        quantization: None,
        download_status: None,
        bytes_downloaded: None,
        total_bytes: None,
        speed_bytes_per_sec: None,
        eta_seconds: None,
        local_path: None,
        error: None,
    }
}

/// Updates the rich state of a download without creating another progress
/// store. The semaphore and the token live in the provider's service; this job
/// is the source of truth the UI queries with ETag.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn set_model_download_progress(
    id: &str,
    status: &'static str,
    progress: Option<f64>,
    bytes_downloaded: Option<u64>,
    total_bytes: Option<u64>,
    speed_bytes_per_sec: Option<u64>,
    eta_seconds: Option<u64>,
    local_path: Option<String>,
    error: Option<String>,
) {
    let terminal = matches!(status, "completed" | "cancelled" | "failed");
    let state = match status {
        "completed" => "done",
        "cancelled" | "failed" => "failed",
        _ => "running",
    };
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(id) {
        job.state = state;
        job.download_status = Some(status);
        job.progress = progress.map(|pct| pct.clamp(0.0, 100.0));
        job.bytes_downloaded = bytes_downloaded;
        job.total_bytes = total_bytes;
        job.speed_bytes_per_sec = speed_bytes_per_sec;
        job.eta_seconds = eta_seconds;
        if local_path.is_some() {
            job.local_path = local_path;
        }
        if error.is_some() {
            job.error = error.clone();
            job.message = error.unwrap_or_default();
        }
        job.step = status.to_string();
        if terminal {
            job.finished_at = Some(chrono::Utc::now().to_rfc3339());
            if status == "completed" {
                job.progress = Some(100.0);
            }
        }
    }
}

/// How long a finished job stays listed before it is dropped.
///
/// Long enough that an operator who was on another screen still sees "pronto"
/// when they come back; short enough that yesterday's runs are not news.
pub(super) const JOB_RETENTION: Duration = Duration::from_secs(600);

/// Every job this node knows about, newest first.
///
/// This is what makes the notification menu survive a reload: the work lives
/// here, on the node running it, and the browser only ever asks what is going
/// on. Finished jobs are pruned as the list is built, which is the only moment
/// anybody looks at them.
pub(super) async fn list_jobs(headers: HeaderMap) -> Response {
    let cutoff = chrono::Utc::now() - chrono::Duration::from_std(JOB_RETENTION).unwrap();
    let mut map = jobs().write().await;
    map.retain(|_, job| {
        job.state == "running"
            || job
                .finished_at
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .is_none_or(|t| t.with_timezone(&chrono::Utc) > cutoff)
    });

    let mut list: Vec<Job> = map.values().cloned().collect();
    list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    conditional_json(&headers, &json!({ "jobs": list }))
}

pub(crate) async fn set_job(id: &str, state: &'static str, step: &str, message: &str) {
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(id) {
        job.state = state;
        if job.step != step {
            job.phase_started_at = chrono::Utc::now().to_rfc3339();
        }
        job.step = step.to_string();
        job.message = message.to_string();
        if state != "running" {
            job.finished_at = Some(chrono::Utc::now().to_rfc3339());
            // Finished is finished: leaving a bar at 87% on a job that is done
            // reads as a stall.
            if state == "done" {
                job.progress = Some(100.0);
            }
            if state == "failed" {
                job.error = Some(message.to_string());
            }
        }
    }
}

pub(crate) async fn set_job_phase(
    id: &str,
    step: &str,
    message: &str,
    current_file: Option<String>,
) {
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(id) {
        job.state = "running";
        job.step = step.to_string();
        job.message = message.to_string();
        job.phase_started_at = chrono::Utc::now().to_rfc3339();
        job.bytes_processed = None;
        job.total_bytes = None;
        job.speed_bytes_per_sec = None;
        job.eta_seconds = None;
        job.current_file = current_file;
    }
}

/// Update byte activity only while the job is still in the phase that emitted
/// it. Hashing happens in a blocking task, so this guard prevents a late read
/// report from replacing a newer "writing" or "verifying" state.
pub(crate) async fn set_job_byte_progress(
    id: &str,
    step: &str,
    bytes_processed: u64,
    total_bytes: u64,
    current_file: Option<String>,
) {
    let now = chrono::Utc::now();
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(id) {
        if job.state != "running" || job.step != step {
            return;
        }
        if job
            .bytes_processed
            .is_some_and(|current| bytes_processed < current)
        {
            return;
        }
        let phase_started = chrono::DateTime::parse_from_rfc3339(&job.phase_started_at)
            .map(|value| value.with_timezone(&chrono::Utc))
            .unwrap_or(now);
        let elapsed = (now - phase_started).num_milliseconds().max(1) as f64 / 1000.0;
        let speed = bytes_processed as f64 / elapsed;
        job.bytes_processed = Some(bytes_processed);
        job.total_bytes = Some(total_bytes);
        job.speed_bytes_per_sec = speed.is_finite().then_some(speed.round() as u64);
        job.eta_seconds = if bytes_processed < total_bytes && speed > 0.0 {
            Some(((total_bytes - bytes_processed) as f64 / speed).ceil() as u64)
        } else {
            None
        };
        if current_file.is_some() {
            job.current_file = current_file;
        }
    }
}

/// Progress only ever moves forward.
///
/// Each reported percentage is delivered by its own task, so a line from the
/// slice that just finished can land after the next one has started and drag
/// the bar backwards — which reads as the work being redone.
pub(crate) async fn set_job_progress(id: &str, pct: f64) {
    let pct = pct.clamp(0.0, 100.0);
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(id) {
        if job.progress.is_none_or(|current| pct > current) {
            job.progress = Some(pct);
        }
    }
}

/// Progress reported by the slicer, mapped onto the whole job: part `index` of
/// `total` running at `pct`.
pub(super) fn overall_progress(index: u32, total: u32, pct: f64) -> f64 {
    let total = total.max(1) as f64;
    ((index as f64 + pct / 100.0) / total * 100.0).clamp(0.0, 100.0)
}

pub(super) async fn job_status(Path(id): Path<String>) -> Response {
    match jobs().read().await.get(&id) {
        Some(job) => Json(job.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown job" })),
        )
            .into_response(),
    }
}

/// Slice locally if needed, then reconfigure this node onto the result.
pub(super) async fn apply(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ApplyRequest>,
) -> Response {
    if req.params.no_embeddings {
        if let Err(reason) =
            crate::capabilities::validate_no_embeddings(&req.no_embeddings_evaluation(), true)
        {
            return bad_request(reason);
        }
    }
    // A merge request lists the files it needs instead of pointing at one; the
    // guard below checks whichever set is actually going to be read.
    if let Some(parts) = &req.merge {
        for p in parts {
            if !std::path::Path::new(&p.source_model_path).exists() {
                return bad_request(format!(
                    "slice not found on this peer: {}",
                    p.source_model_path
                ));
            }
        }
    } else if !std::path::Path::new(&req.source_model_path).exists() {
        return bad_request(format!(
            "model not found on this peer: {}",
            req.source_model_path
        ));
    }
    if req.start_layer > req.end_layer {
        return bad_request("inverted layer range");
    }
    // Answered here rather than from inside the spawned job: a configuration
    // that `NodeConfig::validate` will reject is a bad request, and reporting
    // it as one keeps the caller from reading "job criado" as "deu certo".
    if req.total_layers == 0 {
        return bad_request(format!(
            "slice {} did not come with the total number of layers of the model, and without it the node does not know whether this range is the start, the middle or the end of the network",
            req.base_name
        ));
    }
    if req.end_layer >= req.total_layers {
        return bad_request(format!(
            "slice L{}-{} goes past the end of a model with {} layers",
            req.start_layer, req.end_layer, req.total_layers
        ));
    }

    let id = uuid::Uuid::new_v4().simple().to_string();
    let title = format!(
        "Loading {} · layers {}–{}",
        req.base_name, req.start_layer, req.end_layer
    );
    jobs()
        .write()
        .await
        .insert(id.clone(), new_job(&id, "apply", title, "preparing"));

    let job_id = id.clone();
    tokio::spawn(async move {
        match run_apply(state, req, &job_id).await {
            Ok(path) => {
                set_job(&job_id, "done", "pronto", &format!("serving {path}")).await;
            }
            Err(e) => {
                warn!("apply failed: {e:#}");
                set_job(&job_id, "failed", "error", &format!("{e:#}")).await;
            }
        }
    });

    Json(json!({ "job_id": id })).into_response()
}

/// Which tensor shard this apply has to extract, if any.
///
/// **Every** rank of a tensor-parallel group extracts its own, rank 0
/// included. The `role` (`primary`/`worker`) says who hosts the collective
/// (`rank0_url`), **not** who slices - and confusing the two was the defect
/// of 17/08: the condition was `role == "worker"`, so the primary fell into the
/// `skip_split` branch and served the whole GGUF, running attention and FFN at
/// full width against workers at half width. The `_tp0of2.gguf` never came to
/// exist on disk, and the worker's `_tp1of2.gguf` was the group's only half.
pub(super) fn tp_shard_to_extract(req: &ApplyRequest) -> Option<&crate::network::TpIdentity> {
    req.tp.as_ref().filter(|_| !req.skip_split)
}

pub(super) const SOURCE_HASH_PROGRESS_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn hash_source_with_progress<F>(
    path: &std::path::Path,
    mut on_progress: F,
) -> anyhow::Result<String>
where
    F: FnMut(u64, u64),
{
    let source = crate::multipart::GgufSource::open(path)?;
    let total = source.source_bytes();
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut reported = 0u64;
    on_progress(0, total);

    for part in source.parts() {
        for chunk in part.map().chunks(8 * 1024 * 1024) {
            hasher.update(chunk);
            done += chunk.len() as u64;
            if done.saturating_sub(reported) >= SOURCE_HASH_PROGRESS_BYTES || done == total {
                reported = done;
                on_progress(done, total);
            }
        }
    }
    if done != total {
        anyhow::bail!("the GGUF size changed while the hash was being computed");
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(super) async fn source_model_sha256(path: &std::path::Path) -> anyhow::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || hash_source_with_progress(&path, |_, _| {}))
        .await
        .context("compute the hash of the original model")?
}

pub(super) async fn source_model_sha256_for_job(
    path: &std::path::Path,
    job_id: &str,
) -> anyhow::Result<String> {
    let path = path.to_path_buf();
    let display_path = path.to_string_lossy().into_owned();
    set_job_phase(
        job_id,
        "reading model",
        "reading the source model to identify the operation",
        Some(display_path.clone()),
    )
    .await;
    let progress_job = job_id.to_string();
    tokio::task::spawn_blocking(move || {
        hash_source_with_progress(&path, |done, total| {
            let id = progress_job.clone();
            let current_file = display_path.clone();
            tokio::spawn(async move {
                set_job_byte_progress(&id, "reading model", done, total, Some(current_file)).await;
            });
        })
    })
    .await
    .context("compute the hash of the original model")?
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ModelIdentityRequest {
    pub path: String,
    #[serde(default = "default_true")]
    pub hash: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ModelMetadataRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelIdentityResponse {
    pub canonical_path: String,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LayerCut {
    pub end_layer: u32,
    pub layers: u32,
    pub size_bytes: u64,
    pub remainder_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ModelLayerOptions {
    pub total_layers: u32,
    pub cuts: Vec<LayerCut>,
    /// Valid starts for a standalone slice, derived from the GGUF architecture.
    pub valid_starts: Vec<u32>,
    pub granularity: Option<u32>,
    pub rule: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ModelLayerOptionsRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SliceRange {
    pub start_layer: u32,
    pub end_layer: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SliceRangesRequest {
    pub model_path: String,
    pub name: String,
    pub total_layers: u32,
    pub ranges: Vec<SliceRange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MaterializedSlice {
    pub start_layer: u32,
    pub end_layer: u32,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SliceRangesResponse {
    pub slices: Vec<MaterializedSlice>,
}

/// Proves the identity of a file selected for deployment without hashing the
/// catalog during ordinary polling. The path must already be in this peer's
/// catalog, so a caller cannot turn the route into an arbitrary file reader.
pub(super) async fn model_identity(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ModelIdentityRequest>,
) -> Response {
    match model_identity_local(&state, &req.path, req.hash).await {
        Ok(identity) => Json(identity).into_response(),
        Err(error) => bad_request(error),
    }
}

/// Read the header for one model selected by the operator and publish only
/// that result into the local catalog. This is the deployment escape hatch
/// while unrelated files are still being enriched in the background.
pub(super) async fn model_metadata(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ModelMetadataRequest>,
) -> Response {
    match model_metadata_local(&state, &req.path).await {
        Ok(model) => Json(model).into_response(),
        Err(error) => bad_request(error),
    }
}

fn catalog_path_error(requested: &std::path::Path, metadata_complete: bool) -> String {
    if !metadata_complete {
        return format!(
            "the catalog of this peer is still being discovered; wait for metadata_complete=true before selecting {}",
            requested.display()
        );
    }
    format!(
        "the file {} does not belong to the catalog of this peer",
        requested.display()
    )
}

pub(crate) async fn model_metadata_local(
    state: &GatewayState,
    requested: &str,
) -> Result<crate::catalog::DiscoveredModel, String> {
    let requested_path = std::path::Path::new(requested);
    let canonical = requested_path
        .canonicalize()
        .map_err(|e| format!("could not resolve the model file: {e}"))?;
    let catalog = local_catalog(state).await;
    let mut model = catalog
        .models
        .iter()
        .find(|model| {
            std::path::Path::new(&model.path)
                .canonicalize()
                .ok()
                .is_some_and(|path| path == canonical)
        })
        .cloned()
        .ok_or_else(|| catalog_path_error(requested_path, catalog.metadata_complete))?;

    if !catalog::metadata_is_current(&model) {
        model = tokio::task::spawn_blocking(move || {
            catalog::enrich_selected(&mut model).map(|()| model)
        })
        .await
        .map_err(|e| format!("reading the GGUF metadata failed: {e}"))??;
    }

    let mut guard = catalog_cache().lock().await;
    if let Some(current) = guard.as_mut() {
        let catalog = Arc::make_mut(&mut current.catalog);
        if let Some(target) = catalog.models.iter_mut().find(|target| {
            std::path::Path::new(&target.path)
                .canonicalize()
                .ok()
                .is_some_and(|path| path == canonical)
        }) {
            *target = model.clone();
            catalog::rebuild_shard_sets(catalog);
            catalog.metadata_complete = catalog.models.iter().all(catalog::metadata_is_current);
        }
    }
    Ok(model)
}

pub(crate) async fn model_identity_local(
    state: &GatewayState,
    requested: &str,
    calculate_hash: bool,
) -> Result<ModelIdentityResponse, String> {
    let requested = std::path::Path::new(requested);
    let canonical = requested
        .canonicalize()
        .map_err(|e| format!("could not resolve the model file: {e}"))?;
    let catalog = local_catalog(state).await;
    let listed = catalog.models.iter().any(|model| {
        std::path::Path::new(&model.path)
            .canonicalize()
            .ok()
            .is_some_and(|path| path == canonical)
    });
    if !listed {
        return Err(catalog_path_error(requested, catalog.metadata_complete));
    }
    let sha256 = if calculate_hash {
        Some(
            source_model_sha256(&canonical)
                .await
                .map_err(|e| format!("could not compute the identity of the GGUF: {e}"))?,
        )
    } else {
        None
    };
    Ok(ModelIdentityResponse {
        canonical_path: canonical.to_string_lossy().into_owned(),
        sha256,
    })
}

pub(super) async fn model_layer_options(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ModelLayerOptionsRequest>,
) -> Response {
    match model_layer_options_local(&state, &req.path).await {
        Ok(options) => Json(options).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(crate) async fn model_layer_options_local(
    state: &GatewayState,
    requested: &str,
) -> Result<ModelLayerOptions, String> {
    let requested_path = std::path::Path::new(requested);
    let canonical = requested_path
        .canonicalize()
        .map_err(|e| format!("could not resolve the whole model: {e}"))?;
    let catalog = local_catalog(state).await;
    let listed = catalog.models.iter().any(|model| {
        std::path::Path::new(&model.path)
            .canonicalize()
            .ok()
            .is_some_and(|path| path == canonical)
            && model.shard_range.is_none()
    });
    if !listed {
        let error = if !catalog.metadata_complete {
            format!(
                "the catalog of this peer is still being discovered; wait for metadata_complete=true before selecting {}",
                requested_path.display()
            )
        } else {
            format!(
                "the file {} is not a whole model of the catalog of this peer",
                requested_path.display()
            )
        };
        return Err(error);
    }
    let layout = layout_of(&canonical).await?;
    let total_layers = layout.layer_count();
    if total_layers == 0 {
        return Err("the GGUF declares no layers to slice".to_string());
    }
    let cuts = layout
        .valid_prefix_end_layers()
        .into_iter()
        .map(|end_layer| LayerCut {
            end_layer,
            layers: end_layer + 1,
            size_bytes: layout.slice_bytes(0, end_layer, end_layer + 1 >= total_layers),
            remainder_bytes: if end_layer + 1 < total_layers {
                layout.slice_bytes(end_layer + 1, total_layers - 1, true)
            } else {
                0
            },
        })
        .collect::<Vec<_>>();
    let valid_starts = std::iter::once(0)
        .chain(layout.arch_boundaries())
        .collect::<Vec<_>>();
    let rule = if layout.arch_boundaries().is_empty() {
        "dense model: any cut at a whole layer".to_string()
    } else {
        "hybrid model: cuts only at the real architectural boundaries of the GGUF".to_string()
    };
    Ok(ModelLayerOptions {
        total_layers,
        cuts,
        valid_starts,
        granularity: layout.prefix_cut_granularity(),
        rule,
    })
}

/// Seals a part cut here into the original model's manifest.
///
/// `apply` cuts **one** range, for this machine. Before, that ended up in a
/// folder named after the model with no manifest at all, and the catalog showed
/// an orphan `<sha256>.gguf` that did not say which model it belonged to. A
/// single-part manifest solves both: it names the slice and puts it in the
/// model's shared folder, without losing earlier alternatives.
pub(super) fn seal_single_part(
    shard_dir: &std::path::Path,
    scratch: &std::path::Path,
    req: &ApplyRequest,
    outcome: &plan::SliceOutcome,
    source_sha256: impl Into<String>,
    kind: crate::pom::PomKind,
    tp: Option<crate::pom::PomTp>,
    node_id: &str,
) -> anyhow::Result<String> {
    let part = crate::pom::PomPart {
        sha256: outcome.sha256.clone(),
        bytes: outcome.bytes,
        start_layer: req.start_layer,
        end_layer: req.end_layer,
        is_first: req.is_first(),
        is_last: req.is_last(),
        tp,
    };
    let mut manifesto = crate::pom::PomManifest::new(
        crate::pom::PomModel {
            name: req.base_name.clone(),
            label: model_label(req),
            total_layers: req.total_layers,
            quant: std::path::Path::new(&req.source_model_path)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(crate::catalog::parse_quant),
        },
        kind,
        node_id,
        source_sha256,
        Vec::new(),
    );
    match kind {
        crate::pom::PomKind::TensorParallel => manifesto.tp_parts.push(part),
        crate::pom::PomKind::Layers => manifesto.parts.push(part),
        crate::pom::PomKind::Combined => {
            anyhow::bail!("an isolated part cannot seal a combined manifest")
        }
    }
    let destination = crate::pom::finalize_run(shard_dir, scratch, &manifesto)?;
    Ok(crate::pom::part_path(&destination, &outcome.sha256)?
        .to_string_lossy()
        .into_owned())
}

pub(super) async fn run_apply(
    state: GatewayState,
    req: ApplyRequest,
    job_id: &str,
) -> anyhow::Result<String> {
    let model_path = if let Some(tp) = tp_shard_to_extract(&req) {
        set_job(
            job_id,
            "running",
            "slicing",
            &format!("extracting tensor shard rank {}/{}", tp.rank, tp.size),
        )
        .await;
        let shard_dir = state.shard_dir().await;
        let source_sha256 =
            source_model_sha256_for_job(std::path::Path::new(&req.source_model_path), job_id)
                .await?;
        // Do I already have this part? The search is by content, across all
        // runs - the destination folder depends on the manifest, which only
        // exists afterwards.
        if let Some(ready) = req
            .part_sha256
            .as_deref()
            .and_then(|sha| crate::transfer::find_part(&shard_dir, sha))
        {
            info!("tensor shard already on disk: {}", ready.display());
            ready.to_string_lossy().into_owned()
        } else {
            let scratch = crate::pom::scratch_dir(&shard_dir);
            let progress_job = job_id.to_string();
            let outcome = plan::extract_tp_shard(
                &req.source_model_path,
                &scratch,
                None,
                tp.size,
                tp.rank,
                move |pct| {
                    let id = progress_job.clone();
                    tokio::spawn(async move { set_job_progress(&id, pct).await });
                },
            )
            .await?;
            let path = seal_single_part(
                &shard_dir,
                &scratch,
                &req,
                &outcome,
                source_sha256,
                crate::pom::PomKind::TensorParallel,
                Some(crate::pom::PomTp {
                    rank: tp.rank,
                    size: tp.size,
                }),
                &state.network.node_id,
            )?;
            invalidate_catalog(state.database.clone()).await;
            path
        }
    } else if let Some(parts) = req.merge.clone() {
        let shard_dir = state.shard_dir().await;
        let merged = merge_parts(shard_dir, &req, &parts, job_id).await?;
        invalidate_catalog(state.database.clone()).await;
        merged
    } else if req.skip_split || (req.is_first() && req.is_last() && req.peer_count <= 1) {
        // Nothing to carve out: this peer serves the file as it is.
        req.source_model_path.clone()
    } else {
        set_job(
            job_id,
            "running",
            "slicing",
            &format!(
                "extracting layers {}-{} (may take a few minutes)",
                req.start_layer, req.end_layer
            ),
        )
        .await;

        let shard_dir = state.shard_dir().await;
        let source_sha256 =
            source_model_sha256_for_job(std::path::Path::new(&req.source_model_path), job_id)
                .await?;
        if let Some(ready) = req
            .part_sha256
            .as_deref()
            .and_then(|sha| crate::transfer::find_part(&shard_dir, sha))
        {
            info!("slice already on disk: {}", ready.display());
            ready.to_string_lossy().into_owned()
        } else {
            let scratch = crate::pom::scratch_dir(&shard_dir);
            let progress_job = job_id.to_string();
            let outcome = plan::extract_shard(
                &req.source_model_path,
                &scratch,
                None,
                req.start_layer,
                req.end_layer,
                req.is_first(),
                req.is_last(),
                move |pct| {
                    let id = progress_job.clone();
                    tokio::spawn(async move { set_job_progress(&id, pct).await });
                },
            )
            .await?;
            let path = seal_single_part(
                &shard_dir,
                &scratch,
                &req,
                &outcome,
                source_sha256,
                crate::pom::PomKind::Layers,
                None,
                &state.network.node_id,
            )?;
            invalidate_catalog(state.database.clone()).await;

            info!(
                "shard pronto: {} ({:.1} GB)",
                path,
                outcome.bytes as f64 / 1_073_741_824.0
            );
            path
        }
    };

    set_job(
        job_id,
        "running",
        "loading",
        "starting the inference server",
    )
    .await;

    // The selected deployment label is shared by every peer. Old callers that
    // do not send it retain the profile-derived compatibility fallback.
    let label = model_label(&req);
    let deployment = deployment_name(&req);

    let inference_port = state
        .network
        .inference_port
        .ok_or_else(|| anyhow::anyhow!("this node does not announce an inference port"))?;

    // Sidecars the deployment named by file: the path came with the request,
    // or this machine finds the file in its own folders now.
    let sidecar_roots = {
        let config = state.auth.read().await;
        let mut roots = vec![config.shard_dir_path()];
        roots.extend(config.model_dir_paths());
        roots
    };
    let local_sidecar = |kind, file: Option<&crate::catalog::SidecarFile>| -> anyhow::Result<Option<String>> {
        let Some(file) = file else { return Ok(None) };
        crate::catalog::find_local_sidecar(&sidecar_roots, kind, file)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("{} is not on this machine", file.file_name))
    };
    let requested_mmproj = match req.mmproj_path.clone().filter(|path| !path.is_empty()) {
        Some(path) => Some(path),
        None if req.use_mmproj => local_sidecar(crate::catalog::SidecarKind::Mmproj, req.mmproj_file.as_ref())?,
        None => None,
    };
    let mmproj_path = if req.use_mmproj {
        Some(resolve_mmproj_path(
            std::path::Path::new(&model_path),
            requested_mmproj.as_deref(),
        )?)
    } else {
        None
    };
    // No opt-in: the switch that already exists is `mtp_enabled` in
    // `req.params`. Finding (or not) the file here only decides how MTP runs
    // when the target model does not carry an embedded `nextn` - never whether
    // it was requested. See `models::catalog::find_mtp_draft_sibling`.
    let requested_mtp = match req.mtp_draft_path.clone().filter(|path| !path.is_empty()) {
        Some(path) => Some(path),
        None => local_sidecar(crate::catalog::SidecarKind::Mtp, req.mtp_draft_file.as_ref())?,
    };
    let mtp_draft_path = match requested_mtp.as_deref() {
        // A deployment that resolved the draft from the model's sidecar link
        // names it: the slice lives in the shard folder, and the draft almost
        // never sits next to it.
        Some(path) => {
            anyhow::ensure!(
                std::path::Path::new(path).is_file(),
                "the chosen MTP file does not exist on this machine: {path}"
            );
            Some(path.to_string())
        }
        None => crate::catalog::find_mtp_draft_sibling(std::path::Path::new(&model_path)),
    };

    let cfg = NodeConfig {
        deploy_id: req.deploy_id.clone(),
        model_path: model_path.clone(),
        mmproj_path,
        mtp_draft_path,
        model_label: label,
        deployment_name: deployment,
        start_layer: req.start_layer,
        end_layer: req.end_layer,
        total_layers: req.total_layers,
        peer_count: req.peer_count.max(1),
        peer_index: req.peer_index,
        primary: req.primary,
        tp: req.tp.clone(),
        inference_mode: req.params.inference_mode,
        inference_port,
        single_peer: req.single_peer(),
        no_embeddings: req.no_embeddings(),
        ..NodeConfig::from_params(&req.params, req.tp.is_none())
    };

    // Turns the TP collective on/off before starting the server: the callback
    // has to be registered in llama.cpp before the first decode. Reapplying
    // changes the group only on a restart (tp.rs state is OnceLock, by v1 design).
    match &req.tp {
        Some(tp) if tp.size > 1 && tp.rank0_url.is_some() => {
            let rank0 = tp.members.first();
            crate::tp::register_tp_reduce_with_urls(
                tp.rank,
                tp.size,
                tp.rank0_url.clone().unwrap(),
                rank0
                    .map(|member| member.url_candidates.clone())
                    .unwrap_or_default(),
                rank0
                    .map(|member| member.addr_candidates.clone())
                    .unwrap_or_default(),
                tp.rank0_node_id.clone(),
                rank0.and_then(|m| m.libp2p_peer_id.clone()),
                rank0.is_some_and(|m| m.supports_tunnel),
                Some(state.auth.clone()),
            );
        }
        _ => crate::tp::unregister_tp_reduce(),
    }

    state.runtime.apply(cfg).await?;
    state.network.set_tp(req.tp.clone());
    Ok(model_path)
}

fn resolve_mmproj_path(
    model_path: &std::path::Path,
    requested: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(path) = requested.filter(|path| !path.is_empty()) {
        anyhow::ensure!(
            std::path::Path::new(path).is_file(),
            "this model has no vision: mmproj not found at the given path"
        );
        crate::catalog::validate_mmproj_pair(model_path, std::path::Path::new(path))?;
        return Ok(path.to_string());
    }
    crate::catalog::find_verified_mmproj_sibling(model_path).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::resolve_mmproj_path;

    #[test]
    fn explicit_projector_path_is_used_for_a_slice() {
        let root = std::env::temp_dir().join(format!("pom-mmproj-apply-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let model = root.join("slice.gguf");
        let projector = root.join("projectors/mmproj.gguf");
        std::fs::write(
            &model,
            crate::catalog::tests::text_model_fixture("Model", "llama", 3072),
        )
        .unwrap();
        std::fs::create_dir_all(projector.parent().unwrap()).unwrap();
        std::fs::write(
            &projector,
            crate::catalog::tests::projector_fixture("Model", 3072),
        )
        .unwrap();

        assert_eq!(
            resolve_mmproj_path(&model, projector.to_str()).unwrap(),
            projector.to_string_lossy()
        );
        std::fs::remove_dir_all(root).ok();
    }
}

/// Join local slices into one GGUF covering their union, then serve that.
///
/// The peer that receives a merged step holds (or just received) each part as
/// a separate file; this runs `splitter::merge` on this machine and the result
/// becomes the model file the node is configured with. The output lands in the
/// shard directory under the shared base name, so the catalog picks it up the
/// same way it picks up a slice that was carved here.
pub(super) async fn merge_parts(
    shard_dir: std::path::PathBuf,
    req: &ApplyRequest,
    parts: &[MergeSource],
    job_id: &str,
) -> anyhow::Result<String> {
    if parts.len() < 2 {
        anyhow::bail!("merging fewer than two slices makes no sense");
    }
    let start = parts.first().map(|p| p.start_layer).unwrap_or(0);
    let end = parts
        .last()
        .map(|p| p.end_layer)
        .ok_or_else(|| anyhow::anyhow!("no slice to merge"))?;
    // Reusing the merge requires knowing the hash of the result, and whoever
    // sends the request knows it: it is the same part the `.pom` lists. The
    // search is by content, across all runs - the folder depends on the
    // manifest, which only exists afterwards.
    if let Some(path) = req
        .part_sha256
        .as_deref()
        .and_then(|sha| crate::transfer::find_part(&shard_dir, sha))
    {
        info!(
            "merged model already exists, reusing: {}",
            path.display()
        );
        crate::gguf::read_layout(&path)
            .with_context(|| format!("validate the existing merged GGUF {}", path.display()))?;
        return Ok(path.to_string_lossy().into_owned());
    }
    // The folder name comes from the original model's manifest associated with the parts.
    let out_dir = crate::pom::scratch_dir(&shard_dir);

    set_job(
        job_id,
        "running",
        "merging slices",
        &format!(
            "{} slice(s), layers {}-{} (may take a few minutes)",
            parts.len(),
            start,
            end
        ),
    )
    .await;

    let merge_parts: Vec<crate::splitter::MergePart> = parts
        .iter()
        .map(|p| crate::splitter::MergePart {
            path: std::path::PathBuf::from(&p.source_model_path),
            start_layer: p.start_layer,
            end_layer: p.end_layer,
        })
        .collect();
    // Write to a temporary name and rename when complete, so a merge cut short
    // (kill, power loss) never leaves a half file under the final name where a
    // later apply would reuse it as a finished model. The unique suffix also
    // keeps two concurrent merges of the same range from interleaving writes.
    // The final name is only known at the end: it is the hash of what came out.
    std::fs::create_dir_all(&out_dir).with_context(|| format!("criar {}", out_dir.display()))?;
    let temp = out_dir.join(format!("{}.part", uuid::Uuid::new_v4().simple()));
    let temp_cleanup = temp.clone();
    let progress_job = job_id.to_string();
    let dest_dir = out_dir.clone();

    let (output, bytes) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<(std::path::PathBuf, u64)> {
            let written = crate::splitter::merge(&merge_parts, &temp, move |pct| {
                let id = progress_job.clone();
                tokio::spawn(async move { set_job_progress(&id, pct).await });
            })?;
            crate::gguf::read_layout(&temp)
                .with_context(|| format!("validate the merged GGUF {}", temp.display()))?;
            let dst = crate::pom::part_path(&dest_dir, &written.sha256)?;
            // A destination already taken is another merge that finished first.
            // With content addressing the two files are byte for byte the same,
            // so keeping the one already done is the right thing - and on
            // Windows the rename would not even replace it.
            if dst.exists() {
                if crate::gguf::read_layout(&dst).is_ok() {
                    let _ = std::fs::remove_file(&temp);
                    return Ok((dst, written.bytes));
                }
                info!(
                    "file at the content address is not a valid GGUF, replacing: {}",
                    dst.display()
                );
                std::fs::remove_file(&dst)
                    .with_context(|| format!("remove invalid output {}", dst.display()))?;
            }
            std::fs::rename(&temp, &dst)
                .with_context(|| format!("move {} to {}", temp.display(), dst.display()))?;
            Ok((dst, written.bytes))
        })
        .await
        .context("spawn_blocking for the splitter")?
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temp_cleanup);
        })?;

    // The merge is a new part, and a part without a manifest is a file with an
    // unreadable name in the catalog. A single-part manifest names it and puts
    // the manifest in the model's folder.
    let sha = output
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let part = crate::pom::PomPart {
        sha256: sha.clone(),
        bytes,
        start_layer: start,
        end_layer: end,
        is_first: start == 0,
        is_last: end + 1 >= req.total_layers,
        tp: None,
    };
    let source_ids: Vec<String> = parts
        .iter()
        .filter_map(|part| {
            crate::pom::manifest_for_part_path(std::path::Path::new(&part.source_model_path))
                .map(|manifest| manifest.pom_id)
        })
        .collect();
    let source_sha256 = source_ids.first().cloned();
    if source_ids
        .iter()
        .any(|id| Some(id) != source_sha256.as_ref())
    {
        anyhow::bail!("the slices belong to different original models");
    }
    let source_sha256 = source_sha256
        .ok_or_else(|| anyhow::anyhow!("the slices have no manifest of the original model"))?;
    let mut manifesto = crate::pom::PomManifest::new(
        crate::pom::PomModel {
            name: req.base_name.clone(),
            label: model_label(req),
            total_layers: req.total_layers,
            quant: std::path::Path::new(&req.source_model_path)
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(crate::catalog::parse_quant),
        },
        crate::pom::PomKind::Layers,
        String::new(),
        source_sha256,
        Vec::new(),
    );
    manifesto.parts.push(part);
    let destination = crate::pom::locate_source_dir(&shard_dir, &manifesto.pom_id)
        .unwrap_or_else(|| crate::pom::dir_for(&shard_dir, &manifesto));
    let existing = crate::pom::part_path(&destination, &sha)?;
    if existing.is_file() && crate::gguf::read_layout(&existing).is_err() {
        info!(
            "existing part at the content address is not a valid GGUF, replacing: {}",
            existing.display()
        );
        std::fs::remove_file(&existing)
            .with_context(|| format!("remove invalid part {}", existing.display()))?;
    }
    let folder = crate::pom::finalize_run(&shard_dir, &out_dir, &manifesto)?;
    let final_path = crate::pom::part_path(&folder, &sha)?;

    info!(
        "merged model ready: {} ({:.1} GB)",
        final_path.display(),
        bytes as f64 / 1_073_741_824.0
    );
    Ok(final_path.to_string_lossy().into_owned())
}
