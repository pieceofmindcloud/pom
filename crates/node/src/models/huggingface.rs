//! Provider Hugging Face por HTTP nativo.

use crate::model_provider::{
    validate_directory, validate_filename, validate_filter, validate_repo_id, validate_revision,
    DownloadOptions, DownloadProgress, DownloadResult, DownloadStatus, HfCliStatus, HfError,
    LocalModelStorage, ManifestInput, ModelFile, ModelFiles, ModelInfo, ModelLicense,
    ModelProvider, ModelStorage, ModelSummary, ProgressSender,
};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE};
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use url::Url;

const DEFAULT_ENDPOINT: &str = "https://huggingface.co";
const DEFAULT_REVISION: &str = "main";

trait HfTokenSource: Send + Sync {
    fn load(&self) -> Result<Option<String>, HfError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HfTokenSourceKind {
    HfToken,
    HubToken,
    Config,
    Cache,
    None,
}

impl HfTokenSourceKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::HfToken => "HF_TOKEN",
            Self::HubToken => "HUGGING_FACE_HUB_TOKEN",
            Self::Config => "pom.json",
            Self::Cache => "~/.cache/huggingface/token",
            Self::None => "none",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct HfTokenResolution {
    token: Option<String>,
    source: HfTokenSourceKind,
}

#[derive(Clone, Default)]
pub(crate) struct HfTokenStore(Arc<RwLock<Option<String>>>);

impl HfTokenStore {
    pub(crate) fn new(token: Option<String>) -> Self {
        Self(Arc::new(RwLock::new(clean_token(token))))
    }

    pub(crate) fn set(&self, token: Option<String>) {
        *self.0.write().expect("token store lock poisoned") = clean_token(token);
    }

    fn current(&self) -> Option<String> {
        self.0.read().expect("token store lock poisoned").clone()
    }

    pub(crate) fn status(&self) -> Result<HfTokenStatus, HfError> {
        let hf_token = std::env::var("HF_TOKEN").ok();
        let hub_token = std::env::var("HUGGING_FACE_HUB_TOKEN").ok();
        let config_token = self.current();
        let cache_token = read_cache_token()?;
        let resolution = resolve_token_values(
            hf_token.as_deref(),
            hub_token.as_deref(),
            config_token.as_deref(),
            cache_token.as_deref(),
        );
        Ok(HfTokenStatus {
            configured: config_token.is_some(),
            masked: resolution.token.as_deref().map(mask_token),
            source: resolution.source,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HfTokenStatus {
    pub(crate) configured: bool,
    pub(crate) masked: Option<String>,
    pub(crate) source: HfTokenSourceKind,
}

struct EnvironmentHfTokenSource {
    store: HfTokenStore,
}

impl HfTokenSource for EnvironmentHfTokenSource {
    fn load(&self) -> Result<Option<String>, HfError> {
        let hf_token = std::env::var("HF_TOKEN").ok();
        let hub_token = std::env::var("HUGGING_FACE_HUB_TOKEN").ok();
        let config_token = self.store.current();
        let cache_token = read_cache_token()?;
        Ok(resolve_token_values(
            hf_token.as_deref(),
            hub_token.as_deref(),
            config_token.as_deref(),
            cache_token.as_deref(),
        )
        .token)
    }
}

fn resolve_token_values(
    hf_token: Option<&str>,
    hub_token: Option<&str>,
    config_token: Option<&str>,
    cache_token: Option<&str>,
) -> HfTokenResolution {
    [
        (HfTokenSourceKind::HfToken, hf_token),
        (HfTokenSourceKind::HubToken, hub_token),
        (HfTokenSourceKind::Config, config_token),
        (HfTokenSourceKind::Cache, cache_token),
    ]
    .into_iter()
    .find_map(|(source, token)| {
        clean_token_ref(token).map(|token| HfTokenResolution {
            token: Some(token),
            source,
        })
    })
    .unwrap_or(HfTokenResolution {
        token: None,
        source: HfTokenSourceKind::None,
    })
}

fn read_cache_token() -> Result<Option<String>, HfError> {
    let path = token_path();
    match std::fs::read_to_string(&path) {
        Ok(token) => Ok(clean_token(Some(token))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(HfError::Io {
            message: format!("não foi possível ler a credencial do Hugging Face: {error}"),
        }),
    }
}

fn clean_token(value: Option<String>) -> Option<String> {
    value.and_then(|token| {
        let token = token.trim().to_string();
        (!token.is_empty()).then_some(token)
    })
}

fn clean_token_ref(value: Option<&str>) -> Option<String> {
    clean_token(value.map(str::to_string))
}

fn mask_token(token: &str) -> String {
    let tail: String = token
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if token.chars().count() <= 4 {
        "••••••••".into()
    } else {
        format!("••••••••{tail}")
    }
}

fn token_path() -> PathBuf {
    if let Ok(path) = std::env::var("HF_TOKEN_PATH") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    if let Ok(home) = std::env::var("HF_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join("token");
        }
    }

    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    };
    home.map(|home| PathBuf::from(home).join(".cache/huggingface/token"))
        .unwrap_or_else(|| PathBuf::from(".cache/huggingface/token"))
}

#[derive(Clone)]
pub struct HuggingFaceProvider {
    client: reqwest::Client,
    endpoint: String,
    token_source: Arc<dyn HfTokenSource>,
    storage: Arc<dyn ModelStorage>,
}

impl HuggingFaceProvider {
    pub fn new() -> Self {
        Self::new_with_token_store(HfTokenStore::default())
    }

    pub(crate) fn new_with_token_store(store: HfTokenStore) -> Self {
        Self::from_parts(
            DEFAULT_ENDPOINT,
            Arc::new(EnvironmentHfTokenSource { store }),
            Arc::new(LocalModelStorage),
        )
    }

    fn from_parts(
        endpoint: &str,
        token_source: Arc<dyn HfTokenSource>,
        storage: Arc<dyn ModelStorage>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .expect("cliente HTTP do Hugging Face deve ser criado");
        Self {
            client,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            token_source,
            storage,
        }
    }

    #[cfg(test)]
    fn with_http_endpoint_for_test(endpoint: &str) -> Self {
        Self::from_parts(
            endpoint,
            Arc::new(StaticHfTokenSource(None)),
            Arc::new(LocalModelStorage),
        )
    }

    #[cfg(test)]
    fn with_http_endpoint_and_token_for_test(endpoint: &str, token: Option<&str>) -> Self {
        Self::from_parts(
            endpoint,
            Arc::new(StaticHfTokenSource(token.map(str::to_string))),
            Arc::new(LocalModelStorage),
        )
    }

    #[cfg(test)]
    fn with_http_endpoint_and_token_store_for_test(endpoint: &str, store: HfTokenStore) -> Self {
        Self::from_parts(
            endpoint,
            Arc::new(StoreOnlyHfTokenSource(store)),
            Arc::new(LocalModelStorage),
        )
    }

    #[cfg(test)]
    fn with_http_parts_for_test(
        endpoint: &str,
        token: Option<&str>,
        storage: Arc<dyn ModelStorage>,
    ) -> Self {
        Self::from_parts(
            endpoint,
            Arc::new(StaticHfTokenSource(token.map(str::to_string))),
            storage,
        )
    }

    fn url(&self, segments: &[String]) -> Result<Url, HfError> {
        let mut url = Url::parse(&self.endpoint).map_err(|error| HfError::HfInvalidResponse {
            message: format!("endpoint do Hugging Face inválido: {error}"),
        })?;
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| HfError::HfInvalidResponse {
                    message: "endpoint do Hugging Face não aceita segmentos de caminho".into(),
                })?;
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    fn model_url(
        &self,
        repo_id: &str,
        revision: Option<&str>,
        blobs: bool,
    ) -> Result<Url, HfError> {
        let mut segments = vec!["api".into(), "models".into()];
        segments.extend(repo_id.split('/').map(str::to_string));
        if let Some(revision) = revision {
            segments.push("revision".into());
            segments.push(revision.to_string());
        }
        let mut url = self.url(&segments)?;
        if blobs {
            url.query_pairs_mut().append_pair("blobs", "true");
        }
        Ok(url)
    }

    fn resolve_url(&self, repo_id: &str, revision: &str, filename: &str) -> Result<Url, HfError> {
        let mut segments = vec![repo_id.split('/').next().unwrap().to_string()];
        segments.push(repo_id.split('/').nth(1).unwrap().to_string());
        segments.push("resolve".into());
        segments.push(revision.to_string());
        segments.extend(filename.split('/').map(str::to_string));
        self.url(&segments)
    }

    fn request(&self, method: Method, url: Url) -> Result<(RequestBuilder, bool), HfError> {
        let token = self.token_source.load()?;
        let authenticated = token.is_some();
        let request = self.client.request(method, url);
        Ok((
            match token {
                Some(token) => request.bearer_auth(token),
                None => request,
            },
            authenticated,
        ))
    }

    async fn request_json(&self, url: Url, operation: &str) -> Result<Value, HfError> {
        let (request, authenticated) = self.request(Method::GET, url)?;
        let response = request
            .send()
            .await
            .map_err(|_| HfError::HfInvalidResponse {
                message: format!("{operation}: não foi possível conectar ao Hugging Face"),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(http_error(status, authenticated, operation, false));
        }
        response
            .json()
            .await
            .map_err(|_| HfError::HfInvalidResponse {
                message: format!("{operation}: resposta JSON inválida do Hugging Face"),
            })
    }

    async fn search(&self, query: &str) -> Result<Vec<ModelSummary>, HfError> {
        let mut url = self.url(&["api".into(), "models".into()])?;
        url.query_pairs_mut()
            .append_pair("search", query)
            .append_pair("limit", "100");
        let value = self.request_json(url, "busca").await?;
        let items = array_values(&value, &["models", "items"]);
        items.into_iter().map(parse_model_summary).collect()
    }

    async fn model_info_http(
        &self,
        repo_id: &str,
        revision: Option<&str>,
    ) -> Result<ModelInfo, HfError> {
        let value = self
            .request_json(
                self.model_url(repo_id, revision, true)?,
                "detalhes do modelo",
            )
            .await?;
        parse_model_info_value(&value, repo_id)
    }

    async fn head_remote(
        &self,
        url: Url,
        expected: &ModelFile,
        operation: &str,
    ) -> Result<RemoteMetadata, HfError> {
        let (request, authenticated) = self.request(Method::HEAD, url.clone())?;
        let mut response = request
            .send()
            .await
            .map_err(|_| HfError::HfDownloadFailed {
                message: format!("{operation}: não foi possível consultar o arquivo remoto"),
            })?;
        if matches!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            let (request, authenticated) = self.request(Method::GET, url)?;
            response = request
                .header(RANGE, "bytes=0-0")
                .send()
                .await
                .map_err(|_| HfError::HfDownloadFailed {
                    message: format!("{operation}: não foi possível consultar o arquivo remoto"),
                })?;
            if !response.status().is_success() {
                return Err(http_error(
                    response.status(),
                    authenticated,
                    operation,
                    true,
                ));
            }
        } else if !response.status().is_success() {
            return Err(http_error(
                response.status(),
                authenticated,
                operation,
                true,
            ));
        }

        let content_length = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(content_range_total)
            .or_else(|| {
                response
                    .headers()
                    .get(CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse().ok())
            })
            .or_else(|| response.content_length())
            .or(expected.size_bytes);
        let etag = header_string(&response, ETAG);
        let repo_commit = response
            .headers()
            .get("x-repo-commit")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        Ok(RemoteMetadata {
            size_bytes: content_length,
            etag,
            repo_commit,
        })
    }

    async fn download_one(
        &self,
        repo_id: &str,
        revision: &str,
        file: &ModelFile,
        destination: &Path,
        partial_root: &Path,
        context: DownloadContext<'_>,
    ) -> Result<PathBuf, HfError> {
        let url = self.resolve_url(repo_id, revision, &file.filename)?;
        let remote = self.head_remote(url.clone(), file, "download").await?;
        if let (Some(expected), Some(actual)) = (file.size_bytes, remote.size_bytes) {
            if expected != actual {
                return Err(HfError::HfDownloadFailed {
                    message: "o tamanho remoto mudou antes do download".into(),
                });
            }
        }

        let key = partial_key(repo_id, revision, &file.filename);
        let partial = partial_root.join(format!("{key}.part"));
        let metadata_path = partial_root.join(format!("{key}.json"));
        fs::create_dir_all(partial_root)
            .await
            .map_err(|error| HfError::Io {
                message: format!("não foi possível preparar o cache do download: {error}"),
            })?;
        let metadata = PartialMetadata {
            repo_id: repo_id.to_string(),
            revision: revision.to_string(),
            filename: file.filename.clone(),
            size_bytes: file.size_bytes.or(remote.size_bytes),
            sha256: file.sha256.clone(),
            etag: remote.etag.clone(),
            repo_commit: remote.repo_commit.clone(),
        };

        let mut offset = match fs::metadata(&partial).await {
            Ok(partial_metadata) => {
                let valid_metadata = fs::read(&metadata_path)
                    .await
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<PartialMetadata>(&bytes).ok())
                    .is_some_and(|existing| existing == metadata);
                if valid_metadata {
                    partial_metadata.len()
                } else {
                    remove_file_if_present(&partial).await;
                    remove_file_if_present(&metadata_path).await;
                    0
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(HfError::Io {
                    message: format!("não foi possível consultar o download parcial: {error}"),
                })
            }
        };
        if let Some(expected) = metadata.size_bytes {
            if offset > expected {
                remove_file_if_present(&partial).await;
                remove_file_if_present(&metadata_path).await;
                offset = 0;
            }
        }
        if offset > 0
            && remote.etag.is_none()
            && remote.repo_commit.is_none()
            && file.sha256.is_none()
        {
            remove_file_if_present(&partial).await;
            remove_file_if_present(&metadata_path).await;
            offset = 0;
        }

        write_partial_metadata(&metadata_path, &metadata).await?;
        emit_progress(
            context.progress,
            &file.filename,
            context.completed_before.saturating_add(offset),
            context.total_bytes,
            context.started,
            DownloadStatus::Downloading,
        );

        let mut response = self
            .download_response(url.clone(), offset, remote.etag.as_deref(), context.cancel)
            .await?;
        if offset > 0 && response.status() == StatusCode::OK {
            offset = 0;
            response = self.download_response(url, 0, None, context.cancel).await?;
        }
        if !response.status().is_success() {
            return Err(http_error(
                response.status(),
                self.token_source.load()?.is_some(),
                "download",
                true,
            ));
        }
        if offset > 0 && response.status() == StatusCode::PARTIAL_CONTENT {
            let start = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(content_range_start)
                .ok_or_else(|| HfError::HfDownloadFailed {
                    message: "o servidor não confirmou a faixa retomada".into(),
                })?;
            if start != offset {
                return Err(HfError::HfDownloadFailed {
                    message: "o servidor retornou uma faixa inesperada".into(),
                });
            }
        }

        let mut output = if offset == 0 {
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&partial)
                .await
        } else {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&partial)
                .await
        }
        .map_err(|error| HfError::Io {
            message: format!("não foi possível abrir o download parcial: {error}"),
        })?;
        let mut stream = response.bytes_stream();
        let mut downloaded = offset;
        loop {
            let item = tokio::select! {
                _ = context.cancel.cancelled() => {
                    drop(output);
                    remove_file_if_present(&partial).await;
                    remove_file_if_present(&metadata_path).await;
                    return Err(HfError::Cancelled);
                }
                item = stream.next() => item,
            };
            let Some(item) = item else { break };
            let chunk = item.map_err(|_| HfError::HfDownloadFailed {
                message: "a transferência foi interrompida".into(),
            })?;
            output
                .write_all(&chunk)
                .await
                .map_err(|error| HfError::Io {
                    message: format!("não foi possível gravar o download: {error}"),
                })?;
            downloaded = downloaded.saturating_add(chunk.len() as u64);
            emit_progress(
                context.progress,
                &file.filename,
                context.completed_before.saturating_add(downloaded),
                context.total_bytes,
                context.started,
                DownloadStatus::Downloading,
            );
        }
        output.flush().await.map_err(|error| HfError::Io {
            message: format!("não foi possível concluir o arquivo baixado: {error}"),
        })?;
        drop(output);

        let partial_for_hash = partial.clone();
        let (sha256, size_bytes) =
            tokio::task::spawn_blocking(move || crate::pom::hash_file(&partial_for_hash))
                .await
                .map_err(|error| HfError::Io {
                    message: format!("worker de integridade falhou: {error}"),
                })?
                .map_err(|error| HfError::Io {
                    message: format!("não foi possível verificar o arquivo baixado: {error}"),
                })?;
        if let Some(expected) = metadata.size_bytes {
            if expected != size_bytes {
                remove_file_if_present(&partial).await;
                remove_file_if_present(&metadata_path).await;
                return Err(HfError::HfDownloadFailed {
                    message: "o tamanho do arquivo baixado não confere".into(),
                });
            }
        }
        if let Some(expected) = metadata.sha256.as_deref() {
            if !expected.eq_ignore_ascii_case(&sha256) {
                remove_file_if_present(&partial).await;
                remove_file_if_present(&metadata_path).await;
                return Err(HfError::HfDownloadFailed {
                    message: "a integridade do arquivo baixado não confere".into(),
                });
            }
        }

        let final_path = destination.join(&file.filename);
        if fs::metadata(&final_path).await.is_ok() {
            return Err(HfError::Io {
                message: "o arquivo de destino já existe".into(),
            });
        }
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| HfError::Io {
                    message: format!("não foi possível preparar o destino do modelo: {error}"),
                })?;
        }
        fs::rename(&partial, &final_path)
            .await
            .map_err(|error| HfError::Io {
                message: format!("não foi possível finalizar o arquivo baixado: {error}"),
            })?;
        remove_file_if_present(&metadata_path).await;
        Ok(final_path)
    }

    async fn download_response(
        &self,
        url: Url,
        offset: u64,
        etag: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, HfError> {
        if cancel.is_cancelled() {
            return Err(HfError::Cancelled);
        }
        let (mut request, authenticated) = self.request(Method::GET, url)?;
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
            if let Some(etag) = etag {
                request = request.header(IF_RANGE, etag);
            }
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(HfError::Cancelled),
            response = request.send() => response.map_err(|_| HfError::HfDownloadFailed {
                message: "não foi possível iniciar o download do Hugging Face".into(),
            })?,
        };
        if !response.status().is_success() {
            return Err(http_error(
                response.status(),
                authenticated,
                "download",
                true,
            ));
        }
        Ok(response)
    }

    fn emit(progress: &Option<ProgressSender>, event: DownloadProgress) {
        if let Some(sender) = progress {
            let _ = sender.send(event);
        }
    }
}

impl Default for HuggingFaceProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ModelProvider for HuggingFaceProvider {
    async fn search_models(&self, query: &str) -> Result<Vec<ModelSummary>, HfError> {
        validate_filter(query, "query")?;
        self.search(query).await
    }

    async fn get_model_info(&self, repo_id: &str) -> Result<ModelInfo, HfError> {
        let repo_id = validate_repo_id(repo_id)?;
        self.model_info_http(&repo_id, None).await
    }

    async fn list_files(
        &self,
        repo_id: &str,
        revision: Option<&str>,
    ) -> Result<ModelFiles, HfError> {
        let repo_id = validate_repo_id(repo_id)?;
        let revision = revision.map(validate_revision).transpose()?;
        let info = self.model_info_http(&repo_id, revision.as_deref()).await?;
        Ok(ModelFiles {
            repo_id,
            revision,
            gguf_files: info.gguf_files.clone(),
            files: info.files,
        })
    }

    async fn download_file(
        &self,
        repo_id: &str,
        filename: &str,
        mut options: DownloadOptions,
        progress: Option<ProgressSender>,
        cancel: CancellationToken,
    ) -> Result<DownloadResult, HfError> {
        validate_filename(filename)?;
        options.filenames = vec![filename.to_string()];
        self.download_model(repo_id, options, progress, cancel)
            .await
    }

    async fn download_model(
        &self,
        repo_id: &str,
        options: DownloadOptions,
        progress: Option<ProgressSender>,
        cancel: CancellationToken,
    ) -> Result<DownloadResult, HfError> {
        let repo_id = validate_repo_id(repo_id)?;
        validate_options(&options)?;
        if cancel.is_cancelled() {
            return Err(HfError::Cancelled);
        }
        let destination = options
            .local_dir
            .clone()
            .or_else(|| options.cache_dir.clone())
            .ok_or_else(|| HfError::InvalidInput {
                field: "local_dir".into(),
                message: "o destino local é obrigatório para um download HTTP".into(),
            })?;
        validate_directory(&destination, "local_dir")?;
        let revision = options
            .revision
            .as_deref()
            .unwrap_or(DEFAULT_REVISION)
            .to_string();
        let listed = self.list_files(&repo_id, Some(&revision)).await?;
        let selected = select_files(&listed.files, &options)?;
        if selected.is_empty() {
            return Err(HfError::HfFileNotFound);
        }
        let total_known = selected.iter().all(|file| file.size_bytes.is_some());
        let total_bytes = total_known.then(|| {
            selected
                .iter()
                .map(|file| file.size_bytes.unwrap_or_default())
                .sum()
        });
        let partial_root = options
            .cache_dir
            .clone()
            .unwrap_or_else(|| destination.join(".pom-hf"))
            .join("partials");
        let started = Instant::now();
        Self::emit(
            &progress,
            DownloadProgress {
                status: DownloadStatus::Starting,
                current_file: selected.first().map(|file| file.filename.clone()),
                bytes_downloaded: Some(0),
                bytes_total: total_bytes,
                percent: Some(0.0),
                speed_bytes_per_sec: None,
            },
        );

        let mut completed_bytes = 0;
        let mut local_paths = Vec::with_capacity(selected.len());
        for file in &selected {
            match self
                .download_one(
                    &repo_id,
                    &revision,
                    file,
                    &destination,
                    &partial_root,
                    DownloadContext {
                        progress: &progress,
                        completed_before: completed_bytes,
                        total_bytes,
                        started,
                        cancel: &cancel,
                    },
                )
                .await
            {
                Ok(path) => {
                    completed_bytes = completed_bytes.saturating_add(file.size_bytes.unwrap_or(0));
                    local_paths.push(path);
                }
                Err(error) => {
                    if matches!(error, HfError::Cancelled) {
                        for path in &local_paths {
                            remove_file_if_present(path).await;
                        }
                        Self::emit(
                            &progress,
                            DownloadProgress {
                                status: DownloadStatus::Cancelled,
                                current_file: Some(file.filename.clone()),
                                bytes_downloaded: Some(completed_bytes),
                                bytes_total: total_bytes,
                                percent: percentage(completed_bytes, total_bytes),
                                speed_bytes_per_sec: None,
                            },
                        );
                    } else {
                        Self::emit(
                            &progress,
                            DownloadProgress {
                                status: DownloadStatus::Failed,
                                current_file: Some(file.filename.clone()),
                                bytes_downloaded: Some(completed_bytes),
                                bytes_total: total_bytes,
                                percent: percentage(completed_bytes, total_bytes),
                                speed_bytes_per_sec: None,
                            },
                        );
                    }
                    tracing::warn!(
                        provider = "huggingface",
                        repo_id = %repo_id,
                        revision = %revision,
                        status = "error",
                        error_type = error_type(&error),
                        "download de modelo falhou"
                    );
                    return Err(error);
                }
            }
        }

        let inputs = selected
            .iter()
            .zip(local_paths.iter())
            .map(|(file, path)| ManifestInput {
                filename: file.filename.clone(),
                path: path.clone(),
            })
            .collect::<Vec<_>>();
        let manifest = self
            .storage
            .manifest(
                "huggingface",
                &repo_id,
                options.revision.as_deref(),
                &inputs,
            )
            .await?;
        let files = manifest.files.clone();
        let total_bytes: u64 = files.iter().map(|file| file.size_bytes).sum();
        let duration_ms = started.elapsed().as_millis() as u64;
        let speed = (duration_ms > 0).then(|| total_bytes.saturating_mul(1000) / duration_ms);
        Self::emit(
            &progress,
            DownloadProgress {
                status: DownloadStatus::Completed,
                current_file: selected.last().map(|file| file.filename.clone()),
                bytes_downloaded: Some(total_bytes),
                bytes_total: Some(total_bytes),
                percent: Some(100.0),
                speed_bytes_per_sec: speed,
            },
        );
        tracing::info!(
            provider = "huggingface",
            repo_id = %repo_id,
            revision = %revision,
            download_duration_ms = duration_ms,
            download_bytes = total_bytes,
            status = "ok",
            "download de modelo concluído"
        );
        Ok(DownloadResult {
            repo_id,
            revision: options.revision,
            files,
            local_paths,
            total_bytes,
            duration_ms,
            manifest,
        })
    }

    async fn get_license(&self, repo_id: &str) -> Result<ModelLicense, HfError> {
        let repo_id = validate_repo_id(repo_id)?;
        Ok(self.model_info_http(&repo_id, None).await?.license)
    }

    async fn check_authentication(&self) -> Result<HfCliStatus, HfError> {
        let Some(_) = self.token_source.load()? else {
            return Ok(HfCliStatus {
                installed: true,
                version: None,
                authenticated: false,
                username: None,
            });
        };
        let value = self
            .request_json(
                self.url(&["api".into(), "whoami-v2".into()])?,
                "autenticação",
            )
            .await?;
        let username = value
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| value.get("username").and_then(Value::as_str))
            .map(str::to_string)
            .ok_or_else(|| HfError::HfInvalidResponse {
                message: "autenticação não retornou um usuário".into(),
            })?;
        Ok(HfCliStatus {
            installed: true,
            version: None,
            authenticated: true,
            username: Some(username),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PartialMetadata {
    repo_id: String,
    revision: String,
    filename: String,
    size_bytes: Option<u64>,
    sha256: Option<String>,
    etag: Option<String>,
    repo_commit: Option<String>,
}

#[derive(Debug, Clone)]
struct RemoteMetadata {
    size_bytes: Option<u64>,
    etag: Option<String>,
    repo_commit: Option<String>,
}

struct DownloadContext<'a> {
    progress: &'a Option<ProgressSender>,
    completed_before: u64,
    total_bytes: Option<u64>,
    started: Instant,
    cancel: &'a CancellationToken,
}

#[cfg(test)]
#[derive(Debug)]
struct StaticHfTokenSource(Option<String>);

#[cfg(test)]
impl HfTokenSource for StaticHfTokenSource {
    fn load(&self) -> Result<Option<String>, HfError> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
struct StoreOnlyHfTokenSource(HfTokenStore);

#[cfg(test)]
impl HfTokenSource for StoreOnlyHfTokenSource {
    fn load(&self) -> Result<Option<String>, HfError> {
        Ok(self.0.current())
    }
}

fn http_error(status: StatusCode, authenticated: bool, operation: &str, download: bool) -> HfError {
    match status {
        StatusCode::UNAUTHORIZED if authenticated => HfError::HfAccessDenied,
        StatusCode::UNAUTHORIZED => HfError::HfAuthenticationRequired,
        StatusCode::FORBIDDEN => HfError::HfAccessDenied,
        StatusCode::NOT_FOUND if download => HfError::HfFileNotFound,
        StatusCode::NOT_FOUND => HfError::HfRepositoryNotFound,
        StatusCode::TOO_MANY_REQUESTS => HfError::HfRateLimited,
        _ if download => HfError::HfDownloadFailed {
            message: format!("{operation}: o servidor retornou HTTP {}", status.as_u16()),
        },
        _ => HfError::HfInvalidResponse {
            message: format!("{operation}: o servidor retornou HTTP {}", status.as_u16()),
        },
    }
}

fn parse_model_summary(value: &Value) -> Result<ModelSummary, HfError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_json("modelo não é objeto"))?;
    Ok(ModelSummary {
        repo_id: required_string(object, &["id", "modelId", "repo_id"], "modelo sem id")?,
        author: string_field(object, &["author", "owner"]),
        description: string_field(object, &["description"]),
        last_modified: string_field(object, &["lastModified", "last_modified"]),
        downloads: integer_field(object, &["downloads", "downloadsAllTime"]),
        likes: integer_field(object, &["likes"]),
        private: bool_field(object, &["private"]),
        gated: bool_field(object, &["gated"]),
        tags: string_array_field(object, "tags"),
    })
}

fn parse_model_info_value(value: &Value, fallback_repo_id: &str) -> Result<ModelInfo, HfError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_json("informação do modelo não é objeto"))?;
    let repo_id = string_field(object, &["id", "modelId", "repo_id"])
        .unwrap_or_else(|| fallback_repo_id.to_string());
    let files = object
        .get("siblings")
        .or_else(|| object.get("files"))
        .map(parse_model_files_value)
        .transpose()?
        .unwrap_or_default();
    Ok(ModelInfo {
        repo_id,
        description: string_field(object, &["description"]),
        last_modified: string_field(object, &["lastModified", "last_modified"]),
        sha: string_field(object, &["sha", "sha256"]),
        downloads: integer_field(object, &["downloads"]),
        likes: integer_field(object, &["likes"]),
        private: bool_field(object, &["private"]),
        gated: bool_field(object, &["gated"]),
        tags: string_array_field(object, "tags"),
        license: parse_license_value(value),
        gguf_files: files.iter().filter_map(to_gguf).collect(),
        files,
    })
}

fn parse_model_files_value(value: &Value) -> Result<Vec<ModelFile>, HfError> {
    let items = value
        .as_array()
        .ok_or_else(|| invalid_json("lista de arquivos não é uma matriz"))?;
    items
        .iter()
        .map(|item| {
            let object = item
                .as_object()
                .ok_or_else(|| invalid_json("arquivo não é objeto"))?;
            let filename = required_string(
                object,
                &["path", "rfilename", "filename", "name"],
                "arquivo sem nome",
            )?;
            validate_filename(&filename)?;
            let lfs = object.get("lfs").and_then(Value::as_object);
            Ok(ModelFile {
                filename,
                size_bytes: integer_field(object, &["size", "size_bytes"])
                    .or_else(|| lfs.and_then(|value| integer_field(value, &["size"]))),
                sha256: string_field(object, &["sha256"])
                    .or_else(|| lfs.and_then(|value| string_field(value, &["sha256"]))),
                lfs: Some(lfs.is_some()),
            })
        })
        .collect()
}

fn parse_license_value(value: &Value) -> ModelLicense {
    let object = value.as_object();
    let card_data = object
        .and_then(|object| object.get("cardData").or_else(|| object.get("card_data")))
        .and_then(Value::as_object);
    let raw = card_data
        .and_then(|data| data.get("license"))
        .or_else(|| object.and_then(|data| data.get("license")));
    let tag_license = object
        .and_then(|data| data.get("tags"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|tag| tag.strip_prefix("license:"));

    match raw {
        Some(Value::String(id)) if !id.trim().is_empty() => license_from_id(id, Some(id.clone())),
        Some(Value::Object(data)) => {
            let id = string_field(data, &["id", "key", "spdx_id"]);
            let name = string_field(data, &["name", "label"]);
            let url = string_field(data, &["url", "link"]);
            if id.is_some() || name.is_some() || url.is_some() {
                ModelLicense {
                    raw_value: id.clone().or_else(|| name.clone()),
                    id,
                    name,
                    url,
                    detected: true,
                }
            } else {
                ModelLicense::unknown()
            }
        }
        _ => tag_license
            .map(|id| license_from_id(id, Some(id.to_string())))
            .unwrap_or_else(ModelLicense::unknown),
    }
}

fn license_from_id(id: &str, raw_value: Option<String>) -> ModelLicense {
    let normalized = id.to_ascii_lowercase();
    let name = match normalized.as_str() {
        "apache-2.0" => Some("Apache License 2.0".into()),
        "mit" => Some("MIT License".into()),
        "gpl-3.0" => Some("GNU General Public License v3.0".into()),
        _ => None,
    };
    let url = match normalized.as_str() {
        "apache-2.0" => Some("https://www.apache.org/licenses/LICENSE-2.0".into()),
        "mit" => Some("https://opensource.org/license/mit".into()),
        "gpl-3.0" => Some("https://www.gnu.org/licenses/gpl-3.0.html".into()),
        _ => None,
    };
    ModelLicense {
        id: Some(id.into()),
        name,
        url,
        raw_value,
        detected: true,
    }
}

fn array_values<'a>(value: &'a Value, keys: &[&str]) -> Vec<&'a Value> {
    if let Some(items) = value.as_array() {
        return items.iter().collect();
    }
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_array))
        .map(|items| items.iter().collect())
        .unwrap_or_default()
}

fn required_string(
    object: &Map<String, Value>,
    keys: &[&str],
    message: &str,
) -> Result<String, HfError> {
    string_field(object, keys).ok_or_else(|| invalid_json(message))
}

fn string_field(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str).map(str::to_string))
}

fn integer_field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        object.get(*key).and_then(Value::as_u64).or_else(|| {
            object
                .get(*key)
                .and_then(Value::as_i64)
                .and_then(|value| u64::try_from(value).ok())
        })
    })
}

fn bool_field(object: &Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_bool))
}

fn string_array_field(object: &Map<String, Value>, key: &str) -> Vec<String> {
    object
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn invalid_json(message: &str) -> HfError {
    HfError::HfInvalidResponse {
        message: message.to_string(),
    }
}

fn to_gguf(file: &ModelFile) -> Option<crate::model_provider::GgufFile> {
    if !file.filename.to_ascii_lowercase().ends_with(".gguf") {
        return None;
    }
    let metadata = crate::model_provider::gguf_metadata(&file.filename);
    Some(crate::model_provider::GgufFile {
        filename: file.filename.clone(),
        size_bytes: file.size_bytes,
        quantization: metadata.quantization,
        parameter_size: metadata.parameter_size,
        base_name: metadata.base_name,
    })
}

fn validate_options(options: &DownloadOptions) -> Result<(), HfError> {
    if let Some(revision) = &options.revision {
        validate_revision(revision)?;
    }
    if let Some(cache_dir) = &options.cache_dir {
        validate_directory(cache_dir, "cache_dir")?;
    }
    if let Some(local_dir) = &options.local_dir {
        validate_directory(local_dir, "local_dir")?;
    }
    for filename in &options.filenames {
        validate_filename(filename)?;
    }
    for include in &options.include {
        validate_filter(include, "include")?;
    }
    for exclude in &options.exclude {
        validate_filter(exclude, "exclude")?;
    }
    Ok(())
}

fn select_files(files: &[ModelFile], options: &DownloadOptions) -> Result<Vec<ModelFile>, HfError> {
    let selected = files
        .iter()
        .filter(|file| {
            let explicitly_requested = options.filenames.is_empty()
                || options.filenames.iter().any(|name| name == &file.filename);
            let included = options.include.is_empty()
                || options
                    .include
                    .iter()
                    .any(|pattern| wildcard_match(pattern, &file.filename));
            let excluded = options
                .exclude
                .iter()
                .any(|pattern| wildcard_match(pattern, &file.filename));
            explicitly_requested && included && !excluded
        })
        .cloned()
        .collect::<Vec<_>>();
    Ok(selected)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for &pattern_byte in pattern {
        let mut current = vec![false; value.len() + 1];
        if pattern_byte == b'*' {
            current[0] = previous[0];
            for index in 1..=value.len() {
                current[index] = previous[index] || current[index - 1];
            }
        } else {
            for index in 1..=value.len() {
                current[index] = previous[index - 1]
                    && (pattern_byte == b'?' || pattern_byte == value[index - 1]);
            }
        }
        previous = current;
    }
    previous[value.len()]
}

fn partial_key(repo_id: &str, revision: &str, filename: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_id.as_bytes());
    hasher.update([0]);
    hasher.update(revision.as_bytes());
    hasher.update([0]);
    hasher.update(filename.as_bytes());
    hex::encode(hasher.finalize())
}

async fn write_partial_metadata(path: &Path, metadata: &PartialMetadata) -> Result<(), HfError> {
    let bytes = serde_json::to_vec(metadata).map_err(|error| HfError::Io {
        message: format!("não foi possível serializar o estado do download: {error}"),
    })?;
    fs::write(path, bytes).await.map_err(|error| HfError::Io {
        message: format!("não foi possível persistir o estado do download: {error}"),
    })
}

async fn remove_file_if_present(path: &Path) {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
}

fn header_string(
    response: &reqwest::Response,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn content_range_start(value: &str) -> Option<u64> {
    value
        .strip_prefix("bytes ")?
        .split('/')
        .next()?
        .split('-')
        .next()?
        .parse()
        .ok()
}

fn content_range_total(value: &str) -> Option<u64> {
    value.split('/').nth(1)?.parse().ok()
}

fn emit_progress(
    progress: &Option<ProgressSender>,
    filename: &str,
    downloaded: u64,
    total: Option<u64>,
    started: Instant,
    status: DownloadStatus,
) {
    let seconds = started.elapsed().as_secs_f64();
    let speed = (seconds > 0.0).then_some((downloaded as f64 / seconds) as u64);
    HuggingFaceProvider::emit(
        progress,
        DownloadProgress {
            status,
            current_file: Some(filename.to_string()),
            bytes_downloaded: Some(downloaded),
            bytes_total: total,
            percent: percentage(downloaded, total),
            speed_bytes_per_sec: speed,
        },
    );
}

fn percentage(downloaded: u64, total: Option<u64>) -> Option<f64> {
    total
        .filter(|total| *total > 0)
        .map(|total| (downloaded as f64 * 100.0 / total as f64).clamp(0.0, 100.0))
}

fn error_type(error: &HfError) -> &'static str {
    match error {
        HfError::HfAuthenticationRequired => "authentication_required",
        HfError::HfRepositoryNotFound => "repository_not_found",
        HfError::HfFileNotFound => "file_not_found",
        HfError::HfAccessDenied => "access_denied",
        HfError::HfRateLimited => "rate_limited",
        HfError::HfDownloadFailed { .. } => "download_failed",
        HfError::HfInvalidResponse { .. } => "invalid_response",
        HfError::InvalidInput { .. } => "invalid_input",
        HfError::Io { .. } => "io",
        HfError::Cancelled => "cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        extract::State,
        http::{HeaderMap, Method, StatusCode, Uri},
        response::Response,
        routing::any,
        Router,
    };
    use futures::stream;
    use std::convert::Infallible;
    use tokio::sync::{oneshot, Mutex};
    use tokio::time::{sleep, Duration};

    #[derive(Debug, Clone)]
    struct MockRequest {
        method: Method,
        path: String,
        range: Option<String>,
        if_range: Option<String>,
    }

    #[derive(Clone)]
    struct MockHubState {
        bytes: Arc<Vec<u8>>,
        sha256: String,
        etag: String,
        commit: String,
        forbidden: bool,
        slow: bool,
        requests: Arc<Mutex<Vec<MockRequest>>>,
    }

    impl MockHubState {
        fn new(bytes: Vec<u8>) -> Self {
            let sha256 = hex::encode(Sha256::digest(&bytes));
            Self {
                bytes: Arc::new(bytes),
                sha256,
                etag: "\"etag-v1\"".into(),
                commit: "commit-v1".into(),
                forbidden: false,
                slow: false,
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    struct MockServer {
        endpoint: String,
        shutdown: Option<oneshot::Sender<()>>,
    }

    impl MockServer {
        async fn start(state: MockHubState) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let (shutdown, signal) = oneshot::channel();
            let app = Router::new()
                .fallback(any(mock_hub_handler))
                .with_state(state);
            tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = signal.await;
                    })
                    .await;
            });
            Self {
                endpoint,
                shutdown: Some(shutdown),
            }
        }

        async fn stop(mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            tokio::task::yield_now().await;
        }
    }

    async fn mock_hub_handler(
        State(state): State<MockHubState>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
    ) -> Response {
        let path = uri.path().to_string();
        state.requests.lock().await.push(MockRequest {
            method: method.clone(),
            path: path.clone(),
            range: headers
                .get("range")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            if_range: headers
                .get("if-range")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
        });

        if path == "/api/models" {
            return json_response(
                StatusCode::OK,
                serde_json::json!([{
                    "id": "owner/model",
                    "author": "owner",
                    "description": "modelo de teste",
                    "downloads": 7,
                    "likes": 2,
                    "tags": ["gguf"]
                }]),
            );
        }
        if path == "/api/whoami-v2" {
            return if headers.contains_key("authorization") {
                json_response(StatusCode::OK, serde_json::json!({"name": "captain"}))
            } else {
                response(StatusCode::UNAUTHORIZED, &[], Body::empty())
            };
        }
        if path.starts_with("/api/models/owner/model") {
            if state.forbidden {
                return response(StatusCode::FORBIDDEN, &[], Body::empty());
            }
            return json_response(
                StatusCode::OK,
                serde_json::json!({
                    "id": "owner/model",
                    "description": "modelo de teste",
                    "cardData": {"license": "apache-2.0"},
                    "siblings": [{
                        "rfilename": "model.gguf",
                        "size": state.bytes.len(),
                        "lfs": {"size": state.bytes.len(), "sha256": state.sha256}
                    }]
                }),
            );
        }
        if path == "/owner/model/resolve/main/model.gguf" {
            if method == Method::HEAD {
                return response(
                    StatusCode::OK,
                    &[
                        ("content-length", state.bytes.len().to_string()),
                        ("etag", state.etag.clone()),
                        ("x-repo-commit", state.commit.clone()),
                    ],
                    Body::empty(),
                );
            }
            if method == Method::GET {
                let start = headers
                    .get("range")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("bytes="))
                    .and_then(|value| value.strip_suffix('-'))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                if start >= state.bytes.len() {
                    return response(StatusCode::RANGE_NOT_SATISFIABLE, &[], Body::empty());
                }
                let chunks = state.bytes[start..]
                    .chunks(4)
                    .map(<[u8]>::to_vec)
                    .collect::<Vec<_>>();
                let slow = state.slow;
                let stream = stream::unfold((chunks, 0usize), move |(chunks, index)| async move {
                    if index >= chunks.len() {
                        return None;
                    }
                    if slow {
                        sleep(Duration::from_millis(100)).await;
                    }
                    Some((
                        Ok::<_, Infallible>(chunks[index].clone()),
                        (chunks, index + 1),
                    ))
                });
                let status = if start == 0 {
                    StatusCode::OK
                } else {
                    StatusCode::PARTIAL_CONTENT
                };
                let mut response = Response::builder()
                    .status(status)
                    .header("content-length", state.bytes.len() - start)
                    .header("etag", state.etag.clone())
                    .header("x-repo-commit", state.commit.clone());
                if start > 0 {
                    response = response.header(
                        "content-range",
                        format!(
                            "bytes {start}-{}/{}",
                            state.bytes.len() - 1,
                            state.bytes.len()
                        ),
                    );
                }
                return response.body(Body::from_stream(stream)).unwrap();
            }
        }
        response(StatusCode::NOT_FOUND, &[], Body::empty())
    }

    fn response(status: StatusCode, headers: &[(&str, String)], body: Body) -> Response {
        let mut response = Response::builder().status(status);
        for (name, value) in headers {
            response = response.header(*name, value);
        }
        response.body(body).unwrap()
    }

    fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
        response(
            status,
            &[("content-type", "application/json".into())],
            Body::from(serde_json::to_vec(&value).unwrap()),
        )
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pom-hf-http-{label}-{}", uuid::Uuid::new_v4()))
    }

    async fn model_options(root: &Path) -> DownloadOptions {
        fs::create_dir_all(root).await.unwrap();
        DownloadOptions {
            local_dir: Some(root.to_path_buf()),
            filenames: vec!["model.gguf".into()],
            ..DownloadOptions::default()
        }
    }

    #[tokio::test]
    async fn native_http_provider_reports_http_failure_without_cli_dependency() {
        let provider = HuggingFaceProvider::with_http_endpoint_for_test("http://127.0.0.1:1");
        let error = provider.search_models("qwen").await.unwrap_err();
        assert!(matches!(error, HfError::HfInvalidResponse { .. }));
    }

    #[tokio::test]
    async fn http_provider_covers_search_details_files_and_license() {
        let state = MockHubState::new(b"abcdefgh".to_vec());
        let server = MockServer::start(state).await;
        let provider = HuggingFaceProvider::with_http_endpoint_for_test(&server.endpoint);

        let summaries = provider.search_models("qwen").await.unwrap();
        assert_eq!(summaries[0].repo_id, "owner/model");
        let info = provider.get_model_info("owner/model").await.unwrap();
        assert_eq!(info.files[0].filename, "model.gguf");
        assert_eq!(info.license.id.as_deref(), Some("apache-2.0"));
        let files = provider
            .list_files("owner/model", Some("main"))
            .await
            .unwrap();
        assert_eq!(files.gguf_files.len(), 1);
        assert_eq!(
            provider
                .get_license("owner/model")
                .await
                .unwrap()
                .id
                .as_deref(),
            Some("apache-2.0")
        );

        server.stop().await;
    }

    #[tokio::test]
    async fn authentication_status_distinguishes_missing_token_and_authenticated_user() {
        let state = MockHubState::new(b"abcdefgh".to_vec());
        let server = MockServer::start(state).await;
        let without_token = HuggingFaceProvider::with_http_endpoint_for_test(&server.endpoint);
        let status = without_token.check_authentication().await.unwrap();
        assert!(status.installed);
        assert!(!status.authenticated);
        assert_eq!(status.username, None);

        let with_token = HuggingFaceProvider::with_http_endpoint_and_token_for_test(
            &server.endpoint,
            Some("hf_test_secret"),
        );
        let status = with_token.check_authentication().await.unwrap();
        assert!(status.authenticated);
        assert_eq!(status.username.as_deref(), Some("captain"));

        server.stop().await;
    }

    #[test]
    fn configured_token_beats_cache_but_environment_keeps_existing_precedence() {
        let resolved = resolve_token_values(
            Some("hf-from-env"),
            Some("hf-from-hub-env"),
            Some("hf-from-config"),
            Some("hf-from-cache"),
        );
        assert_eq!(resolved.source, HfTokenSourceKind::HfToken);
        assert_eq!(resolved.token.as_deref(), Some("hf-from-env"));

        let resolved = resolve_token_values(
            None,
            Some("hf-from-hub-env"),
            Some("hf-from-config"),
            Some("hf-from-cache"),
        );
        assert_eq!(resolved.source, HfTokenSourceKind::HubToken);
        assert_eq!(resolved.token.as_deref(), Some("hf-from-hub-env"));

        let resolved =
            resolve_token_values(None, None, Some("hf-from-config"), Some("hf-from-cache"));
        assert_eq!(resolved.source, HfTokenSourceKind::Config);
        assert_eq!(resolved.token.as_deref(), Some("hf-from-config"));
    }

    #[tokio::test]
    async fn configured_token_is_reloaded_without_recreating_the_provider() {
        let state = MockHubState::new(b"abcdefgh".to_vec());
        let server = MockServer::start(state).await;
        let store = HfTokenStore::new(None);
        let provider = HuggingFaceProvider::with_http_endpoint_and_token_store_for_test(
            &server.endpoint,
            store.clone(),
        );

        assert!(!provider.check_authentication().await.unwrap().authenticated);
        store.set(Some("hf_test_secret".into()));
        let status = provider.check_authentication().await.unwrap();
        assert!(status.authenticated);
        assert_eq!(status.username.as_deref(), Some("captain"));

        server.stop().await;
    }

    #[tokio::test]
    async fn access_denied_is_distinct_from_missing_authentication() {
        let mut state = MockHubState::new(b"abcdefgh".to_vec());
        state.forbidden = true;
        let server = MockServer::start(state).await;
        let provider = HuggingFaceProvider::with_http_endpoint_and_token_for_test(
            &server.endpoint,
            Some("hf_test_secret"),
        );
        assert_eq!(
            provider.get_model_info("owner/model").await.unwrap_err(),
            HfError::HfAccessDenied
        );
        assert_eq!(
            http_error(StatusCode::UNAUTHORIZED, false, "auth", false),
            HfError::HfAuthenticationRequired
        );
        assert_eq!(
            http_error(StatusCode::UNAUTHORIZED, true, "auth", false),
            HfError::HfAccessDenied
        );
        server.stop().await;
    }

    #[tokio::test]
    async fn missing_remote_file_and_path_traversal_are_rejected() {
        let state = MockHubState::new(b"abcdefgh".to_vec());
        let server = MockServer::start(state).await;
        let provider = HuggingFaceProvider::with_http_endpoint_for_test(&server.endpoint);
        let root = temp_root("missing");
        let options = model_options(&root).await;
        assert_eq!(
            provider
                .download_file(
                    "owner/model",
                    "missing.gguf",
                    options,
                    None,
                    CancellationToken::new()
                )
                .await
                .unwrap_err(),
            HfError::HfFileNotFound
        );
        assert!(matches!(
            provider
                .download_file(
                    "owner/model",
                    "../outside.gguf",
                    DownloadOptions::default(),
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap_err(),
            HfError::InvalidInput { .. }
        ));

        server.stop().await;
        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn resumes_partial_file_only_with_matching_remote_identity() {
        let state = MockHubState::new(b"abcdefgh".to_vec());
        let requests = state.requests.clone();
        let server = MockServer::start(state.clone()).await;
        let provider = HuggingFaceProvider::with_http_endpoint_for_test(&server.endpoint);
        let root = temp_root("resume");
        let partial_root = root.join(".pom-hf/partials");
        fs::create_dir_all(&partial_root).await.unwrap();
        let partial = partial_root.join(format!(
            "{}.part",
            partial_key("owner/model", "main", "model.gguf")
        ));
        fs::write(&partial, b"abcd").await.unwrap();
        let metadata = PartialMetadata {
            repo_id: "owner/model".into(),
            revision: "main".into(),
            filename: "model.gguf".into(),
            size_bytes: Some(8),
            sha256: Some(state.sha256.clone()),
            etag: Some(state.etag.clone()),
            repo_commit: Some(state.commit.clone()),
        };
        fs::write(
            partial.with_extension("json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .await
        .unwrap();

        let result = provider
            .download_file(
                "owner/model",
                "model.gguf",
                DownloadOptions {
                    local_dir: Some(root.clone()),
                    ..DownloadOptions::default()
                },
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(fs::read(&result.local_paths[0]).await.unwrap(), b"abcdefgh");
        let requests = requests.lock().await;
        let download = requests
            .iter()
            .find(|request| request.method == Method::GET && request.path.contains("/resolve/"))
            .unwrap();
        assert_eq!(download.range.as_deref(), Some("bytes=4-"));
        assert_eq!(download.if_range.as_deref(), Some("\"etag-v1\""));

        server.stop().await;
        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn discards_partial_file_when_remote_identity_changes() {
        let state = MockHubState::new(b"abcdefgh".to_vec());
        let requests = state.requests.clone();
        let server = MockServer::start(state.clone()).await;
        let provider = HuggingFaceProvider::with_http_endpoint_for_test(&server.endpoint);
        let root = temp_root("changed");
        let partial_root = root.join(".pom-hf/partials");
        fs::create_dir_all(&partial_root).await.unwrap();
        let partial = partial_root.join(format!(
            "{}.part",
            partial_key("owner/model", "main", "model.gguf")
        ));
        fs::write(&partial, b"old-").await.unwrap();
        let old_sha256 = hex::encode(Sha256::digest(b"old-data"));
        let old_metadata = PartialMetadata {
            repo_id: "owner/model".into(),
            revision: "main".into(),
            filename: "model.gguf".into(),
            size_bytes: Some(8),
            sha256: Some(old_sha256),
            etag: Some("\"etag-old\"".into()),
            repo_commit: Some("commit-old".into()),
        };
        fs::write(
            partial.with_extension("json"),
            serde_json::to_vec(&old_metadata).unwrap(),
        )
        .await
        .unwrap();

        let result = provider
            .download_file(
                "owner/model",
                "model.gguf",
                DownloadOptions {
                    local_dir: Some(root.clone()),
                    ..DownloadOptions::default()
                },
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(fs::read(&result.local_paths[0]).await.unwrap(), b"abcdefgh");
        let requests = requests.lock().await;
        let download = requests
            .iter()
            .find(|request| request.method == Method::GET && request.path.contains("/resolve/"))
            .unwrap();
        assert_eq!(download.range, None);

        server.stop().await;
        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn cancellation_removes_partial_transfer_state() {
        let mut state = MockHubState::new(b"abcdefghijklmno".to_vec());
        state.slow = true;
        let requests = state.requests.clone();
        let server = MockServer::start(state).await;
        let provider = HuggingFaceProvider::with_http_endpoint_for_test(&server.endpoint);
        let root = temp_root("cancel");
        let options = model_options(&root).await;
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let provider = provider.clone();
            let cancel = cancel.clone();
            async move {
                provider
                    .download_file("owner/model", "model.gguf", options, None, cancel)
                    .await
            }
        });
        for _ in 0..100 {
            if requests
                .lock()
                .await
                .iter()
                .any(|request| request.method == Method::GET && request.path.contains("/resolve/"))
            {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err(), HfError::Cancelled);
        assert!(!root.join("model.gguf").exists());
        if let Ok(mut entries) = fs::read_dir(root.join(".pom-hf/partials")).await {
            assert!(entries.next_entry().await.unwrap().is_none());
        }

        server.stop().await;
        let _ = fs::remove_dir_all(root).await;
    }

    #[test]
    fn wildcard_filters_support_repository_paths() {
        assert!(wildcard_match("*.gguf", "nested/model.gguf"));
        assert!(wildcard_match("nested/*", "nested/model.gguf"));
        assert!(!wildcard_match("*.safetensors", "nested/model.gguf"));
    }
}
