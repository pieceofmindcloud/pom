//! API e jobs de modelos remotos.
//!
//! Esta camada conhece somente `ModelProvider`. O provider padrão é criado no
//! bootstrap do nó; o módulo não monta comandos, lê tokens ou fala com o
//! Hugging Face diretamente.

use crate::auth::ConnectionAuth;
use crate::gateway::GatewayState;
use crate::model_provider::{
    validate_filename, validate_repo_id, validate_revision, DownloadOptions, DownloadProgress,
    DownloadResult, DownloadStatus, DynModelProvider, GgufFile, HfCliStatus, HfError, ManifestFile,
    ModelFile, ModelLicense,
};
use crate::webui::{self, Job};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{mpsc, Mutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

const MAX_SEARCH_PAGE_SIZE: usize = 50;
const MAX_CONCURRENT_DOWNLOADS: usize = 2;
pub(crate) const MODEL_DOWNLOAD_KIND: &str = "model_download";

#[derive(Clone)]
pub(crate) struct ModelService {
    pub(crate) provider: DynModelProvider,
    permits: Arc<Semaphore>,
    admission: Arc<Mutex<()>>,
    cancellations: Arc<RwLock<HashMap<String, CancellationToken>>>,
}

impl ModelService {
    pub(crate) fn new(provider: DynModelProvider) -> Self {
        Self {
            provider,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS)),
            admission: Arc::new(Mutex::new(())),
            cancellations: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct DownloadRequest {
    pub repo_id: String,
    pub revision: Option<String>,
    pub filename: String,
    pub quantization: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownloadIdentity {
    pub repo_id: String,
    pub revision: Option<String>,
    pub filename: String,
}

impl DownloadRequest {
    pub(crate) fn identity(&self) -> Result<DownloadIdentity, HfError> {
        let repo_id = validate_repo_id(&self.repo_id)?;
        let revision = self
            .revision
            .as_deref()
            .map(validate_revision)
            .transpose()?;
        let filename = validate_filename(&self.filename)?;
        if !filename.to_ascii_lowercase().ends_with(".gguf") {
            return Err(HfError::InvalidInput {
                field: "filename".into(),
                message: "somente arquivos GGUF podem ser adicionados ao catálogo".into(),
            });
        }
        if let Some(quantization) = self.quantization.as_deref() {
            if quantization.is_empty()
                || quantization.len() > 64
                || !quantization
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err(HfError::InvalidInput {
                    field: "quantization".into(),
                    message: "quantização contém caracteres inválidos".into(),
                });
            }
        }
        Ok(DownloadIdentity {
            repo_id,
            revision,
            filename,
        })
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct SearchResponse {
    pub models: Vec<RemoteModelSummary>,
    pub total: usize,
    pub page: usize,
    pub page_size: usize,
    pub has_next: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct RemoteModelSummary {
    pub repo_id: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub downloads: Option<u64>,
    pub likes: Option<u64>,
    pub private: Option<bool>,
    pub gated: Option<bool>,
    pub tags: Vec<String>,
    pub last_modified: Option<String>,
    pub has_gguf: Option<bool>,
    pub installed: bool,
    pub downloading: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelDetailsResponse {
    pub repo_id: String,
    pub description: Option<String>,
    pub last_modified: Option<String>,
    pub revision: Option<String>,
    pub sha: Option<String>,
    pub downloads: Option<u64>,
    pub likes: Option<u64>,
    pub private: Option<bool>,
    pub gated: Option<bool>,
    pub tags: Vec<String>,
    pub license: ModelLicense,
    pub files: Vec<ModelFileView>,
    pub gguf_files: Vec<GgufFileView>,
    pub storage: StorageResponse,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelProviderStatusResponse {
    pub cli: HfCliStatus,
}

#[derive(Debug, Serialize)]
pub(crate) struct ModelFileView {
    pub filename: String,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub lfs: Option<bool>,
    pub installed: bool,
    pub downloading: bool,
    pub download_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct GgufFileView {
    pub filename: String,
    pub size_bytes: Option<u64>,
    pub quantization: Option<String>,
    pub parameter_size: Option<String>,
    pub base_name: Option<String>,
    pub installed: bool,
    pub downloading: bool,
    pub download_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct StorageResponse {
    pub available_bytes: Option<u64>,
    pub destination_available: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchQuery {
    pub q: String,
    pub page: Option<usize>,
    pub page_size: Option<usize>,
    pub gguf_only: Option<bool>,
    pub sort: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ModelQuery {
    pub repo_id: String,
    pub revision: Option<String>,
}

pub(crate) fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/model-provider/search", get(search_models))
        .route("/api/ui/model-provider/details", get(model_details))
        .route("/api/ui/model-provider/status", get(status))
        .route("/api/ui/model-provider/storage", get(storage_info))
        .route("/api/ui/model-provider/downloads", post(start_download))
        .route(
            "/api/ui/model-provider/downloads/:id/cancel",
            post(cancel_download),
        )
        .route(
            "/api/ui/model-provider/downloads/:id/retry",
            post(retry_download),
        )
}

async fn search_models(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(query): Query<SearchQuery>,
) -> Response {
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(20).clamp(1, MAX_SEARCH_PAGE_SIZE);
    let summaries = match state.model_service.provider.search_models(&query.q).await {
        Ok(models) => models,
        Err(error) => return provider_error(error),
    };
    let installed_repos = installed_repo_ids(&state).await;
    let downloading_repos = downloading_repo_ids().await;

    let mut summaries = summaries;
    match query.sort.as_deref().unwrap_or("relevance") {
        "relevance" => {}
        "downloads" => {
            summaries.sort_by_key(|model| std::cmp::Reverse(model.downloads.unwrap_or(0)))
        }
        "recent" => summaries.sort_by_key(|model| {
            std::cmp::Reverse(
                model
                    .last_modified
                    .as_deref()
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok()),
            )
        }),
        _ => {
            return provider_error(HfError::InvalidInput {
                field: "sort".into(),
                message: "ordenação inválida".into(),
            })
        }
    }

    let mut models = Vec::with_capacity(summaries.len());
    for summary in summaries {
        let mut has_gguf = summary
            .tags
            .iter()
            .any(|tag| tag.eq_ignore_ascii_case("gguf"));
        if query.gguf_only.unwrap_or(false) {
            has_gguf = match state
                .model_service
                .provider
                .get_model_info(&summary.repo_id)
                .await
            {
                Ok(info) => !info.gguf_files.is_empty(),
                Err(HfError::HfRepositoryNotFound | HfError::HfAccessDenied) => false,
                Err(error) => return provider_error(error),
            };
            if !has_gguf {
                continue;
            }
        }
        let installed = installed_repos.contains(&summary.repo_id);
        let downloading = downloading_repos.contains(&summary.repo_id);
        models.push(RemoteModelSummary {
            repo_id: summary.repo_id,
            author: summary.author,
            description: summary.description,
            downloads: summary.downloads,
            likes: summary.likes,
            private: summary.private,
            gated: summary.gated,
            tags: summary.tags,
            last_modified: summary.last_modified,
            has_gguf: Some(has_gguf),
            installed,
            downloading,
        });
    }

    let total = models.len();
    let start = page.saturating_sub(1).saturating_mul(page_size);
    let page_models = models.into_iter().skip(start).take(page_size).collect();
    Json(SearchResponse {
        models: page_models,
        total,
        page,
        page_size,
        has_next: start.saturating_add(page_size) < total,
    })
    .into_response()
}

async fn model_details(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(query): Query<ModelQuery>,
) -> Response {
    let repo_id = match validate_repo_id(&query.repo_id) {
        Ok(repo_id) => repo_id,
        Err(error) => return provider_error(error),
    };
    let revision = match query.revision.as_deref().map(validate_revision).transpose() {
        Ok(revision) => revision,
        Err(error) => return provider_error(error),
    };
    let mut info = match state.model_service.provider.get_model_info(&repo_id).await {
        Ok(info) => info,
        Err(error) => return provider_error(error),
    };
    if revision.is_some() {
        let files = match state
            .model_service
            .provider
            .list_files(&repo_id, revision.as_deref())
            .await
        {
            Ok(files) => files,
            Err(error) => return provider_error(error),
        };
        info.files = files.files;
        info.gguf_files = files.gguf_files;
    }
    let license = match model_license(
        &state.model_service.provider,
        &repo_id,
        info.license.clone(),
    )
    .await
    {
        Ok(license) => license,
        Err(error) => return provider_error(error),
    };
    let storage = storage_response(&state).await;
    let files = file_views(&state, &repo_id, revision.as_deref(), &info.files).await;
    let gguf_files = gguf_views(&state, &repo_id, revision.as_deref(), &info.gguf_files).await;
    Json(ModelDetailsResponse {
        repo_id: info.repo_id,
        description: info.description,
        last_modified: info.last_modified,
        revision,
        sha: info.sha,
        downloads: info.downloads,
        likes: info.likes,
        private: info.private,
        gated: info.gated,
        tags: info.tags,
        license,
        files,
        gguf_files,
        storage,
    })
    .into_response()
}

async fn status(auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    consume_connection_auth(auth);
    match provider_status(&state.model_service.provider).await {
        Ok(status) => Json(status).into_response(),
        Err(error) => provider_error(error),
    }
}

fn consume_connection_auth(auth: ConnectionAuth) {
    match auth {
        ConnectionAuth => {}
    }
}

async fn provider_status(
    provider: &DynModelProvider,
) -> Result<ModelProviderStatusResponse, HfError> {
    Ok(ModelProviderStatusResponse {
        cli: provider.check_authentication().await?,
    })
}

async fn model_license(
    provider: &DynModelProvider,
    repo_id: &str,
    fallback: ModelLicense,
) -> Result<ModelLicense, HfError> {
    match provider.get_license(repo_id).await {
        Ok(license) => Ok(license),
        Err(HfError::HfInvalidResponse { .. } | HfError::HfDownloadFailed { .. }) => Ok(fallback),
        Err(error) => Err(error),
    }
}

async fn storage_info(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Json<StorageResponse> {
    Json(storage_response(&state).await)
}

async fn start_download(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(request): Json<DownloadRequest>,
) -> Response {
    let identity = match request.identity() {
        Ok(identity) => identity,
        Err(error) => return provider_error(error),
    };
    let _admission = state.model_service.admission.lock().await;
    if let Some(job) = active_download(&identity).await {
        return conflict(
            "download_in_progress",
            format!("download já em andamento: {}", job.id),
        );
    }
    if let Some(job) = installed_download(&state, &identity).await {
        return conflict(
            "model_installed",
            format!(
                "modelo já instalado: {}",
                job.filename.as_deref().unwrap_or(&identity.filename)
            ),
        );
    }

    let files = match state
        .model_service
        .provider
        .list_files(&identity.repo_id, identity.revision.as_deref())
        .await
    {
        Ok(files) => files,
        Err(error) => return provider_error(error),
    };
    let Some(file) = files
        .files
        .iter()
        .find(|file| file.filename == identity.filename)
        .cloned()
    else {
        return provider_error(HfError::HfRepositoryNotFound);
    };
    if let (Some(size), Some(free)) = (file.size_bytes, disk_free_bytes().await) {
        if free < size {
            return conflict(
                "insufficient_space",
                "espaço insuficiente para baixar este modelo".into(),
            );
        }
    }
    let Some(destination) = state.model_dirs().await.into_iter().next() else {
        return conflict(
            "destination_unavailable",
            "nenhuma pasta de modelos está configurada".into(),
        );
    };
    if let Err(error) = tokio::fs::create_dir_all(&destination).await {
        return provider_error(HfError::Io {
            message: format!("não foi possível preparar a pasta de modelos: {error}"),
        });
    }

    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut job = webui::new_job(
        &id,
        MODEL_DOWNLOAD_KIND,
        format!("{} · {}", identity.repo_id, identity.filename),
        "queued",
    );
    job.repo_id = Some(identity.repo_id.clone());
    job.revision = identity.revision.clone();
    job.filename = Some(identity.filename.clone());
    job.quantization = request.quantization.clone();
    job.download_status = Some("queued");
    job.total_bytes = file.size_bytes;
    webui::jobs().write().await.insert(id.clone(), job);

    let cancel = CancellationToken::new();
    state
        .model_service
        .cancellations
        .write()
        .await
        .insert(id.clone(), cancel.clone());
    let service = state.model_service.clone();
    let job_id = id.clone();
    tokio::spawn(async move {
        run_download(
            service,
            job_id.clone(),
            request,
            destination,
            file.size_bytes,
            cancel,
        )
        .await;
    });

    Json(json!({ "download_id": id })).into_response()
}

async fn cancel_download(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let active = webui::jobs().read().await.get(&id).is_some_and(|job| {
        job.kind == MODEL_DOWNLOAD_KIND
            && job.state == "running"
            && matches!(
                job.download_status,
                Some("queued" | "starting" | "downloading" | "verifying")
            )
    });
    if !active {
        return conflict(
            "download_not_active",
            "download não está em andamento".into(),
        );
    }
    let Some(token) = state
        .model_service
        .cancellations
        .read()
        .await
        .get(&id)
        .cloned()
    else {
        return conflict(
            "download_not_active",
            "download não está em andamento".into(),
        );
    };
    token.cancel();
    Json(json!({ "download_id": id, "status": "cancelling" })).into_response()
}

async fn retry_download(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let job = match webui::jobs().read().await.get(&id).cloned() {
        Some(job) if job.kind == MODEL_DOWNLOAD_KIND => job,
        _ => return not_found("download não encontrado"),
    };
    if job.state == "running" {
        return conflict(
            "download_in_progress",
            "download já está em andamento".into(),
        );
    }
    let Some(repo_id) = job.repo_id else {
        return conflict("download_invalid", "download não pode ser repetido".into());
    };
    let request = DownloadRequest {
        repo_id,
        revision: job.revision,
        filename: job.filename.unwrap_or_default(),
        quantization: job.quantization,
    };
    let body = start_download(ConnectionAuth, State(state), Json(request)).await;
    body
}

async fn run_download(
    service: Arc<ModelService>,
    job_id: String,
    request: DownloadRequest,
    destination: PathBuf,
    known_size: Option<u64>,
    cancel: CancellationToken,
) {
    let permit = tokio::select! {
        permit = service.permits.clone().acquire_owned() => permit.ok(),
        _ = cancel.cancelled() => None,
    };
    let Some(_permit) = permit else {
        webui::set_model_download_progress(
            &job_id,
            "cancelled",
            None,
            None,
            known_size,
            None,
            None,
            None,
            Some("download cancelado".into()),
        )
        .await;
        remove_cancellation(&service, &job_id).await;
        return;
    };
    if cancel.is_cancelled() {
        webui::set_model_download_progress(
            &job_id,
            "cancelled",
            None,
            None,
            known_size,
            None,
            None,
            None,
            Some("download cancelado".into()),
        )
        .await;
        remove_cancellation(&service, &job_id).await;
        return;
    }

    webui::set_model_download_progress(
        &job_id,
        "starting",
        Some(0.0),
        Some(0),
        known_size,
        None,
        None,
        None,
        None,
    )
    .await;
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
    let options = DownloadOptions {
        revision: request.revision.clone(),
        local_dir: Some(destination.clone()),
        filenames: vec![request.filename.clone()],
        ..DownloadOptions::default()
    };
    let provider = service.provider.clone();
    let provider_request = request.clone();
    let provider_cancel = cancel.clone();
    let mut download = Box::pin(provider.download_file(
        &provider_request.repo_id,
        &provider_request.filename,
        options,
        Some(progress_tx),
        provider_cancel,
    ));
    let mut last_progress = (Some(0), known_size, None, None, Some(0.0));
    let result = loop {
        tokio::select! {
            event = progress_rx.recv() => {
                if let Some(event) = event {
                    let status = match event.status {
                        DownloadStatus::Starting => "starting",
                        DownloadStatus::Downloading => "downloading",
                        DownloadStatus::Completed => "verifying",
                        DownloadStatus::Failed => "failed",
                        DownloadStatus::Cancelled => "cancelled",
                    };
                    let total = event.bytes_total.or(known_size);
                    let downloaded = event.bytes_downloaded.or(last_progress.0);
                    let percent = event.percent.or_else(|| match (downloaded, total) {
                        (Some(done), Some(total)) if total > 0 => Some(done as f64 * 100.0 / total as f64),
                        _ => None,
                    });
                    let eta = match (downloaded, total, event.speed_bytes_per_sec) {
                        (Some(done), Some(total), Some(speed)) if speed > 0 && total > done => Some((total - done) / speed),
                        _ => None,
                    };
                    last_progress = (downloaded, total, event.speed_bytes_per_sec, eta, percent);
                    webui::set_model_download_progress(
                        &job_id, status, percent, downloaded, total, event.speed_bytes_per_sec,
                        eta, None, None,
                    ).await;
                }
            }
            answer = &mut download => break answer,
        }
    };

    while progress_rx.try_recv().is_ok() {}
    match result {
        Ok(result) => complete_download(&job_id, &request, &destination, result).await,
        Err(HfError::Cancelled) => {
            webui::set_model_download_progress(
                &job_id,
                "cancelled",
                last_progress.4,
                last_progress.0,
                last_progress.1,
                last_progress.2,
                last_progress.3,
                None,
                Some("download cancelado".into()),
            )
            .await;
        }
        Err(error) => {
            let message = public_error_message(&error);
            webui::set_model_download_progress(
                &job_id,
                "failed",
                last_progress.4,
                last_progress.0,
                last_progress.1,
                last_progress.2,
                last_progress.3,
                None,
                Some(message),
            )
            .await;
            tracing::warn!(
                provider = "model_provider",
                job_id = %job_id,
                error_type = error_code(&error),
                status = "error",
                "download de modelo falhou"
            );
        }
    }
    remove_cancellation(&service, &job_id).await;
}

async fn complete_download(
    job_id: &str,
    request: &DownloadRequest,
    destination: &Path,
    result: DownloadResult,
) {
    if let Err(error) = write_installed_record(destination, request, &result).await {
        let message = public_error_message(&error);
        webui::set_model_download_progress(
            job_id,
            "failed",
            None,
            Some(result.total_bytes),
            Some(result.total_bytes),
            None,
            None,
            None,
            Some(message),
        )
        .await;
        return;
    }
    webui::set_model_download_progress(
        job_id,
        "completed",
        Some(100.0),
        Some(result.total_bytes),
        Some(result.total_bytes),
        (result.duration_ms > 0)
            .then_some(result.total_bytes.saturating_mul(1000) / result.duration_ms),
        None,
        None,
        None,
    )
    .await;
    webui::invalidate_catalog().await;
    tracing::info!(
        provider = "model_provider",
        repo_id = %request.repo_id,
        revision = ?request.revision,
        download_duration_ms = result.duration_ms,
        download_bytes = result.total_bytes,
        status = "ok",
        "download de modelo concluído"
    );
}

async fn write_installed_record(
    destination: &Path,
    request: &DownloadRequest,
    result: &DownloadResult,
) -> Result<(), HfError> {
    let directory = destination.join(".pom-hf");
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| HfError::Io {
            message: format!("não foi possível registrar o modelo: {error}"),
        })?;
    let key = format!(
        "{}\n{:?}\n{}",
        request.repo_id, request.revision, request.filename
    );
    let name = hex::encode(Sha256::digest(key.as_bytes()));
    let path = directory.join(format!("{name}.json"));
    let record = InstalledRecord {
        source: result.manifest.source.clone(),
        repo_id: request.repo_id.clone(),
        revision: request.revision.clone(),
        filename: request.filename.clone(),
        quantization: request.quantization.clone(),
        size_bytes: result.total_bytes,
        sha256: result.files.first().and_then(|file| file.sha256.clone()),
        files: result.manifest.files.clone(),
        downloaded_at: Utc::now().to_rfc3339(),
    };
    let data = serde_json::to_vec_pretty(&record).map_err(|error| HfError::Io {
        message: format!("não foi possível serializar o registro local: {error}"),
    })?;
    tokio::fs::write(path, data)
        .await
        .map_err(|error| HfError::Io {
            message: format!("não foi possível gravar o registro local: {error}"),
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledRecord {
    source: String,
    repo_id: String,
    revision: Option<String>,
    filename: String,
    quantization: Option<String>,
    size_bytes: u64,
    sha256: Option<String>,
    files: Vec<ManifestFile>,
    downloaded_at: String,
}

async fn installed_repo_ids(state: &GatewayState) -> HashSet<String> {
    let directories = state.model_dirs().await;
    tokio::task::spawn_blocking(move || {
        let mut repos = HashSet::new();
        for directory in directories {
            let sidecars = directory.join(".pom-hf");
            let Ok(entries) = std::fs::read_dir(sidecars) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(data) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                if let Ok(record) = serde_json::from_str::<InstalledRecord>(&data) {
                    repos.insert(record.repo_id);
                }
            }
        }
        repos
    })
    .await
    .unwrap_or_default()
}

async fn downloading_repo_ids() -> HashSet<String> {
    webui::jobs()
        .read()
        .await
        .values()
        .filter(|job| {
            job.kind == MODEL_DOWNLOAD_KIND
                && job.state == "running"
                && matches!(
                    job.download_status,
                    Some("queued" | "starting" | "downloading" | "verifying")
                )
        })
        .filter_map(|job| job.repo_id.clone())
        .collect()
}

async fn file_views(
    state: &GatewayState,
    repo_id: &str,
    revision: Option<&str>,
    files: &[ModelFile],
) -> Vec<ModelFileView> {
    files
        .iter()
        .map(|file| async move {
            let download = download_for(repo_id, revision, &file.filename).await;
            ModelFileView {
                filename: file.filename.clone(),
                size_bytes: file.size_bytes,
                sha256: file.sha256.clone(),
                lfs: file.lfs,
                installed: is_installed_file(state, repo_id, revision, &file.filename).await,
                downloading: download.is_some(),
                download_id: download.map(|job| job.id),
            }
        })
        .collect::<futures::future::JoinAll<_>>()
        .await
}

async fn gguf_views(
    state: &GatewayState,
    repo_id: &str,
    revision: Option<&str>,
    files: &[GgufFile],
) -> Vec<GgufFileView> {
    files
        .iter()
        .map(|file| async move {
            let download = download_for(repo_id, revision, &file.filename).await;
            GgufFileView {
                filename: file.filename.clone(),
                size_bytes: file.size_bytes,
                quantization: file.quantization.clone(),
                parameter_size: file.parameter_size.clone(),
                base_name: file.base_name.clone(),
                installed: is_installed_file(state, repo_id, revision, &file.filename).await,
                downloading: download.is_some(),
                download_id: download.map(|job| job.id),
            }
        })
        .collect::<futures::future::JoinAll<_>>()
        .await
}

async fn storage_response(state: &GatewayState) -> StorageResponse {
    StorageResponse {
        available_bytes: disk_free_bytes().await,
        destination_available: state.model_dirs().await.into_iter().next().is_some(),
    }
}

async fn disk_free_bytes() -> Option<u64> {
    let free = crate::sysmon::sample().await.disk.free_mb;
    (free > 0).then_some(free.saturating_mul(1_048_576))
}

async fn active_download(identity: &DownloadIdentity) -> Option<Job> {
    webui::jobs()
        .read()
        .await
        .values()
        .find(|job| {
            job.kind == MODEL_DOWNLOAD_KIND
                && job.state == "running"
                && matches!(
                    job.download_status,
                    Some("queued" | "starting" | "downloading" | "verifying")
                )
                && job.repo_id.as_deref() == Some(identity.repo_id.as_str())
                && job.revision.as_deref() == identity.revision.as_deref()
                && job.filename.as_deref() == Some(identity.filename.as_str())
        })
        .cloned()
}

async fn installed_download(state: &GatewayState, identity: &DownloadIdentity) -> Option<Job> {
    is_installed_file(
        state,
        &identity.repo_id,
        identity.revision.as_deref(),
        &identity.filename,
    )
    .await
    .then(|| Job {
        id: "installed".into(),
        state: "done",
        kind: MODEL_DOWNLOAD_KIND,
        title: identity.filename.clone(),
        step: "completed".into(),
        message: String::new(),
        progress: Some(100.0),
        outputs: Vec::new(),
        started_at: String::new(),
        finished_at: None,
        repo_id: Some(identity.repo_id.clone()),
        revision: identity.revision.clone(),
        filename: Some(identity.filename.clone()),
        quantization: None,
        download_status: Some("completed"),
        bytes_downloaded: None,
        total_bytes: None,
        speed_bytes_per_sec: None,
        eta_seconds: None,
        local_path: None,
        error: None,
    })
}

async fn download_for(repo_id: &str, revision: Option<&str>, filename: &str) -> Option<Job> {
    let jobs = webui::jobs().read().await;
    jobs.values()
        .find(|job| {
            job.kind == MODEL_DOWNLOAD_KIND
                && job.state == "running"
                && job.repo_id.as_deref() == Some(repo_id)
                && job.revision.as_deref() == revision
                && job.filename.as_deref() == Some(filename)
        })
        .cloned()
}

async fn is_installed_file(
    state: &GatewayState,
    repo_id: &str,
    revision: Option<&str>,
    filename: &str,
) -> bool {
    let directories = state.model_dirs().await;
    let filename = filename.to_string();
    let key = format!("{}\n{:?}\n{}", repo_id, revision, filename);
    let sidecar = hex::encode(Sha256::digest(key.as_bytes()));
    tokio::task::spawn_blocking(move || {
        directories.into_iter().any(|directory| {
            directory.join(&filename).is_file()
                && directory
                    .join(".pom-hf")
                    .join(format!("{sidecar}.json"))
                    .is_file()
        })
    })
    .await
    .unwrap_or(false)
}

async fn remove_cancellation(service: &ModelService, id: &str) {
    service.cancellations.write().await.remove(id);
}

fn provider_error(error: HfError) -> Response {
    let (status, code) = match error {
        HfError::HfCliNotInstalled => (StatusCode::SERVICE_UNAVAILABLE, "cli_not_installed"),
        HfError::HfAuthenticationRequired => (StatusCode::UNAUTHORIZED, "authentication_required"),
        HfError::HfRepositoryNotFound => (StatusCode::NOT_FOUND, "repository_not_found"),
        HfError::HfAccessDenied => (StatusCode::FORBIDDEN, "access_denied"),
        HfError::HfRateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        HfError::InvalidInput { .. } => (StatusCode::BAD_REQUEST, "invalid_input"),
        HfError::Cancelled => (StatusCode::CONFLICT, "cancelled"),
        HfError::HfDownloadFailed { .. } => (StatusCode::BAD_GATEWAY, "download_failed"),
        HfError::HfInvalidResponse { .. } => (StatusCode::BAD_GATEWAY, "invalid_response"),
        HfError::ProcessFailed { .. } => (StatusCode::BAD_GATEWAY, "process_failed"),
        HfError::Io { .. } => (StatusCode::INTERNAL_SERVER_ERROR, "io_error"),
    };
    let message = public_error_message_code(code);
    (
        status,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

fn conflict(code: &'static str, message: String) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

fn not_found(message: &'static str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "code": "not_found", "message": message } })),
    )
        .into_response()
}

fn error_code(error: &HfError) -> &'static str {
    match error {
        HfError::HfCliNotInstalled => "cli_not_installed",
        HfError::HfAuthenticationRequired => "authentication_required",
        HfError::HfRepositoryNotFound => "repository_not_found",
        HfError::HfAccessDenied => "access_denied",
        HfError::HfRateLimited => "rate_limited",
        HfError::HfDownloadFailed { .. } => "download_failed",
        HfError::HfInvalidResponse { .. } => "invalid_response",
        HfError::InvalidInput { .. } => "invalid_input",
        HfError::ProcessFailed { .. } => "process_failed",
        HfError::Io { .. } => "io_error",
        HfError::Cancelled => "cancelled",
    }
}

fn public_error_message(error: &HfError) -> String {
    public_error_message_code(error_code(error)).to_string()
}

fn public_error_message_code(code: &str) -> &'static str {
    match code {
        "cli_not_installed" => "o suporte ao provedor não está instalado neste nó",
        "authentication_required" => "autenticação do provedor necessária",
        "repository_not_found" => "modelo não encontrado",
        "access_denied" => "você não possui acesso a este modelo",
        "rate_limited" => "o provedor recusou a requisição por limite de uso",
        "insufficient_space" => "espaço insuficiente para baixar este modelo",
        "cancelled" => "download cancelado",
        "download_in_progress" => "este modelo já está sendo baixado",
        "model_installed" => "este modelo já está instalado",
        "destination_unavailable" => "nenhuma pasta de modelos está disponível",
        _ => "não foi possível concluir a operação do modelo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_provider::{
        DownloadOptions, DownloadResult, HfCliStatus, ModelFiles, ModelInfo, ModelProvider,
        ModelSummary, ProgressSender,
    };
    use async_trait::async_trait;
    use axum::{body::Body, http::Request};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    struct TestProvider {
        license: ModelLicense,
        license_calls: Arc<AtomicUsize>,
        status: HfCliStatus,
    }

    fn unreachable_provider_call<T>(value: T) -> ! {
        std::mem::drop(value);
        unreachable!("não faz parte deste teste")
    }

    #[async_trait]
    impl ModelProvider for TestProvider {
        async fn search_models(&self, query: &str) -> Result<Vec<ModelSummary>, HfError> {
            unreachable_provider_call(query)
        }

        async fn get_model_info(&self, repo_id: &str) -> Result<ModelInfo, HfError> {
            unreachable_provider_call(repo_id)
        }

        async fn list_files(
            &self,
            repo_id: &str,
            revision: Option<&str>,
        ) -> Result<ModelFiles, HfError> {
            unreachable_provider_call((repo_id, revision))
        }

        async fn download_file(
            &self,
            repo_id: &str,
            filename: &str,
            options: DownloadOptions,
            progress: Option<ProgressSender>,
            cancel: CancellationToken,
        ) -> Result<DownloadResult, HfError> {
            unreachable_provider_call((repo_id, filename, options, progress, cancel))
        }

        async fn download_model(
            &self,
            repo_id: &str,
            options: DownloadOptions,
            progress: Option<ProgressSender>,
            cancel: CancellationToken,
        ) -> Result<DownloadResult, HfError> {
            unreachable_provider_call((repo_id, options, progress, cancel))
        }

        async fn get_license(&self, repo_id: &str) -> Result<ModelLicense, HfError> {
            assert!(!repo_id.is_empty());
            self.license_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.license.clone())
        }

        async fn check_authentication(&self) -> Result<HfCliStatus, HfError> {
            Ok(self.status.clone())
        }
    }

    fn test_provider() -> (DynModelProvider, Arc<AtomicUsize>) {
        let license_calls = Arc::new(AtomicUsize::new(0));
        let provider = TestProvider {
            license: ModelLicense {
                id: Some("apache-2.0".into()),
                name: Some("Apache License 2.0".into()),
                url: Some("https://www.apache.org/licenses/LICENSE-2.0".into()),
                raw_value: Some("apache-2.0".into()),
                detected: true,
            },
            license_calls: license_calls.clone(),
            status: HfCliStatus::not_installed(),
        };
        (Arc::new(provider), license_calls)
    }

    #[tokio::test]
    async fn status_endpoint_reads_structured_cli_state() {
        let (provider, _) = test_provider();

        let response = provider_status(&provider).await.unwrap();

        assert!(!response.cli.installed);
        assert!(!response.cli.authenticated);
        assert_eq!(response.cli.version, None);
    }

    #[tokio::test]
    async fn status_route_requires_auth_and_serializes_cli_state() {
        let (provider, _) = test_provider();
        let mut state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.model_service = Arc::new(ModelService::new(provider));
        let app = routes().with_state(state);

        let missing = app
            .clone()
            .oneshot(
                Request::get("/api/ui/model-provider/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(
                Request::get("/api/ui/model-provider/status")
                    .header("authorization", "Bearer chave")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["cli"]["installed"], false);
        assert_eq!(body["cli"]["authenticated"], false);
    }

    #[tokio::test]
    async fn details_license_uses_provider_license_contract() {
        let (provider, license_calls) = test_provider();

        let license = model_license(&provider, "owner/model", ModelLicense::unknown())
            .await
            .unwrap();

        assert_eq!(license.id.as_deref(), Some("apache-2.0"));
        assert_eq!(license_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn structured_download_identity_rejects_traversal() {
        let valid = DownloadRequest {
            repo_id: "Qwen/model".into(),
            revision: Some("refs/main".into()),
            filename: "Qwen3-Q5_K_M.gguf".into(),
            quantization: Some("Q5_K_M".into()),
        };

        assert_eq!(valid.identity().unwrap().repo_id, "Qwen/model");
        assert!(DownloadRequest {
            repo_id: "Qwen/../model".into(),
            ..valid.clone()
        }
        .identity()
        .is_err());
        assert!(DownloadRequest {
            filename: "../model.gguf".into(),
            ..valid
        }
        .identity()
        .is_err());
    }

    #[test]
    fn maps_provider_errors_to_safe_user_messages() {
        let error = HfError::HfDownloadFailed {
            message: "Authorization: Bearer hf_secret /private/model".into(),
        };

        let message = public_error_message(&error);
        assert!(!message.contains("hf_secret"));
        assert!(!message.contains("Authorization"));
        assert!(!message.contains("/private/model"));
        assert!(message.contains("operação") || message.contains("concluir"));
    }

    #[test]
    fn serialized_download_job_does_not_keep_provider_details() {
        let mut job = webui::new_job("job-1", MODEL_DOWNLOAD_KIND, "Qwen/model", "failed");
        job.error = Some(public_error_message(&HfError::HfDownloadFailed {
            message: "hf_secret /private/model".into(),
        }));
        let serialized = serde_json::to_string(&job).unwrap();
        assert!(!serialized.contains("hf_secret"));
        assert!(!serialized.contains("/private/model"));
        assert!(!serialized.contains("--local-dir"));
    }

    #[test]
    fn model_download_job_is_not_reported_as_a_slice() {
        let job = webui::new_job("job-1", MODEL_DOWNLOAD_KIND, "Qwen/model", "queued");
        assert_eq!(job.kind, MODEL_DOWNLOAD_KIND);
        assert_eq!(job.download_status, None);
    }

    #[test]
    fn page_size_is_bounded() {
        assert_eq!(Some(1000usize).unwrap().clamp(1, MAX_SEARCH_PAGE_SIZE), 50);
    }
}
