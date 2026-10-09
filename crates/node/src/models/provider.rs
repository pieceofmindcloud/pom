//! Agnostic contract for model sources.
//!
//! This module does not know the Hugging Face transport. The types here are the
//! stable boundary that the UI and any remote provider can consume.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub type ProgressSender = UnboundedSender<DownloadProgress>;

#[derive(Clone, PartialEq, Eq)]
pub enum HfError {
    HfAuthenticationRequired,
    HfRepositoryNotFound,
    HfFileNotFound,
    HfAccessDenied,
    HfRateLimited,
    HfDownloadFailed { message: String },
    HfInvalidResponse { message: String },
    InvalidInput { field: String, message: String },
    Io { message: String },
    Cancelled,
}

impl fmt::Debug for HfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HfAuthenticationRequired => f.write_str("HfAuthenticationRequired"),
            Self::HfRepositoryNotFound => f.write_str("HfRepositoryNotFound"),
            Self::HfFileNotFound => f.write_str("HfFileNotFound"),
            Self::HfAccessDenied => f.write_str("HfAccessDenied"),
            Self::HfRateLimited => f.write_str("HfRateLimited"),
            Self::HfDownloadFailed { message } => f
                .debug_struct("HfDownloadFailed")
                .field("message", &redact_sensitive(message))
                .finish(),
            Self::HfInvalidResponse { message } => f
                .debug_struct("HfInvalidResponse")
                .field("message", &redact_sensitive(message))
                .finish(),
            Self::InvalidInput { field, message } => f
                .debug_struct("InvalidInput")
                .field("field", field)
                .field("message", &redact_sensitive(message))
                .finish(),
            Self::Io { message } => f
                .debug_struct("Io")
                .field("message", &redact_sensitive(message))
                .finish(),
            Self::Cancelled => f.write_str("Cancelled"),
        }
    }
}

impl fmt::Display for HfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HfAuthenticationRequired => {
                write!(f, "Hugging Face authentication is required")
            }
            Self::HfRepositoryNotFound => write!(f, "Hugging Face repository not found"),
            Self::HfFileNotFound => write!(f, "Hugging Face file not found"),
            Self::HfAccessDenied => write!(f, "access to the Hugging Face repository denied"),
            Self::HfRateLimited => {
                write!(f, "Hugging Face refused the request because of a usage limit")
            }
            Self::HfDownloadFailed { message } => write!(
                f,
                "Hugging Face download failed: {}",
                redact_sensitive(message)
            ),
            Self::HfInvalidResponse { message } => write!(
                f,
                "invalid response from Hugging Face: {}",
                redact_sensitive(message)
            ),
            Self::InvalidInput { field, message } => {
                write!(f, "invalid {field}: {}", redact_sensitive(message))
            }
            Self::Io { message } => {
                write!(f, "local operation failed: {}", redact_sensitive(message))
            }
            Self::Cancelled => write!(f, "download cancelled"),
        }
    }
}

impl std::error::Error for HfError {}

/// Redacts tokens without logging raw stdout/stderr.
pub(crate) fn redact_sensitive<T: AsRef<str>>(text: T) -> String {
    let mut out = text.as_ref().to_string();
    for marker in [
        "hf_",
        "HF_TOKEN=",
        "hf_token=",
        "token=",
        "Authorization: Bearer ",
        "authorization: bearer ",
    ] {
        let mut cursor = 0;
        while let Some(relative) = out[cursor..].find(marker) {
            let start = cursor + relative;
            let value_start = start + marker.len();
            let end = out[value_start..]
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | ')'))
                .map(|offset| value_start + offset)
                .unwrap_or(out.len());
            out.replace_range(value_start..end, "[REDACTED]");
            cursor = value_start + "[REDACTED]".len();
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelSummary {
    pub repo_id: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub last_modified: Option<String>,
    pub downloads: Option<u64>,
    pub likes: Option<u64>,
    pub private: Option<bool>,
    pub gated: Option<bool>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub repo_id: String,
    pub description: Option<String>,
    pub last_modified: Option<String>,
    pub sha: Option<String>,
    pub downloads: Option<u64>,
    pub likes: Option<u64>,
    pub private: Option<bool>,
    pub gated: Option<bool>,
    pub tags: Vec<String>,
    pub license: ModelLicense,
    pub files: Vec<ModelFile>,
    pub gguf_files: Vec<GgufFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelFiles {
    pub repo_id: String,
    pub revision: Option<String>,
    pub files: Vec<ModelFile>,
    pub gguf_files: Vec<GgufFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelFile {
    pub filename: String,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub lfs: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GgufFile {
    pub filename: String,
    pub size_bytes: Option<u64>,
    pub quantization: Option<String>,
    pub parameter_size: Option<String>,
    pub base_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelLicense {
    pub id: Option<String>,
    pub name: Option<String>,
    pub url: Option<String>,
    pub raw_value: Option<String>,
    pub detected: bool,
}

impl ModelLicense {
    pub fn unknown() -> Self {
        Self {
            id: None,
            name: None,
            url: None,
            raw_value: None,
            detected: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HfCliStatus {
    pub installed: bool,
    pub version: Option<String>,
    pub authenticated: bool,
    pub username: Option<String>,
}

impl HfCliStatus {
    #[cfg(test)]
    pub fn not_installed() -> Self {
        Self {
            installed: false,
            version: None,
            authenticated: false,
            username: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadOptions {
    pub revision: Option<String>,
    pub cache_dir: Option<PathBuf>,
    pub local_dir: Option<PathBuf>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub filenames: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStatus {
    Starting,
    Downloading,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DownloadProgress {
    pub status: DownloadStatus,
    pub current_file: Option<String>,
    pub bytes_downloaded: Option<u64>,
    pub bytes_total: Option<u64>,
    pub percent: Option<f64>,
    pub speed_bytes_per_sec: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DownloadResult {
    pub repo_id: String,
    pub revision: Option<String>,
    pub files: Vec<ManifestFile>,
    pub local_paths: Vec<PathBuf>,
    pub total_bytes: u64,
    pub duration_ms: u64,
    pub manifest: ModelManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelManifest {
    pub source: String,
    pub repo_id: String,
    pub revision: Option<String>,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestFile {
    pub filename: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestInput {
    pub filename: String,
    pub path: PathBuf,
}

#[async_trait]
pub trait ModelStorage: Send + Sync {
    async fn manifest(
        &self,
        source: &str,
        repo_id: &str,
        revision: Option<&str>,
        files: &[ManifestInput],
    ) -> Result<ModelManifest, HfError>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LocalModelStorage;

#[async_trait]
impl ModelStorage for LocalModelStorage {
    async fn manifest(
        &self,
        source: &str,
        repo_id: &str,
        revision: Option<&str>,
        files: &[ManifestInput],
    ) -> Result<ModelManifest, HfError> {
        let source = source.to_string();
        let repo_id = repo_id.to_string();
        let revision = revision.map(str::to_string);
        let files = files.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut manifest_files = Vec::with_capacity(files.len());
            for input in files {
                validate_filename(&input.filename)?;
                let metadata = std::fs::metadata(&input.path).map_err(|error| HfError::Io {
                    message: redact_sensitive(format!("{}: {error}", input.path.display())),
                })?;
                if !metadata.is_file() {
                    return Err(HfError::Io {
                        message: format!("{} is not a regular file", input.path.display()),
                    });
                }
                let (sha256, size_bytes) =
                    crate::pom::hash_file(&input.path).map_err(|error| HfError::Io {
                        message: redact_sensitive(format!("{}: {error:#}", input.path.display())),
                    })?;
                manifest_files.push(ManifestFile {
                    filename: input.filename,
                    path: input.path,
                    size_bytes,
                    sha256: Some(sha256),
                });
            }
            Ok(ModelManifest {
                source,
                repo_id,
                revision,
                files: manifest_files,
            })
        })
        .await
        .map_err(|error| HfError::Io {
            message: redact_sensitive(format!("manifest worker failed: {error}")),
        })?
    }
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn search_models(&self, query: &str) -> Result<Vec<ModelSummary>, HfError>;
    async fn get_model_info(&self, repo_id: &str) -> Result<ModelInfo, HfError>;
    async fn list_files(
        &self,
        repo_id: &str,
        revision: Option<&str>,
    ) -> Result<ModelFiles, HfError>;
    async fn download_file(
        &self,
        repo_id: &str,
        filename: &str,
        options: DownloadOptions,
        progress: Option<ProgressSender>,
        cancel: CancellationToken,
    ) -> Result<DownloadResult, HfError>;
    async fn download_model(
        &self,
        repo_id: &str,
        options: DownloadOptions,
        progress: Option<ProgressSender>,
        cancel: CancellationToken,
    ) -> Result<DownloadResult, HfError>;
    async fn get_license(&self, repo_id: &str) -> Result<ModelLicense, HfError>;
    async fn check_authentication(&self) -> Result<HfCliStatus, HfError>;
}

pub type DynModelProvider = Arc<dyn ModelProvider>;

pub fn validate_repo_id(value: &str) -> Result<String, HfError> {
    if value.is_empty() || value.trim() != value || value.contains('\0') {
        return Err(invalid("repo_id", "must be a non-empty identifier"));
    }
    let parts: Vec<&str> = value.split('/').collect();
    if parts.len() != 2
        || parts
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err(invalid("repo_id", "use OWNER/REPOSITORY without traversal"));
    }
    if parts.iter().any(|part| {
        !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    }) {
        return Err(invalid("repo_id", "contains a character that is not allowed"));
    }
    Ok(value.to_string())
}

pub fn validate_revision(value: &str) -> Result<String, HfError> {
    if value.is_empty()
        || value.trim() != value
        || value.contains('\0')
        || value.contains('\\')
        || value.starts_with('/')
        || value.ends_with('/')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid("revision", "invalid path or revision"));
    }
    Ok(value.to_string())
}

pub fn validate_filename(value: &str) -> Result<String, HfError> {
    if value.is_empty()
        || value.trim() != value
        || value.contains('\0')
        || value.contains('\\')
        || value.contains(':')
        || value.starts_with('/')
        || value.ends_with('/')
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(invalid("filename", "invalid relative file name"));
    }
    Ok(value.to_string())
}

pub fn validate_filter(value: &str, field: &str) -> Result<String, HfError> {
    if value.is_empty() || value.contains('\0') {
        return Err(invalid(field, "filter is empty or contains NUL"));
    }
    Ok(value.to_string())
}

pub fn validate_directory(path: &Path, field: &str) -> Result<PathBuf, HfError> {
    if path.as_os_str().to_string_lossy().contains('\0')
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(invalid(field, "path contains NUL or traversal"));
    }
    Ok(path.to_path_buf())
}

fn invalid(field: &str, message: &str) -> HfError {
    HfError::InvalidInput {
        field: field.to_string(),
        message: message.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufMetadata {
    pub quantization: Option<String>,
    pub parameter_size: Option<String>,
    pub base_name: Option<String>,
}

pub fn gguf_metadata(filename: &str) -> GgufMetadata {
    let stem = filename
        .strip_suffix(".gguf")
        .unwrap_or(filename)
        .to_string();
    let quantization = crate::catalog::parse_quant(filename);
    let mut base = stem.clone();
    if let Some(quant) = &quantization {
        if let Some(index) = base.to_ascii_uppercase().find(&quant.to_ascii_uppercase()) {
            base.truncate(index);
            base = base.trim_end_matches(['-', '_', '.', ' ']).to_string();
        }
    }
    if let Some(index) = base.find("-00001-of-") {
        base.truncate(index);
    }
    let parameter_size = stem.split(['-', '_', ' ', '.']).find_map(|part| {
        let upper = part.to_ascii_uppercase();
        let number = upper.strip_suffix('B')?;
        if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit() || c == '.') {
            Some(upper)
        } else {
            None
        }
    });
    GgufMetadata {
        quantization,
        parameter_size,
        base_name: (!base.is_empty()).then_some(base),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_repo_id_traversal() {
        assert!(validate_repo_id("owner/../model").is_err());
        assert!(validate_repo_id("../model").is_err());
        assert!(validate_repo_id("owner/model;echo pwned").is_err());
    }

    #[test]
    fn accepts_revision_branch_with_internal_slash() {
        assert_eq!(
            validate_revision("feature/gguf-v2").unwrap(),
            "feature/gguf-v2"
        );
        assert!(validate_revision("feature/../main").is_err());
        assert!(validate_revision("/main").is_err());
    }

    #[test]
    fn rejects_filename_parent_component() {
        assert!(validate_filename("../../model.gguf").is_err());
        assert!(validate_filename("folder/../model.gguf").is_err());
        assert!(validate_filename("folder\\model.gguf").is_err());
    }

    #[test]
    fn rejects_directory_parent_component() {
        assert!(validate_directory(Path::new("cache/../outside"), "cache_dir").is_err());
        assert!(validate_directory(Path::new("/var/lib/pom-cache"), "cache_dir").is_ok());
    }

    #[test]
    fn redact_sensitive_accepts_owned_text_without_leaking_secrets() {
        let text = redact_sensitive(String::from(
            "HF_TOKEN=hf_owned_secret path=/Users/alice/private-model.gguf",
        ));
        assert!(!text.contains("hf_owned_secret"));
        assert!(text.contains("[REDACTED]"));
    }

    #[test]
    fn parses_quantization_without_guessing() {
        let known = gguf_metadata("Qwen3-32B-Q4_K_M.gguf");
        assert_eq!(known.quantization.as_deref(), Some("Q4_K_M"));
        assert_eq!(known.base_name.as_deref(), Some("Qwen3-32B"));
        assert_eq!(known.parameter_size.as_deref(), Some("32B"));

        let unknown = gguf_metadata("custom-model.gguf");
        assert_eq!(unknown.quantization, None);
        assert_eq!(unknown.parameter_size, None);
    }

    #[test]
    fn unknown_license_is_not_detected() {
        let license = ModelLicense::unknown();
        assert!(!license.detected);
        assert_eq!(license.id, None);
        assert_eq!(license.raw_value, None);
    }

    #[tokio::test]
    async fn manifest_keeps_optional_sha256() {
        let root =
            std::env::temp_dir().join(format!("pom-provider-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("model.gguf");
        std::fs::write(&file, b"gguf-test").unwrap();

        let manifest = LocalModelStorage
            .manifest(
                "huggingface",
                "owner/model",
                Some("main"),
                &[ManifestInput {
                    filename: "model.gguf".into(),
                    path: file.clone(),
                }],
            )
            .await
            .unwrap();

        assert_eq!(manifest.source, "huggingface");
        assert_eq!(manifest.repo_id, "owner/model");
        assert_eq!(manifest.revision.as_deref(), Some("main"));
        assert_eq!(manifest.files[0].filename, "model.gguf");
        assert!(manifest.files[0].sha256.is_some());
        let _ = std::fs::remove_dir_all(root);
    }
}
