//! Runner e parser isolados do executável `hf`.

use crate::model_provider::{
    redact_sensitive, HfError, ModelFile, ModelInfo, ModelLicense, ModelSummary,
};
use async_trait::async_trait;
use serde_json::{Map, Value};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    pub fn new<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: "hf".to_string(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

pub type ProcessObserver = Arc<dyn Fn(OutputStream, String) + Send + Sync>;

#[async_trait]
pub trait HfCommandRunner: Send + Sync {
    async fn run(
        &self,
        spec: CommandSpec,
        observer: ProcessObserver,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, HfError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TokioHfCommandRunner;

#[async_trait]
impl HfCommandRunner for TokioHfCommandRunner {
    async fn run(
        &self,
        spec: CommandSpec,
        observer: ProcessObserver,
        cancel: CancellationToken,
    ) -> Result<ProcessOutput, HfError> {
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    HfError::HfCliNotInstalled
                } else {
                    HfError::Io {
                        message: redact_sensitive(format!("iniciar hf: {error}")),
                    }
                }
            })?;

        let stdout = child.stdout.take().ok_or_else(|| HfError::Io {
            message: "stdout do processo hf não foi conectado".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| HfError::Io {
            message: "stderr do processo hf não foi conectado".into(),
        })?;
        let stdout_task = tokio::spawn(read_pipe(stdout, OutputStream::Stdout, observer.clone()));
        let stderr_task = tokio::spawn(read_pipe(stderr, OutputStream::Stderr, observer));

        let status = tokio::select! {
            result = child.wait() => result.map_err(|error| HfError::Io {
                message: redact_sensitive(format!("esperar hf: {error}")),
            })?,
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(HfError::Cancelled);
            }
        };

        let stdout = stdout_task
            .await
            .map_err(|error| HfError::Io {
                message: redact_sensitive(format!("ler stdout do hf: {error}")),
            })?
            .map_err(|error| HfError::Io {
                message: redact_sensitive(format!("ler stdout do hf: {error}")),
            })?;
        let stderr = stderr_task
            .await
            .map_err(|error| HfError::Io {
                message: redact_sensitive(format!("ler stderr do hf: {error}")),
            })?
            .map_err(|error| HfError::Io {
                message: redact_sensitive(format!("ler stderr do hf: {error}")),
            })?;

        Ok(ProcessOutput {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            exit_code: status.code(),
        })
    }
}

async fn read_pipe<R>(
    mut reader: R,
    stream: OutputStream,
    observer: ProcessObserver,
) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let size = reader.read(&mut buffer).await?;
        if size == 0 {
            return Ok(output);
        }
        output.extend_from_slice(&buffer[..size]);
        observer(
            stream,
            String::from_utf8_lossy(&buffer[..size]).into_owned(),
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAuthentication {
    pub username: Option<String>,
}

pub fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|part| {
            let trimmed = part.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.');
            trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) && trimmed.contains('.')
        })
        .map(|part| {
            part.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                .to_string()
        })
}

pub fn parse_authentication(text: &str) -> Result<ParsedAuthentication, HfError> {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        let username = value
            .get("name")
            .or_else(|| value.get("username"))
            .and_then(Value::as_str)
            .map(str::to_string);
        return username
            .map(|username| {
                Ok(ParsedAuthentication {
                    username: Some(username),
                })
            })
            .unwrap_or_else(|| {
                Err(HfError::HfInvalidResponse {
                    message: "whoami não retornou um usuário".into(),
                })
            });
    }
    let username = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("orgs:"))
        .map(str::to_string);
    if username.is_some() {
        Ok(ParsedAuthentication { username })
    } else {
        Err(HfError::HfInvalidResponse {
            message: "whoami não retornou um usuário".into(),
        })
    }
}

pub fn parse_model_summaries(text: &str) -> Result<Vec<ModelSummary>, HfError> {
    let value = parse_json(text, "lista de modelos")?;
    let items = array_from_value(&value, &["models", "items"]);
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let object = item
            .as_object()
            .ok_or_else(|| invalid_json("modelo não é objeto"))?;
        let repo_id = string_field(object, &["id", "repo_id", "modelId"])
            .ok_or_else(|| invalid_json("modelo sem id"))?;
        result.push(ModelSummary {
            author: string_field(object, &["author", "owner"]),
            description: string_field(object, &["description"]),
            last_modified: string_field(object, &["lastModified", "last_modified"]),
            downloads: integer_field(object, &["downloads", "downloadsAllTime"]),
            likes: integer_field(object, &["likes"]),
            private: bool_field(object, &["private"]),
            gated: bool_field(object, &["gated"]),
            tags: string_array_field(object, "tags"),
            repo_id,
        });
    }
    Ok(result)
}

pub fn parse_model_files(text: &str) -> Result<Vec<ModelFile>, HfError> {
    let value = parse_json(text, "lista de arquivos")?;
    let items = array_from_value(&value, &["siblings", "files", "items"]);
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let object = item
            .as_object()
            .ok_or_else(|| invalid_json("arquivo não é objeto"))?;
        let filename = string_field(object, &["path", "rfilename", "filename", "name"])
            .ok_or_else(|| invalid_json("arquivo sem nome"))?;
        let lfs = object.get("lfs").and_then(Value::as_object);
        let size_bytes = integer_field(object, &["size", "size_bytes"])
            .or_else(|| lfs.and_then(|lfs| integer_field(lfs, &["size"])));
        let sha256 = string_field(object, &["sha256"])
            .or_else(|| lfs.and_then(|lfs| string_field(lfs, &["sha256"])));
        result.push(ModelFile {
            filename,
            size_bytes,
            sha256,
            lfs: Some(lfs.is_some()),
        });
    }
    Ok(result)
}

pub fn parse_model_info(text: &str, fallback_repo_id: &str) -> Result<ModelInfo, HfError> {
    let value = parse_json(text, "informação do modelo")?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_json("informação não é objeto"))?;
    let repo_id = string_field(object, &["id", "repo_id", "modelId"])
        .unwrap_or_else(|| fallback_repo_id.to_string());
    let files = if object.contains_key("siblings") || object.contains_key("files") {
        parse_model_files(&value.to_string())?
    } else {
        Vec::new()
    };
    Ok(ModelInfo {
        sha: string_field(object, &["sha", "sha256"]),
        description: string_field(object, &["description"]),
        last_modified: string_field(object, &["lastModified", "last_modified"]),
        downloads: integer_field(object, &["downloads"]),
        likes: integer_field(object, &["likes"]),
        private: bool_field(object, &["private"]),
        gated: bool_field(object, &["gated"]),
        tags: string_array_field(object, "tags"),
        license: parse_license_value(&value),
        gguf_files: files.iter().filter_map(to_gguf).collect(),
        files,
        repo_id,
    })
}

pub fn parse_license(text: &str) -> Result<ModelLicense, HfError> {
    let value = parse_json(text, "licença")?;
    Ok(parse_license_value(&value))
}

fn parse_license_value(value: &Value) -> ModelLicense {
    let object = value.as_object();
    let card_data = object
        .and_then(|object| object.get("cardData").or_else(|| object.get("card_data")))
        .and_then(Value::as_object);
    let raw = card_data
        .and_then(|data| data.get("license"))
        .or_else(|| object.and_then(|data| data.get("license")));
    let tags = object
        .and_then(|data| data.get("tags"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let tag_license = tags
        .filter_map(Value::as_str)
        .find_map(|tag| tag.strip_prefix("license:").map(str::to_string));

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
            .as_deref()
            .map(|id| license_from_id(id, Some(id.to_string())))
            .unwrap_or_else(ModelLicense::unknown),
    }
}

fn license_from_id(id: &str, raw_value: Option<String>) -> ModelLicense {
    let normalized = id.to_ascii_lowercase();
    let name = match normalized.as_str() {
        "apache-2.0" => Some("Apache License 2.0".to_string()),
        "mit" => Some("MIT License".to_string()),
        "gpl-3.0" => Some("GNU General Public License v3.0".to_string()),
        _ => None,
    };
    let url = match normalized.as_str() {
        "apache-2.0" => Some("https://www.apache.org/licenses/LICENSE-2.0".to_string()),
        "mit" => Some("https://opensource.org/license/mit".to_string()),
        "gpl-3.0" => Some("https://www.gnu.org/licenses/gpl-3.0.html".to_string()),
        _ => None,
    };
    ModelLicense {
        id: Some(id.to_string()),
        name,
        url,
        raw_value,
        detected: true,
    }
}

pub fn last_nonempty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

pub fn classify_failure(text: &str) -> HfError {
    let sanitized = redact_sensitive(text);
    let lower = sanitized.to_ascii_lowercase();
    if lower.contains("429") || lower.contains("rate limit") || lower.contains("too many requests")
    {
        HfError::HfRateLimited
    } else if lower.contains("not logged in")
        || lower.contains("login required")
        || lower.contains("token is required")
        || lower.contains("authentication required")
        || lower.contains("unauthorized")
        || lower.contains("401")
    {
        HfError::HfAuthenticationRequired
    } else if lower.contains("404")
        || lower.contains("repository not found")
        || lower.contains("does not exist")
        || lower.contains("not found")
    {
        HfError::HfRepositoryNotFound
    } else if lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("gated")
        || lower.contains("private repository")
        || lower.contains("access denied")
    {
        HfError::HfAccessDenied
    } else {
        HfError::HfDownloadFailed {
            message: concise_message(&sanitized),
        }
    }
}

fn parse_json(text: &str, what: &str) -> Result<Value, HfError> {
    serde_json::from_str(text).map_err(|error| HfError::HfInvalidResponse {
        message: format!("{what}: {error}"),
    })
}

fn invalid_json(message: &str) -> HfError {
    HfError::HfInvalidResponse {
        message: message.to_string(),
    }
}

fn array_from_value<'a>(value: &'a Value, keys: &[&str]) -> Vec<&'a Value> {
    if let Some(array) = value.as_array() {
        return array.iter().collect();
    }
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_array))
        .map(|array| array.iter().collect())
        .unwrap_or_default()
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

fn concise_message(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .last()
        .unwrap_or("erro desconhecido")
        .chars()
        .take(240)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_and_auth_json() {
        assert_eq!(
            parse_version("hf version 1.2.3\n").as_deref(),
            Some("1.2.3")
        );
        let auth = parse_authentication(r#"{"name":"alice","orgs":["team"]}"#).unwrap();
        assert_eq!(auth.username.as_deref(), Some("alice"));
    }

    #[test]
    fn parses_model_search_and_file_json() {
        let models = parse_model_summaries(
            r#"[{"id":"owner/model","author":"owner","downloads":42,"tags":["gguf"]}]"#,
        )
        .unwrap();
        assert_eq!(models[0].repo_id, "owner/model");

        let files =
            parse_model_files(r#"[{"path":"Qwen-Q4_K_M.gguf","size":123,"lfs":{"sha256":"abc"}}]"#)
                .unwrap();
        assert_eq!(files[0].filename, "Qwen-Q4_K_M.gguf");
        assert_eq!(files[0].size_bytes, Some(123));
        assert_eq!(files[0].sha256.as_deref(), Some("abc"));
    }

    #[test]
    fn parses_public_model_metadata_when_present() {
        let models = parse_model_summaries(
            r#"[{"id":"owner/model","description":"A GGUF model","lastModified":"2026-08-24T10:00:00Z"}]"#,
        )
        .unwrap();
        assert_eq!(models[0].description.as_deref(), Some("A GGUF model"));
        assert_eq!(
            models[0].last_modified.as_deref(),
            Some("2026-08-24T10:00:00Z")
        );

        let info = parse_model_info(
            r#"{"id":"owner/model","description":"Details","lastModified":"2026-08-24T10:00:00Z"}"#,
            "owner/model",
        )
        .unwrap();
        assert_eq!(info.description.as_deref(), Some("Details"));
        assert_eq!(info.last_modified.as_deref(), Some("2026-08-24T10:00:00Z"));
    }

    #[test]
    fn parses_license_metadata_without_inventing_a_default() {
        let license = parse_license(r#"{"license":"custom-license"}"#).unwrap();
        assert!(license.detected);
        assert_eq!(license.id.as_deref(), Some("custom-license"));

        let unknown = parse_license(r#"{"tags":["library:llama.cpp"]}"#).unwrap();
        assert!(!unknown.detected);
    }

    #[test]
    fn download_path_is_last_nonempty_line() {
        assert_eq!(
            last_nonempty_line("progress\n\n/tmp/model.gguf\n"),
            Some("/tmp/model.gguf".into())
        );
    }

    #[test]
    fn classifies_private_repo_without_token_as_authentication() {
        assert_eq!(
            classify_failure("Not logged in. Please run `hf auth login`"),
            HfError::HfAuthenticationRequired
        );
    }

    #[test]
    fn classifies_rate_limit() {
        assert_eq!(
            classify_failure("HTTP 429: rate limit exceeded"),
            HfError::HfRateLimited
        );
    }

    #[test]
    fn redacts_hf_token_from_process_text() {
        let text = redact_sensitive("HF_TOKEN=hf_secret123 Authorization: Bearer hf_other");
        assert!(!text.contains("hf_secret123"));
        assert!(!text.contains("hf_other"));
        assert!(text.contains("[REDACTED]"));
    }

    #[test]
    fn command_spec_keeps_user_value_as_one_argument() {
        let spec = CommandSpec::new(vec!["models", "ls", "owner/model;echo pwned"]);
        assert_eq!(spec.program, "hf");
        assert_eq!(spec.args[2], "owner/model;echo pwned");
        assert_eq!(spec.args.len(), 3);
    }
}
