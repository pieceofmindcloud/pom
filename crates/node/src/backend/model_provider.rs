//! API and jobs for remote models.
//!
//! This layer knows only `ModelProvider`. The default provider is created at
//! node bootstrap; the module does not build commands, read tokens or talk to
//! Hugging Face directly.

use crate::auth::ConnectionAuth;
use crate::gateway::GatewayState;
use crate::huggingface::HfTokenStore;
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
    pub(crate) hf_token_store: HfTokenStore,
    permits: Arc<Semaphore>,
    admission: Arc<Mutex<()>>,
    cancellations: Arc<RwLock<HashMap<String, CancellationToken>>>,
}

impl ModelService {
    #[cfg(test)]
    pub(crate) fn new(provider: DynModelProvider) -> Self {
        Self::with_token_store(provider, HfTokenStore::default())
    }

    pub(crate) fn with_token_store(
        provider: DynModelProvider,
        hf_token_store: HfTokenStore,
    ) -> Self {
        Self {
            provider,
            hf_token_store,
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
    /// The machine that downloads, when it is not this one. The admin's node
    /// forwards the call, signed, and the worker downloads with its own
    /// Hugging Face token into its own models folder. Absent or this node's
    /// id: a local download, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Other files of the same repository fetched by the same action: the
    /// other parts of a split GGUF, a vision projector, an MTP draft. Each
    /// becomes its own download row; one already installed or downloading
    /// is skipped, not an error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_files: Vec<String>,
    /// The models folder to download into: one of this node's configured
    /// folders, exactly as `storage.destinations` lists it. Absent: the first
    /// configured folder, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

/// `?node_id=` on the download routes that name a download by id, and on the
/// list: the machine whose downloads are meant.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct NodeQuery {
    pub node_id: Option<String>,
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
                message: "only GGUF files can be added to the catalog".into(),
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
                    message: "quantization contains invalid characters".into(),
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
pub(crate) struct HuggingFaceTokenConfigResponse {
    pub configured: bool,
    pub masked: Option<String>,
    pub source: &'static str,
}

#[derive(Deserialize)]
pub(crate) struct HuggingFaceTokenUpdate {
    pub token: Option<String>,
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
    /// Free space of the default (first) models folder.
    pub available_bytes: Option<u64>,
    pub destination_available: bool,
    pub destination_paths: Vec<String>,
    /// Every configured models folder with the free space of its own disk.
    pub destinations: Vec<DestinationView>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DestinationView {
    pub path: String,
    pub available_bytes: Option<u64>,
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
        .route(
            "/api/ui/model-provider/config",
            get(token_config).post(save_token_config),
        )
        .route("/api/ui/model-provider/storage", get(storage_info))
        .route(
            "/api/ui/model-provider/downloads",
            get(list_downloads).post(start_download),
        )
        .route(
            "/api/ui/model-provider/downloads/:id/cancel",
            post(cancel_download),
        )
        .route(
            "/api/ui/model-provider/downloads/:id/retry",
            post(retry_download),
        )
        .route(
            "/api/ui/model-provider/downloads/:id/dismiss",
            post(dismiss_download),
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
                message: "invalid sort order".into(),
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

async fn token_config(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    match state.model_service.hf_token_store.status() {
        Ok(status) => Json(token_config_response(status)).into_response(),
        Err(error) => provider_error(error),
    }
}

async fn save_token_config(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(update): Json<HuggingFaceTokenUpdate>,
) -> Response {
    let token = update
        .token
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty());
    if let Err(error) = state.database.save_huggingface_token(token.as_deref()) {
        tracing::error!("could not store the Hugging Face token: {error:#}");
        return provider_error(HfError::Io {
            message: "could not store the Hugging Face token".into(),
        });
    }
    {
        let mut config = state.auth.write().await;
        config.huggingface_token = token.clone();
    }

    // The provider reads this shared store for every request, so the next
    // status check and download use the saved credential without a restart.
    state.model_service.hf_token_store.set(token);
    match state.model_service.hf_token_store.status() {
        Ok(status) => Json(token_config_response(status)).into_response(),
        Err(error) => provider_error(error),
    }
}

fn token_config_response(
    status: crate::huggingface::HfTokenStatus,
) -> HuggingFaceTokenConfigResponse {
    HuggingFaceTokenConfigResponse {
        configured: status.configured,
        masked: status.masked,
        source: status.source.label(),
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

/// How long the admin's node waits on a forwarded download call. Starting a
/// download lists the repository's files on the worker first.
const FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The other machine a call names, or `None` when it is meant for this node
/// (no id, or this node's own).
fn remote_target<'a>(state: &GatewayState, node_id: Option<&'a str>) -> Option<&'a str> {
    node_id
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != state.network.node_id)
}

/// Sends a model call to the machine that must carry it out, signed by this
/// node (`GatewayState::control_post_json` / `control_get`, over the tunnel
/// when no address answers), and hands its answer back as it came. The
/// worker authorizes the signature by its own route policy: an admin of the
/// chain may ask, a visitor never holds models.
pub(crate) async fn forward_to_node(
    state: &GatewayState,
    node_id: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Response {
    let Some(peer) = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == node_id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": {
                "code": "node_not_found",
                "message": "that machine is not in the network",
            } })),
        )
            .into_response();
    };
    let sent = if method == reqwest::Method::GET {
        state.control_get(&peer, path, FORWARD_TIMEOUT).await
    } else {
        let body = body.unwrap_or_else(|| json!({}));
        state
            .control_post_json(&peer, path, &body, FORWARD_TIMEOUT)
            .await
    };
    let response = match sent {
        Ok(response) => response,
        Err(error) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": {
                    "code": "node_unreachable",
                    "message": format!("could not reach that machine: {error:#}"),
                } })),
            )
                .into_response();
        }
    };
    let status = StatusCode::from_u16(response.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = response.bytes().await.unwrap_or_default();
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response()
}

/// This machine's model downloads, newest first; with `?node_id=` another
/// machine's, so the admin follows a download it started on a worker.
async fn list_downloads(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(query): Query<NodeQuery>,
) -> Response {
    if let Some(node_id) = remote_target(&state, query.node_id.as_deref()) {
        return forward_to_node(
            &state,
            node_id,
            reqwest::Method::GET,
            "/api/ui/model-provider/downloads",
            None,
        )
        .await;
    }
    let mut list: Vec<Job> = webui::jobs()
        .read()
        .await
        .values()
        .filter(|job| job.kind == MODEL_DOWNLOAD_KIND)
        .cloned()
        .collect();
    list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Json(json!({ "node_id": state.network.node_id, "downloads": list })).into_response()
}

async fn start_download(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(mut request): Json<DownloadRequest>,
) -> Response {
    if let Some(node_id) = remote_target(&state, request.node_id.as_deref()).map(str::to_owned) {
        request.node_id = None;
        return forward_to_node(
            &state,
            &node_id,
            reqwest::Method::POST,
            "/api/ui/model-provider/downloads",
            serde_json::to_value(&request).ok(),
        )
        .await;
    }
    request.node_id = None;
    let extra_files = std::mem::take(&mut request.extra_files);
    let main = start_one(state.clone(), request.clone()).await;
    if !main.status().is_success() || extra_files.is_empty() {
        return main;
    }
    let body = axum::body::to_bytes(main.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_default();
    let mut extra_ids = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut seen = HashSet::from([request.filename.clone()]);
    for filename in extra_files {
        if !seen.insert(filename.clone()) {
            continue;
        }
        let extra = DownloadRequest {
            filename: filename.clone(),
            quantization: None,
            ..request.clone()
        };
        let response = start_one(state.clone(), extra).await;
        let status = response.status();
        let answer = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .unwrap_or_default();
        if status.is_success() {
            extra_ids.push(answer["download_id"].clone());
            continue;
        }
        let code = answer["error"]["code"].as_str().unwrap_or("download_failed").to_string();
        if matches!(code.as_str(), "model_installed" | "download_in_progress") {
            skipped.push(json!({ "filename": filename, "code": code }));
        } else {
            failed.push(json!({ "filename": filename, "code": code }));
        }
    }
    Json(json!({
        "download_id": body["download_id"],
        "extra_download_ids": extra_ids,
        "skipped": skipped,
        "failed": failed,
    }))
    .into_response()
}

/// Starts one file's download on this machine: the checks, the job row and
/// the background task.
async fn start_one(state: GatewayState, request: DownloadRequest) -> Response {
    let identity = match request.identity() {
        Ok(identity) => identity,
        Err(error) => return provider_error(error),
    };
    let _admission = state.model_service.admission.lock().await;
    if let Some(job) = active_download(&identity).await {
        return conflict(
            "download_in_progress",
            format!("download already in progress: {}", job.id),
        );
    }
    if let Some(job) = installed_download(&state, &identity).await {
        return conflict(
            "model_installed",
            format!(
                "model already installed: {}",
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
    let destinations = configured_model_destinations(state.model_dirs().await);
    let Some(destination) =
        requested_model_destination(&destinations, request.destination.as_deref())
    else {
        return conflict(
            "destination_unavailable",
            "the models folder is not configured on this machine".into(),
        );
    };
    if let (Some(size), Some(free)) = (file.size_bytes, folder_free_bytes(&destination).await) {
        if free < size {
            return conflict(
                "insufficient_space",
                "not enough space to download this model".into(),
            );
        }
    }
    if let Err(error) = tokio::fs::create_dir_all(&destination).await {
        return provider_error(HfError::Io {
            message: format!("could not prepare the models folder: {error}"),
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
    job.local_path = Some(
        destination
            .join(&identity.filename)
            .to_string_lossy()
            .into_owned(),
    );
    webui::jobs().write().await.insert(id.clone(), job);

    let cancel = CancellationToken::new();
    state
        .model_service
        .cancellations
        .write()
        .await
        .insert(id.clone(), cancel.clone());
    let service = state.model_service.clone();
    let database = state.database.clone();
    let job_id = id.clone();
    tokio::spawn(async move {
        run_download(
            service,
            database,
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
    Query(query): Query<NodeQuery>,
) -> Response {
    if let Some(node_id) = remote_target(&state, query.node_id.as_deref()) {
        return forward_to_node(
            &state,
            node_id,
            reqwest::Method::POST,
            &download_path(&id, "cancel"),
            None,
        )
        .await;
    }
    let active = webui::jobs().read().await.get(&id).is_some_and(|job| {
        job.kind == MODEL_DOWNLOAD_KIND
            && job.state == "running"
            && matches!(
                job.download_status,
                Some("queued" | "starting" | "downloading" | "verifying")
            )
    });
    if !active {
        return conflict("download_not_active", "download is not in progress".into());
    }
    let Some(token) = state
        .model_service
        .cancellations
        .read()
        .await
        .get(&id)
        .cloned()
    else {
        return conflict("download_not_active", "download is not in progress".into());
    };
    token.cancel();
    // A download cancelled before it finished leaves the list at once: a
    // cancelled row had only "Try again", so it could never be dismissed. The
    // task still winding down updates nothing, since progress only touches a
    // listed job (`webui::set_model_download_progress`).
    webui::jobs().write().await.remove(&id);
    Json(json!({ "download_id": id, "status": "cancelling" })).into_response()
}

async fn retry_download(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<NodeQuery>,
) -> Response {
    if let Some(node_id) = remote_target(&state, query.node_id.as_deref()) {
        return forward_to_node(
            &state,
            node_id,
            reqwest::Method::POST,
            &download_path(&id, "retry"),
            None,
        )
        .await;
    }
    let job = match webui::jobs().read().await.get(&id).cloned() {
        Some(job) if job.kind == MODEL_DOWNLOAD_KIND => job,
        _ => return not_found("download not found"),
    };
    if job.state == "running" {
        return conflict(
            "download_in_progress",
            "download is already in progress".into(),
        );
    }
    let Some(repo_id) = job.repo_id else {
        return conflict("download_invalid", "download cannot be retried".into());
    };
    let filename = job.filename.unwrap_or_default();
    // The new attempt goes to the folder of the failed one.
    let destination = job
        .local_path
        .as_deref()
        .and_then(|path| path.strip_suffix(filename.as_str()))
        .map(|folder| folder.trim_end_matches(['/', '\\']).to_string())
        .filter(|folder| !folder.is_empty());
    let request = DownloadRequest {
        repo_id,
        revision: job.revision,
        filename,
        quantization: job.quantization,
        node_id: None,
        extra_files: Vec::new(),
        destination,
    };
    let response = start_download(ConnectionAuth, State(state), Json(request)).await;
    // The new attempt replaces the old row: "Try again" used to add a second
    // entry and leave the failed one behind, with no way to dismiss it.
    if response.status().is_success() {
        webui::jobs().write().await.remove(&id);
    }
    response
}

/// Takes a download that ended without completing (failed or cancelled) off
/// the list. A running one is refused: that is what Cancel is for.
async fn dismiss_download(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<NodeQuery>,
) -> Response {
    if let Some(node_id) = remote_target(&state, query.node_id.as_deref()) {
        return forward_to_node(
            &state,
            node_id,
            reqwest::Method::POST,
            &download_path(&id, "dismiss"),
            None,
        )
        .await;
    }
    let mut jobs = webui::jobs().write().await;
    match jobs.get(&id) {
        Some(job) if job.kind == MODEL_DOWNLOAD_KIND => {
            if job.state == "running" {
                return conflict(
                    "download_in_progress",
                    "download is still in progress; cancel it instead".into(),
                );
            }
            jobs.remove(&id);
            Json(json!({ "download_id": id, "dismissed": true })).into_response()
        }
        _ => not_found("download not found"),
    }
}

/// The path of an action on one download, as the worker serves it. The id
/// is checked so a forwarded call cannot reach another route.
fn download_path(id: &str, action: &str) -> String {
    let id: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    format!("/api/ui/model-provider/downloads/{id}/{action}")
}

async fn run_download(
    service: Arc<ModelService>,
    database: Arc<crate::db::PomDb>,
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
            Some("download cancelled".into()),
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
            Some("download cancelled".into()),
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
        Ok(result) => complete_download(&database, &job_id, &request, &destination, result).await,
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
                Some("download cancelled".into()),
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
                "model download failed"
            );
        }
    }
    remove_cancellation(&service, &job_id).await;
}

async fn complete_download(
    database: &crate::db::PomDb,
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
        result
            .local_paths
            .first()
            .map(|path| path.to_string_lossy().into_owned()),
        None,
    )
    .await;
    webui::invalidate_catalog(Arc::new(database.clone())).await;
    tracing::info!(
        provider = "model_provider",
        repo_id = %request.repo_id,
        revision = ?request.revision,
        download_duration_ms = result.duration_ms,
        download_bytes = result.total_bytes,
        status = "ok",
        "model download completed"
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
            message: format!("could not register the model: {error}"),
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
        message: format!("could not serialize the local record: {error}"),
    })?;
    tokio::fs::write(path, data)
        .await
        .map_err(|error| HfError::Io {
            message: format!("could not write the local record: {error}"),
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

/// Where each model downloaded through POM came from, read from the
/// records the download leaves in `<models folder>/.pom-hf/`:
/// `{ path, repo_id, revision, filename }`, `path` being the file in that
/// folder. The catalogs carry it (`hf_sources`) so the deploy screen can ask
/// a machine that lacks a model to download the same repository and file. A
/// file placed by hand has no record and no entry.
pub(crate) async fn installed_sources(directories: Vec<PathBuf>) -> Vec<serde_json::Value> {
    tokio::task::spawn_blocking(move || {
        let mut sources = Vec::new();
        for directory in directories {
            let Ok(entries) = std::fs::read_dir(directory.join(".pom-hf")) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(data) = std::fs::read_to_string(entry.path()) else {
                    continue;
                };
                let Ok(record) = serde_json::from_str::<InstalledRecord>(&data) else {
                    continue;
                };
                let path = directory.join(&record.filename);
                if !path.is_file() {
                    continue;
                }
                sources.push(json!({
                    "path": path.to_string_lossy(),
                    "repo_id": record.repo_id,
                    "revision": record.revision,
                    "filename": record.filename,
                }));
            }
        }
        sources
    })
    .await
    .unwrap_or_default()
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
    let destinations = configured_model_destinations(state.model_dirs().await);
    let mut views = Vec::with_capacity(destinations.len());
    for path in &destinations {
        views.push(DestinationView {
            path: path.to_string_lossy().into_owned(),
            available_bytes: folder_free_bytes(path).await,
        });
    }
    StorageResponse {
        available_bytes: match views.first() {
            Some(first) => first.available_bytes,
            None => disk_free_bytes().await,
        },
        destination_available: configured_model_destination(&destinations).is_some(),
        destination_paths: views.iter().map(|view| view.path.clone()).collect(),
        destinations: views,
    }
}

/// The folder a download goes to: the requested one when it is exactly one
/// of the configured folders (a caller cannot name any other path), or the
/// first configured folder when none is requested.
fn requested_model_destination(
    directories: &[PathBuf],
    requested: Option<&str>,
) -> Option<PathBuf> {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        None => configured_model_destination(directories),
        Some(requested) => directories
            .iter()
            .find(|path| !path.as_os_str().is_empty() && path.to_string_lossy() == requested)
            .cloned(),
    }
}

/// Free space of the disk that holds `folder` (its nearest existing parent
/// when it does not exist yet).
async fn folder_free_bytes(folder: &std::path::Path) -> Option<u64> {
    let folder = folder.to_path_buf();
    let free = tokio::task::spawn_blocking(move || crate::sysmon::sample_disk_at(&folder).free_mb)
        .await
        .ok()?;
    (free > 0).then_some(free.saturating_mul(1_048_576))
}

fn configured_model_destinations(directories: Vec<PathBuf>) -> Vec<PathBuf> {
    directories
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

fn configured_model_destination(directories: &[PathBuf]) -> Option<PathBuf> {
    directories
        .iter()
        .find(|path| !path.as_os_str().is_empty())
        .cloned()
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
        bytes_processed: None,
        current_file: None,
        phase_started_at: String::new(),
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
        deploy_id: None,
        deployment_name: None,
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
        HfError::HfAuthenticationRequired => (StatusCode::UNAUTHORIZED, "authentication_required"),
        HfError::HfRepositoryNotFound => (StatusCode::NOT_FOUND, "repository_not_found"),
        HfError::HfFileNotFound => (StatusCode::NOT_FOUND, "file_not_found"),
        HfError::HfAccessDenied => (StatusCode::FORBIDDEN, "access_denied"),
        HfError::HfRateLimited => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        HfError::InvalidInput { .. } => (StatusCode::BAD_REQUEST, "invalid_input"),
        HfError::Cancelled => (StatusCode::CONFLICT, "cancelled"),
        HfError::HfDownloadFailed { .. } => (StatusCode::BAD_GATEWAY, "download_failed"),
        HfError::HfInvalidResponse { .. } => (StatusCode::BAD_GATEWAY, "invalid_response"),
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
        HfError::HfAuthenticationRequired => "authentication_required",
        HfError::HfRepositoryNotFound => "repository_not_found",
        HfError::HfFileNotFound => "file_not_found",
        HfError::HfAccessDenied => "access_denied",
        HfError::HfRateLimited => "rate_limited",
        HfError::HfDownloadFailed { .. } => "download_failed",
        HfError::HfInvalidResponse { .. } => "invalid_response",
        HfError::InvalidInput { .. } => "invalid_input",
        HfError::Io { .. } => "io_error",
        HfError::Cancelled => "cancelled",
    }
}

fn public_error_message(error: &HfError) -> String {
    public_error_message_code(error_code(error)).to_string()
}

fn public_error_message_code(code: &str) -> &'static str {
    match code {
        "authentication_required" => "provider authentication required",
        "repository_not_found" => "model not found",
        "file_not_found" => "model file not found",
        "access_denied" => "you do not have access to this model",
        "rate_limited" => "the provider refused the request because of a usage limit",
        "insufficient_space" => "not enough space to download this model",
        "cancelled" => "download cancelled",
        "download_in_progress" => "this model is already being downloaded",
        "model_installed" => "this model is already installed",
        "destination_unavailable" => "no models folder is available",
        "download_failed" => "could not complete the download; check the connection and try again",
        "invalid_response" => {
            "Hugging Face did not answer correctly; check the connection and try again"
        }
        _ => "could not complete the model operation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_provider::{
        DownloadOptions, DownloadResult, GgufFile, HfCliStatus, ModelFile, ModelFiles, ModelInfo,
        ModelProvider, ModelSummary, ProgressSender,
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
        unreachable!("not part of this test")
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

    struct DestinationProvider;

    #[async_trait]
    impl ModelProvider for DestinationProvider {
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
            Ok(ModelFiles {
                repo_id: repo_id.to_string(),
                revision: revision.map(str::to_string),
                files: vec![ModelFile {
                    filename: "model.gguf".into(),
                    size_bytes: Some(1),
                    sha256: None,
                    lfs: None,
                }],
                gguf_files: vec![GgufFile {
                    filename: "model.gguf".into(),
                    size_bytes: Some(1),
                    quantization: None,
                    parameter_size: None,
                    base_name: None,
                }],
            })
        }

        async fn download_file(
            &self,
            _repo_id: &str,
            _filename: &str,
            _options: DownloadOptions,
            _progress: Option<ProgressSender>,
            _cancel: CancellationToken,
        ) -> Result<DownloadResult, HfError> {
            Err(HfError::Cancelled)
        }

        async fn download_model(
            &self,
            repo_id: &str,
            options: DownloadOptions,
            progress: Option<ProgressSender>,
            cancel: CancellationToken,
        ) -> Result<DownloadResult, HfError> {
            self.download_file(repo_id, "model.gguf", options, progress, cancel)
                .await
        }

        async fn get_license(&self, repo_id: &str) -> Result<ModelLicense, HfError> {
            unreachable_provider_call(repo_id)
        }

        async fn check_authentication(&self) -> Result<HfCliStatus, HfError> {
            Ok(HfCliStatus::not_installed())
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
    async fn token_config_route_returns_only_masked_state() {
        let (provider, _) = test_provider();
        let store = crate::huggingface::HfTokenStore::new(Some("hf_super_secret".into()));
        let mut state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.model_service = Arc::new(ModelService::with_token_store(provider, store));
        let app = routes().with_state(state);

        let response = app
            .oneshot(
                Request::get("/api/ui/model-provider/config")
                    .header("authorization", "Bearer chave")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        let serialized = body.to_string();
        assert!(!serialized.contains("hf_super_secret"));
        assert_eq!(body["configured"], true);
        assert_eq!(body["source"], "pom.json");
        assert!(body["masked"].as_str().unwrap().contains("••••"));
        assert!(body.get("token").is_none());
    }

    #[tokio::test]
    async fn token_config_route_persists_only_in_database_and_never_returns_secret() {
        let (provider, _) = test_provider();
        let mut state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let store = crate::huggingface::HfTokenStore::new_from_database(None);
        state.model_service = Arc::new(ModelService::with_token_store(provider, store));
        let database = state.database.clone();
        let app = routes().with_state(state);

        let response = app
            .oneshot(
                Request::post("/api/ui/model-provider/config")
                    .header("authorization", "Bearer chave")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"token":"hf_route_secret"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert!(!body.to_string().contains("hf_route_secret"));
        assert!(body.get("token").is_none());
        assert_eq!(
            database.huggingface_token().unwrap().as_deref(),
            Some("hf_route_secret")
        );
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

    /// "Try again" replaces the failed row instead of adding a second one
    /// beside it: the failed row had no other button and stayed for good.
    #[tokio::test]
    async fn retrying_a_failed_download_replaces_its_row() {
        let destination = std::env::temp_dir().join(format!(
            "pom-model-retry-{}",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(&destination).await.unwrap();
        let mut state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.auth.write().await.model_dirs =
            Some(vec![destination.to_string_lossy().into_owned()]);
        state.model_service = Arc::new(ModelService::new(Arc::new(DestinationProvider)));
        let repo = format!("owner/retry-{}", uuid::Uuid::new_v4().simple());
        let old_id = uuid::Uuid::new_v4().simple().to_string();
        let mut failed = webui::new_job(&old_id, MODEL_DOWNLOAD_KIND, "failed", "failed");
        failed.state = "failed";
        failed.download_status = Some("failed");
        failed.repo_id = Some(repo.clone());
        failed.filename = Some("model.gguf".into());
        webui::jobs().write().await.insert(old_id.clone(), failed);

        let response = retry_download(ConnectionAuth, State(state), axum::extract::Path(old_id.clone()), Query(NodeQuery::default())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let new_id = crate::gateway::testkit::body_of(response).await["download_id"]
            .as_str()
            .unwrap()
            .to_string();

        let jobs = webui::jobs().read().await;
        assert!(!jobs.contains_key(&old_id), "the failed row stayed beside the new one");
        assert!(jobs.contains_key(&new_id));
        drop(jobs);
        let _ = tokio::fs::remove_dir_all(destination).await;
    }

    /// Dismiss takes a failed download off the list, and refuses one still
    /// running.
    #[tokio::test]
    async fn dismiss_removes_a_failed_download_and_refuses_a_running_one() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let failed_id = uuid::Uuid::new_v4().simple().to_string();
        let mut failed = webui::new_job(&failed_id, MODEL_DOWNLOAD_KIND, "failed", "failed");
        failed.state = "failed";
        failed.download_status = Some("failed");
        let running_id = uuid::Uuid::new_v4().simple().to_string();
        let mut running = webui::new_job(&running_id, MODEL_DOWNLOAD_KIND, "running", "downloading");
        running.download_status = Some("downloading");
        {
            let mut jobs = webui::jobs().write().await;
            jobs.insert(failed_id.clone(), failed);
            jobs.insert(running_id.clone(), running);
        }

        let response = dismiss_download(ConnectionAuth, State(state.clone()), axum::extract::Path(failed_id.clone()), Query(NodeQuery::default())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!webui::jobs().read().await.contains_key(&failed_id));

        let response = dismiss_download(ConnectionAuth, State(state.clone()), axum::extract::Path(running_id.clone()), Query(NodeQuery::default())).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(webui::jobs().write().await.remove(&running_id).is_some());

        let response = dismiss_download(ConnectionAuth, State(state.clone()), axum::extract::Path("no-such-download".into()), Query(NodeQuery::default())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Cancelling a download that has not finished takes it off the list.
    #[tokio::test]
    async fn cancelling_an_active_download_removes_its_row() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mut job = webui::new_job(&id, MODEL_DOWNLOAD_KIND, "downloading", "downloading");
        job.download_status = Some("downloading");
        webui::jobs().write().await.insert(id.clone(), job);
        let token = CancellationToken::new();
        state
            .model_service
            .cancellations
            .write()
            .await
            .insert(id.clone(), token.clone());

        let response = cancel_download(ConnectionAuth, State(state), axum::extract::Path(id.clone()), Query(NodeQuery::default())).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(token.is_cancelled());
        assert!(!webui::jobs().read().await.contains_key(&id));
    }

    /// A download on another machine is forwarded, not started here: with
    /// a node id this node does not know, the answer is "not in the
    /// network" and no row appears on this node.
    #[tokio::test]
    async fn a_download_for_another_machine_is_not_started_here() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let before = webui::jobs().read().await.len();
        let response = start_download(
            ConnectionAuth,
            State(state.clone()),
            Json(DownloadRequest {
                repo_id: "owner/model".into(),
                revision: None,
                filename: "model.gguf".into(),
                quantization: None,
                node_id: Some("someone-else".into()),
                extra_files: Vec::new(),
                destination: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["error"]["code"], "node_not_found");
        assert_eq!(webui::jobs().read().await.len(), before);

        let remote = Query(NodeQuery {
            node_id: Some("someone-else".into()),
        });
        let response = list_downloads(ConnectionAuth, State(state.clone()), remote).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// This node's own id, or none, means here.
    #[tokio::test]
    async fn the_own_node_id_means_this_machine() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let own = state.network.node_id.clone();
        assert_eq!(remote_target(&state, Some(&own)), None);
        assert_eq!(remote_target(&state, Some("  ")), None);
        assert_eq!(remote_target(&state, None), None);
        assert_eq!(remote_target(&state, Some("other")), Some("other"));
        let response = list_downloads(
            ConnectionAuth,
            State(state),
            Query(NodeQuery { node_id: Some(own.clone()) }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["node_id"], own.as_str());
        assert!(body["downloads"].is_array());
    }

    /// A finished download names its repository beside the file; a record
    /// whose file is gone names nothing.
    #[tokio::test]
    async fn installed_sources_name_the_repository_of_each_file() {
        let dir = std::env::temp_dir().join(format!("pom-hf-sources-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join(".pom-hf")).unwrap();
        std::fs::write(dir.join("m.gguf"), b"x").unwrap();
        let record = |filename: &str| InstalledRecord {
            source: "huggingface".into(),
            repo_id: "org/m".into(),
            revision: Some("main".into()),
            filename: filename.into(),
            quantization: None,
            size_bytes: 1,
            sha256: None,
            files: Vec::new(),
            downloaded_at: "now".into(),
        };
        for (name, file) in [("a", "m.gguf"), ("b", "gone.gguf")] {
            std::fs::write(
                dir.join(".pom-hf").join(format!("{name}.json")),
                serde_json::to_vec(&record(file)).unwrap(),
            )
            .unwrap();
        }
        let sources = installed_sources(vec![dir.clone()]).await;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["repo_id"], "org/m");
        assert_eq!(sources[0]["filename"], "m.gguf");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_forwarded_download_path_keeps_to_its_route() {
        assert_eq!(
            download_path("ab12", "cancel"),
            "/api/ui/model-provider/downloads/ab12/cancel"
        );
        assert_eq!(
            download_path("../../admin/x", "retry"),
            "/api/ui/model-provider/downloads/adminx/retry"
        );
    }

    /// The extra files of one action: the main file again is ignored, a file
    /// the repository lacks is reported as failed, and the main download
    /// still starts.
    #[tokio::test]
    async fn extra_files_ride_on_the_same_action() {
        let destination = std::env::temp_dir().join(format!(
            "pom-model-extra-{}",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(&destination).await.unwrap();
        let mut state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.auth.write().await.model_dirs =
            Some(vec![destination.to_string_lossy().into_owned()]);
        state.model_service = Arc::new(ModelService::new(Arc::new(DestinationProvider)));
        let response = start_download(
            ConnectionAuth,
            State(state),
            Json(DownloadRequest {
                repo_id: "owner/extra-model".into(),
                revision: None,
                filename: "model.gguf".into(),
                quantization: None,
                node_id: None,
                extra_files: vec!["model.gguf".into(), "mmproj.gguf".into()],
                destination: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        let main = body["download_id"].as_str().unwrap().to_string();
        assert!(body["extra_download_ids"].as_array().unwrap().is_empty());
        assert_eq!(body["failed"][0]["filename"], "mmproj.gguf");
        if let Some(token) = webui::jobs().write().await.remove(&main) {
            drop(token);
        }
        let _ = tokio::fs::remove_dir_all(destination).await;
    }

    #[tokio::test]
    async fn start_download_records_the_configured_folder_as_destination() {
        let destination = std::env::temp_dir().join(format!(
            "pom-model-destination-{}",
            uuid::Uuid::new_v4().simple()
        ));
        tokio::fs::create_dir_all(&destination).await.unwrap();
        let mut state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.auth.write().await.model_dirs =
            Some(vec![destination.to_string_lossy().into_owned()]);
        state.model_service = Arc::new(ModelService::new(Arc::new(DestinationProvider)));

        let response = start_download(
            ConnectionAuth,
            State(state),
            Json(DownloadRequest {
                repo_id: "owner/model".into(),
                revision: None,
                filename: "model.gguf".into(),
                quantization: None,
                node_id: None,
                extra_files: Vec::new(),
                destination: None,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        let download_id = body["download_id"].as_str().unwrap().to_string();
        let expected_path = destination.join("model.gguf");

        let job = webui::jobs()
            .read()
            .await
            .get(&download_id)
            .cloned()
            .unwrap();
        assert_eq!(job.local_path.as_deref(), expected_path.to_str());

        tokio::fs::remove_dir_all(destination).await.unwrap();
    }

    #[test]
    fn structured_download_identity_rejects_traversal() {
        let valid = DownloadRequest {
            repo_id: "Qwen/model".into(),
            revision: Some("refs/main".into()),
            filename: "Qwen3-Q5_K_M.gguf".into(),
            quantization: Some("Q5_K_M".into()),
            node_id: None,
            extra_files: Vec::new(),
            destination: None,
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
        assert!(message.contains("operation") || message.contains("complete"));
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
    fn model_download_destination_uses_only_configured_folders() {
        let configured = vec![
            PathBuf::from("/models/primary"),
            PathBuf::from("/models/backup"),
        ];

        assert_eq!(
            configured_model_destination(&configured),
            Some(PathBuf::from("/models/primary"))
        );
        assert_eq!(configured_model_destination(&[]), None);
        assert_eq!(configured_model_destination(&[PathBuf::new()]), None);
    }

    #[tokio::test]
    async fn storage_response_reports_resolved_model_destinations() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        {
            let mut config = state.auth.write().await;
            config.model_dirs = Some(vec!["/models/primary".into(), "/models/backup".into()]);
        }

        let response = storage_response(&state).await;

        assert_eq!(
            response.destination_paths,
            vec!["/models/primary".to_string(), "/models/backup".to_string()]
        );
        assert_eq!(
            response
                .destinations
                .iter()
                .map(|destination| destination.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/models/primary", "/models/backup"]
        );
    }

    #[test]
    fn requested_destination_must_be_a_configured_folder() {
        let configured = vec![
            PathBuf::from("/models/primary"),
            PathBuf::from("/models/backup"),
        ];

        assert_eq!(
            requested_model_destination(&configured, None),
            Some(PathBuf::from("/models/primary"))
        );
        assert_eq!(
            requested_model_destination(&configured, Some("  ")),
            Some(PathBuf::from("/models/primary"))
        );
        assert_eq!(
            requested_model_destination(&configured, Some("/models/backup")),
            Some(PathBuf::from("/models/backup"))
        );
        assert_eq!(requested_model_destination(&configured, Some("/etc")), None);
        assert_eq!(
            requested_model_destination(&configured, Some("/models/backup/../../etc")),
            None
        );
        assert_eq!(
            requested_model_destination(&[], Some("/models/backup")),
            None
        );
    }

    #[test]
    fn page_size_is_bounded() {
        assert_eq!(Some(1000usize).unwrap().clamp(1, MAX_SEARCH_PAGE_SIZE), 50);
    }
}
