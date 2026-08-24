//! Provider Hugging Face baseado no executável `hf`.

use crate::hf_cli::{
    classify_failure, last_nonempty_line, parse_authentication, parse_license, parse_model_files,
    parse_model_info, parse_model_summaries, parse_version, CommandSpec, HfCommandRunner,
    OutputStream, ProcessObserver, ProcessOutput, TokioHfCommandRunner,
};
use crate::model_provider::{
    validate_directory, validate_filename, validate_filter, validate_repo_id, validate_revision,
    DownloadOptions, DownloadProgress, DownloadResult, DownloadStatus, HfCliStatus, HfError,
    LocalModelStorage, ManifestInput, ModelFile, ModelFiles, ModelInfo, ModelLicense,
    ModelProvider, ModelStorage, ModelSummary, ProgressSender,
};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

pub struct HuggingFaceProvider {
    runner: Arc<dyn HfCommandRunner>,
    storage: Arc<dyn ModelStorage>,
}

impl HuggingFaceProvider {
    pub fn new() -> Self {
        Self::with_dependencies(Arc::new(TokioHfCommandRunner), Arc::new(LocalModelStorage))
    }

    pub fn with_runner(runner: Arc<dyn HfCommandRunner>) -> Self {
        Self::with_dependencies(runner, Arc::new(LocalModelStorage))
    }

    pub fn with_dependencies(
        runner: Arc<dyn HfCommandRunner>,
        storage: Arc<dyn ModelStorage>,
    ) -> Self {
        Self { runner, storage }
    }

    async fn run_checked(
        &self,
        spec: CommandSpec,
        progress: Option<ProgressSender>,
        cancel: CancellationToken,
        operation: &str,
    ) -> Result<ProcessOutput, HfError> {
        if cancel.is_cancelled() {
            return Err(HfError::Cancelled);
        }
        let observer = progress_observer(progress);
        let output = self.runner.run(spec, observer, cancel).await?;
        if output.exit_code != Some(0) {
            return Err(map_process_failure(operation, &output));
        }
        Ok(output)
    }

    async fn run_status(&self, spec: CommandSpec) -> Result<ProcessOutput, HfError> {
        self.runner
            .run(spec, Arc::new(|_, _| {}), CancellationToken::new())
            .await
    }

    fn manifest_inputs(path: &Path, requested: &[String]) -> Result<Vec<ManifestInput>, HfError> {
        if path.is_file() {
            let filename = requested
                .first()
                .cloned()
                .or_else(|| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_string)
                })
                .ok_or_else(|| HfError::HfInvalidResponse {
                    message: "download retornou arquivo sem nome".into(),
                })?;
            validate_filename(&filename)?;
            return Ok(vec![ManifestInput {
                filename,
                path: path.to_path_buf(),
            }]);
        }
        if !path.is_dir() {
            return Err(HfError::HfInvalidResponse {
                message: "download não retornou caminho local existente".into(),
            });
        }

        let mut paths = Vec::new();
        walk_files(path, path, &mut paths)?;
        let mut inputs = Vec::with_capacity(paths.len());
        for local_path in paths {
            let relative =
                local_path
                    .strip_prefix(path)
                    .map_err(|_| HfError::HfInvalidResponse {
                        message: "arquivo baixado está fora da raiz retornada".into(),
                    })?;
            let filename = relative.to_string_lossy().replace('\\', "/");
            validate_filename(&filename)?;
            inputs.push(ManifestInput {
                filename,
                path: local_path,
            });
        }
        if inputs.is_empty() {
            return Err(HfError::HfInvalidResponse {
                message: "download não retornou arquivos regulares".into(),
            });
        }
        Ok(inputs)
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
        let output = self
            .run_checked(
                CommandSpec::new(["models", "ls", "--search", query, "--format", "json"]),
                None,
                CancellationToken::new(),
                "search",
            )
            .await?;
        parse_model_summaries(&output.stdout)
    }

    async fn get_model_info(&self, repo_id: &str) -> Result<ModelInfo, HfError> {
        let repo_id = validate_repo_id(repo_id)?;
        let output = self
            .run_checked(
                CommandSpec::new(["models", "info", &repo_id, "--format", "json"]),
                None,
                CancellationToken::new(),
                "info",
            )
            .await?;
        let mut info = parse_model_info(&output.stdout, &repo_id)?;
        if info.files.is_empty() {
            let files = self.list_files(&repo_id, None).await?;
            info.files = files.files;
            info.gguf_files = files.gguf_files;
        }
        Ok(info)
    }

    async fn list_files(
        &self,
        repo_id: &str,
        revision: Option<&str>,
    ) -> Result<ModelFiles, HfError> {
        let repo_id = validate_repo_id(repo_id)?;
        let revision = revision.map(validate_revision).transpose()?;
        let mut args = vec!["models".to_string(), "ls".to_string(), repo_id.clone()];
        if let Some(revision) = &revision {
            args.extend(["--revision".into(), revision.clone()]);
        }
        args.extend(["--format".into(), "json".into()]);
        let output = self
            .run_checked(
                CommandSpec::new(args),
                None,
                CancellationToken::new(),
                "files",
            )
            .await?;
        let files = parse_model_files(&output.stdout)?;
        let gguf_files = files.iter().filter_map(to_gguf).collect();
        Ok(ModelFiles {
            repo_id,
            revision,
            files,
            gguf_files,
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
        let started = Instant::now();
        Self::emit(
            &progress,
            DownloadProgress {
                status: DownloadStatus::Starting,
                current_file: options.filenames.first().cloned(),
                bytes_downloaded: None,
                bytes_total: None,
                percent: None,
                speed_bytes_per_sec: None,
            },
        );

        let mut args = vec!["download".to_string(), repo_id.clone()];
        args.extend(options.filenames.iter().cloned());
        if let Some(revision) = &options.revision {
            args.extend(["--revision".into(), revision.clone()]);
        }
        if let Some(cache_dir) = &options.cache_dir {
            args.extend([
                "--cache-dir".into(),
                cache_dir.to_string_lossy().into_owned(),
            ]);
        }
        if let Some(local_dir) = &options.local_dir {
            args.extend([
                "--local-dir".into(),
                local_dir.to_string_lossy().into_owned(),
            ]);
        }
        for include in &options.include {
            args.extend(["--include".into(), include.clone()]);
        }
        for exclude in &options.exclude {
            args.extend(["--exclude".into(), exclude.clone()]);
        }

        let output = match self
            .run_checked(CommandSpec::new(args), progress.clone(), cancel, "download")
            .await
        {
            Ok(output) => output,
            Err(error) => {
                Self::emit(
                    &progress,
                    DownloadProgress {
                        status: if matches!(&error, HfError::Cancelled) {
                            DownloadStatus::Cancelled
                        } else {
                            DownloadStatus::Failed
                        },
                        current_file: options.filenames.first().cloned(),
                        bytes_downloaded: None,
                        bytes_total: None,
                        percent: None,
                        speed_bytes_per_sec: None,
                    },
                );
                tracing::warn!(
                    provider = "huggingface",
                    repo_id = %repo_id,
                    revision = ?options.revision,
                    status = "error",
                    error_type = error_type(&error),
                    "download de modelo falhou"
                );
                return Err(error);
            }
        };

        let path_text = last_nonempty_line(&output.stdout)
            .or_else(|| last_nonempty_line(&output.stderr))
            .ok_or_else(|| HfError::HfInvalidResponse {
                message: "download não retornou caminho local".into(),
            })?;
        let path = PathBuf::from(path_text);
        let inputs = match Self::manifest_inputs(&path, &options.filenames) {
            Ok(inputs) => inputs,
            Err(error) => {
                tracing::warn!(
                    provider = "huggingface",
                    repo_id = %repo_id,
                    revision = ?options.revision,
                    status = "error",
                    error_type = error_type(&error),
                    "download retornou conteúdo local inválido"
                );
                return Err(error);
            }
        };
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
        let local_paths = files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        let total_bytes = files.iter().map(|file| file.size_bytes).sum();
        let duration_ms = started.elapsed().as_millis() as u64;
        let speed = (total_bytes > 0 && duration_ms > 0)
            .then_some(total_bytes.saturating_mul(1000) / duration_ms);
        Self::emit(
            &progress,
            DownloadProgress {
                status: DownloadStatus::Completed,
                current_file: options.filenames.first().cloned(),
                bytes_downloaded: Some(total_bytes),
                bytes_total: Some(total_bytes),
                percent: Some(100.0),
                speed_bytes_per_sec: speed,
            },
        );
        tracing::info!(
            provider = "huggingface",
            repo_id = %repo_id,
            revision = ?options.revision,
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
        let spec = CommandSpec::new(["models", "card", &repo_id, "--metadata", "--format", "json"]);
        match self
            .run_checked(spec, None, CancellationToken::new(), "license")
            .await
        {
            Ok(output) => parse_license(&output.stdout),
            Err(error)
                if matches!(
                    &error,
                    HfError::HfInvalidResponse { .. } | HfError::HfDownloadFailed { .. }
                ) =>
            {
                Ok(self.get_model_info(&repo_id).await?.license)
            }
            Err(error) => Err(error),
        }
    }

    async fn check_authentication(&self) -> Result<HfCliStatus, HfError> {
        let version = match self.run_status(CommandSpec::new(["--version"])).await {
            Err(HfError::HfCliNotInstalled) => return Ok(HfCliStatus::not_installed()),
            Err(error) => return Err(error),
            Ok(output) => parse_version(&format!("{}\n{}", output.stdout, output.stderr)),
        };

        let auth = self
            .run_status(CommandSpec::new(["auth", "whoami", "--format", "json"]))
            .await?;
        if auth.exit_code != Some(0) {
            let error = classify_failure(&format!("{}\n{}", auth.stderr, auth.stdout));
            if matches!(error, HfError::HfCliNotInstalled) {
                return Ok(HfCliStatus::not_installed());
            }
            return Ok(HfCliStatus {
                installed: true,
                version,
                authenticated: false,
                username: None,
            });
        }
        let identity = parse_authentication(&auth.stdout)?;
        Ok(HfCliStatus {
            installed: true,
            version,
            authenticated: true,
            username: identity.username,
        })
    }
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

fn progress_observer(progress: Option<ProgressSender>) -> ProcessObserver {
    Arc::new(move |stream, chunk| {
        let Some(progress) = &progress else { return };
        let Some(event) = parse_progress_chunk(stream, &chunk) else {
            return;
        };
        let _ = progress.send(event);
    })
}

fn parse_progress_chunk(_stream: OutputStream, chunk: &str) -> Option<DownloadProgress> {
    let percent_index = chunk.find('%')?;
    let before = &chunk[..percent_index];
    let digits = before
        .rsplit(|character: char| !character.is_ascii_digit() && character != '.')
        .next()?;
    let percent = digits.parse::<f64>().ok()?.clamp(0.0, 100.0);
    Some(DownloadProgress {
        status: DownloadStatus::Downloading,
        current_file: None,
        bytes_downloaded: None,
        bytes_total: None,
        percent: Some(percent),
        speed_bytes_per_sec: None,
    })
}

fn map_process_failure(operation: &str, output: &ProcessOutput) -> HfError {
    let failure = classify_failure(&format!("{}\n{}", output.stderr, output.stdout));
    match failure {
        HfError::HfDownloadFailed { message } => HfError::HfDownloadFailed {
            message: format!("{operation}: {message}"),
        },
        other => other,
    }
}

fn error_type(error: &HfError) -> &'static str {
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
        HfError::Io { .. } => "io",
        HfError::Cancelled => "cancelled",
    }
}

fn walk_files(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<(), HfError> {
    let entries = std::fs::read_dir(current).map_err(|error| HfError::Io {
        message: crate::model_provider::redact_sensitive(format!("{}: {error}", current.display())),
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| HfError::Io {
            message: crate::model_provider::redact_sensitive(format!(
                "listar {}: {error}",
                current.display()
            )),
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| HfError::Io {
            message: crate::model_provider::redact_sensitive(format!(
                "{}: {error}",
                path.display()
            )),
        })?;
        if file_type.is_dir() {
            walk_files(root, &path, out)?;
            continue;
        }
        if path
            .strip_prefix(root)
            .ok()
            .and_then(|relative| relative.components().next())
            .is_some_and(|component| component.as_os_str() == ".cache")
        {
            continue;
        }
        if std::fs::metadata(&path)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hf_cli::{
        CommandSpec, HfCommandRunner, OutputStream, ProcessObserver, ProcessOutput,
    };
    use crate::model_provider::DownloadOptions;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakeRunner {
        commands: Arc<Mutex<Vec<CommandSpec>>>,
        outputs: Arc<Mutex<VecDeque<Result<ProcessOutput, HfError>>>>,
    }

    impl FakeRunner {
        fn with_outputs(outputs: Vec<Result<ProcessOutput, HfError>>) -> Self {
            Self {
                commands: Arc::new(Mutex::new(Vec::new())),
                outputs: Arc::new(Mutex::new(outputs.into_iter().collect())),
            }
        }

        fn commands(&self) -> Vec<CommandSpec> {
            self.commands.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl HfCommandRunner for FakeRunner {
        async fn run(
            &self,
            spec: CommandSpec,
            observer: ProcessObserver,
            cancel: CancellationToken,
        ) -> Result<ProcessOutput, HfError> {
            self.commands.lock().unwrap().push(spec);
            if cancel.is_cancelled() {
                observer(OutputStream::Stderr, "cancelled".into());
                return Err(HfError::Cancelled);
            }
            self.outputs.lock().unwrap().pop_front().unwrap_or_else(|| {
                Err(HfError::HfInvalidResponse {
                    message: "fake vazio".into(),
                })
            })
        }
    }

    fn output(stdout: &str) -> Result<ProcessOutput, HfError> {
        Ok(ProcessOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }

    fn failed(stderr: &str) -> Result<ProcessOutput, HfError> {
        Ok(ProcessOutput {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code: Some(1),
        })
    }

    #[tokio::test]
    async fn reports_missing_cli_as_structured_status() {
        let fake = FakeRunner::with_outputs(vec![Err(HfError::HfCliNotInstalled)]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake));
        let status = provider.check_authentication().await.unwrap();
        assert!(!status.installed);
        assert!(!status.authenticated);
        assert_eq!(status.version, None);
    }

    #[tokio::test]
    async fn reports_unauthenticated_cli_without_leaking_token() {
        let secret = "hf_secret_not_for_logs";
        let fake = FakeRunner::with_outputs(vec![
            output("hf version 1.2.3"),
            failed(&format!("Not logged in HF_TOKEN={secret}")),
        ]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake));
        let status = provider.check_authentication().await.unwrap();
        assert!(status.installed);
        assert!(!status.authenticated);
        assert!(!format!("{status:?}").contains(secret));
    }

    #[tokio::test]
    async fn searches_with_json_arguments() {
        let fake = FakeRunner::with_outputs(vec![output(
            r#"[{"id":"owner/model","downloads":7,"tags":["gguf"]}]"#,
        )]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake.clone()));
        let models = provider.search_models("qwen;echo pwned").await.unwrap();
        assert_eq!(models[0].repo_id, "owner/model");
        assert_eq!(
            fake.commands()[0].args,
            vec![
                "models",
                "ls",
                "--search",
                "qwen;echo pwned",
                "--format",
                "json"
            ]
        );
    }

    #[tokio::test]
    async fn maps_repository_not_found() {
        let fake = FakeRunner::with_outputs(vec![failed("404 repository not found")]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake));
        assert_eq!(
            provider.list_files("owner/model", None).await.unwrap_err(),
            HfError::HfRepositoryNotFound
        );
    }

    #[tokio::test]
    async fn lists_gguf_and_parses_quantization() {
        let fake = FakeRunner::with_outputs(vec![output(
            r#"[{"path":"Qwen3-32B-Q4_K_M.gguf","size":123}]"#,
        )]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake));
        let files = provider
            .list_files("owner/model", Some("main"))
            .await
            .unwrap();
        assert_eq!(files.gguf_files[0].quantization.as_deref(), Some("Q4_K_M"));
    }

    #[tokio::test]
    async fn downloads_file_and_builds_manifest() {
        let root = std::env::temp_dir().join(format!("pom-hf-provider-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("model.gguf");
        std::fs::write(&file, b"downloaded-model").unwrap();
        let fake = FakeRunner::with_outputs(vec![output(&format!("{}\n", file.display()))]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake.clone()));
        let mut options = DownloadOptions::default();
        options.filenames.push("model.gguf".into());
        let result = provider
            .download_model("owner/model", options, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.repo_id, "owner/model");
        assert_eq!(result.total_bytes, b"downloaded-model".len() as u64);
        assert_eq!(
            result.manifest.files[0].sha256.as_deref().map(str::len),
            Some(64)
        );
        assert!(fake.commands()[0].args.contains(&"download".into()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn maps_download_failure() {
        let fake = FakeRunner::with_outputs(vec![failed("download failed: network unavailable")]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake));
        let error = provider
            .download_model(
                "owner/model",
                DownloadOptions::default(),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, HfError::HfDownloadFailed { .. }));
    }

    #[tokio::test]
    async fn cancellation_stops_download() {
        let fake = FakeRunner::with_outputs(vec![output("/tmp/unused")]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            provider
                .download_model("owner/model", DownloadOptions::default(), None, cancel)
                .await
                .unwrap_err(),
            HfError::Cancelled
        );
    }

    #[tokio::test]
    async fn rejects_repo_revision_and_filename_traversal() {
        let fake = FakeRunner::with_outputs(vec![]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake));
        assert!(provider.list_files("../model", None).await.is_err());
        let mut options = DownloadOptions::default();
        options.revision = Some("feature/../main".into());
        assert!(provider
            .download_model("owner/model", options, None, CancellationToken::new())
            .await
            .is_err());
        assert!(provider
            .download_file(
                "owner/model",
                "../model.gguf",
                DownloadOptions::default(),
                None,
                CancellationToken::new()
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn telemetry_and_errors_do_not_contain_token() {
        let secret = "hf_private_secret";
        let fake = FakeRunner::with_outputs(vec![failed(&format!("forbidden token={secret}"))]);
        let provider = HuggingFaceProvider::with_runner(Arc::new(fake));
        let error = provider
            .download_model(
                "owner/model",
                DownloadOptions::default(),
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }
}
