//! Local-only plugin rebuilds from the mounted source tree.
//!
//! The executable and arguments are fixed here. A mounted repository can
//! provide Cargo source, but cannot choose a shell command, package path, or
//! destination outside its canonical repository directory.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

pub(crate) const DEV_PLUGIN_ROOT: &str = "/opt/pom/dev-plugins";
const MAX_CARGO_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_PLUGIN_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_PLUGIN_BYTES: u64 = 512 * 1024 * 1024;
const CARGO_METADATA_TIMEOUT: Duration = Duration::from_secs(90);
const CARGO_BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy)]
struct KnownPlugin {
    repository: &'static str,
    plugin_code: &'static str,
    display_name: &'static str,
}

/// Repositories accepted by the development workflow. Additions must name a
/// real plugin repository and use the fixed Cargo cdylib build below.
const KNOWN_PLUGINS: &[KnownPlugin] = &[
    KnownPlugin {
        repository: "pom-plugin-base",
        plugin_code: "base",
        display_name: "POM Plugin Base",
    },
    KnownPlugin {
        repository: "pom-plugin-deepseek-harness",
        plugin_code: "deepseek_harness",
        display_name: "DeepSeek Harness",
    },
    KnownPlugin {
        repository: "pom-plugin-harness",
        plugin_code: "harness",
        display_name: "POM Plugin Harness",
    },
    KnownPlugin {
        repository: "pom-plugin-logs",
        plugin_code: "enterprise",
        display_name: "Request Logs and Statistics",
    },
];

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MountedPluginSource {
    pub repository: String,
    pub plugin_code: String,
    pub display_name: String,
    pub active: bool,
    pub install_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MountedPluginSources {
    pub mount_available: bool,
    pub sources: Vec<MountedPluginSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BuildStage {
    Inspect,
    Metadata,
    Compile,
    Install,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DevPluginErrorCode {
    UnknownRepository,
    MountMissing,
    InvalidSource,
    PluginNotActive,
    BuildFailed,
    ArtifactMissing,
    InstallFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DevPluginError {
    pub code: DevPluginErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<BuildStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

impl DevPluginError {
    fn new(code: DevPluginErrorCode) -> Self {
        Self {
            code,
            stage: None,
            exit_code: None,
        }
    }

    fn build_failed(stage: BuildStage, exit_code: Option<i32>) -> Self {
        Self {
            code: DevPluginErrorCode::BuildFailed,
            stage: Some(stage),
            exit_code,
        }
    }
}

#[derive(Debug)]
pub(crate) struct BuiltPlugin {
    pub repository: String,
    pub plugin_code: String,
    pub bytes: Vec<u8>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RebuildInstallResult {
    pub repository: String,
    pub plugin_code: String,
    pub build_ms: u64,
    pub installed_at: String,
}

#[derive(Debug, Clone)]
struct Source {
    spec: KnownPlugin,
    root: PathBuf,
    cargo_manifest: PathBuf,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Debug, Deserialize)]
struct CargoPackage {
    name: String,
    manifest_path: PathBuf,
    targets: Vec<CargoTarget>,
}

#[derive(Debug, Deserialize)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct CommandOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
}

#[async_trait]
pub(crate) trait DevCommandRunner: Send + Sync {
    async fn run(
        &self,
        args: &[String],
        working_directory: &Path,
        target_directory: &Path,
        timeout: Duration,
    ) -> Result<CommandOutput, ()>;
}

pub(crate) struct CargoCommandRunner;

#[async_trait]
impl DevCommandRunner for CargoCommandRunner {
    async fn run(
        &self,
        args: &[String],
        working_directory: &Path,
        target_directory: &Path,
        timeout: Duration,
    ) -> Result<CommandOutput, ()> {
        let capture_stdout = args.first().is_some_and(|arg| arg == "metadata");
        let mut command = tokio::process::Command::new("cargo");
        command
            .args(args)
            .current_dir(working_directory)
            .env("CARGO_TARGET_DIR", target_directory)
            .env("CARGO_TERM_COLOR", "never")
            .stdout(if capture_stdout {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| ())?;
        tokio::time::timeout(timeout, async move {
            let mut stdout = Vec::new();
            if let Some(pipe) = child.stdout.take() {
                use tokio::io::AsyncReadExt;
                let mut bounded = pipe.take(MAX_CARGO_METADATA_BYTES as u64 + 1);
                bounded.read_to_end(&mut stdout).await.map_err(|_| ())?;
                if stdout.len() > MAX_CARGO_METADATA_BYTES {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    return Err(());
                }
            }
            let status = child.wait().await.map_err(|_| ())?;
            Ok(CommandOutput {
                success: status.success(),
                exit_code: status.code(),
                stdout,
            })
        })
        .await
        .map_err(|_| ())?
    }
}

fn known_plugin(repository: &str) -> Option<KnownPlugin> {
    KNOWN_PLUGINS
        .iter()
        .copied()
        .find(|plugin| plugin.repository == repository)
}

pub(crate) fn plugin_code_for_repository(repository: &str) -> Option<&'static str> {
    known_plugin(repository).map(|plugin| plugin.plugin_code)
}

fn canonical_source(root: &Path, repository: &str) -> Result<Source, DevPluginError> {
    let spec = known_plugin(repository)
        .ok_or_else(|| DevPluginError::new(DevPluginErrorCode::UnknownRepository))?;
    let canonical_root = root
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::MountMissing))?;
    if !canonical_root.is_dir() {
        return Err(DevPluginError::new(DevPluginErrorCode::MountMissing));
    }
    let candidate = canonical_root.join(spec.repository);
    let source = candidate
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    if !source.starts_with(&canonical_root)
        || source.file_name().and_then(|name| name.to_str()) != Some(spec.repository)
        || !source.is_dir()
    {
        return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
    }

    let manifest = source.join("Cargo.toml");
    let cargo_manifest = manifest
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    if !cargo_manifest.starts_with(&source) || !cargo_manifest.is_file() {
        return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
    }
    validate_plugin_manifests(&source, spec)?;
    Ok(Source {
        spec,
        root: source,
        cargo_manifest,
    })
}

fn canonical_target_directory(source: &Path) -> Result<PathBuf, DevPluginError> {
    let target = source.join("target");
    std::fs::create_dir_all(&target)
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    let canonical = target
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    if canonical.starts_with(source) && canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(DevPluginError::new(DevPluginErrorCode::InvalidSource))
    }
}

fn validate_plugin_manifests(root: &Path, spec: KnownPlugin) -> Result<(), DevPluginError> {
    let mut found = false;
    for relative in [Path::new("manifest.json"), Path::new("ui/manifest.json")] {
        let path = root.join(relative);
        if !path.exists() {
            continue;
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
        if !canonical.starts_with(root) || !canonical.is_file() {
            return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
        }
        let metadata = std::fs::metadata(&canonical)
            .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
        if metadata.len() > MAX_PLUGIN_MANIFEST_BYTES {
            return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
        }
        let bytes = std::fs::read(canonical)
            .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
        if value.get("plugin_code").and_then(serde_json::Value::as_str) != Some(spec.plugin_code) {
            return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
        }
        found = true;
    }
    if found {
        Ok(())
    } else {
        Err(DevPluginError::new(DevPluginErrorCode::InvalidSource))
    }
}

pub(crate) fn list_mounted_sources(
    root: &Path,
    statuses: &[(String, bool, bool)],
) -> MountedPluginSources {
    let Ok(canonical_root) = root.canonicalize() else {
        return MountedPluginSources {
            mount_available: false,
            sources: Vec::new(),
        };
    };
    if !canonical_root.is_dir() {
        return MountedPluginSources {
            mount_available: false,
            sources: Vec::new(),
        };
    }

    let sources = KNOWN_PLUGINS
        .iter()
        .filter_map(|spec| {
            let source = canonical_source(&canonical_root, spec.repository).ok()?;
            let _ = source;
            let (active, public_install) = statuses
                .iter()
                .find(|(code, _, _)| code == spec.plugin_code)
                .map(|(_, active, public_install)| (*active, *public_install))
                .unwrap_or((false, false));
            Some(MountedPluginSource {
                repository: spec.repository.to_owned(),
                plugin_code: spec.plugin_code.to_owned(),
                display_name: spec.display_name.to_owned(),
                active,
                install_kind: if !active {
                    None
                } else if public_install {
                    Some("github".into())
                } else {
                    Some("licensed".into())
                },
            })
        })
        .collect();
    MountedPluginSources {
        mount_available: true,
        sources,
    }
}

pub(crate) async fn build_source(
    root: &Path,
    repository: &str,
    runner: &dyn DevCommandRunner,
) -> Result<BuiltPlugin, DevPluginError> {
    let started = std::time::Instant::now();
    let source = canonical_source(root, repository)?;
    let target_directory = canonical_target_directory(&source.root)?;

    let metadata_args = vec![
        "metadata".to_owned(),
        "--no-deps".to_owned(),
        "--format-version".to_owned(),
        "1".to_owned(),
        "--locked".to_owned(),
        "--manifest-path".to_owned(),
        source.cargo_manifest.to_string_lossy().into_owned(),
    ];
    let metadata = runner
        .run(
            &metadata_args,
            &source.root,
            &target_directory,
            CARGO_METADATA_TIMEOUT,
        )
        .await
        .map_err(|_| DevPluginError::build_failed(BuildStage::Metadata, None))?;
    if !metadata.success || metadata.stdout.len() > MAX_CARGO_METADATA_BYTES {
        return Err(DevPluginError::build_failed(
            BuildStage::Metadata,
            metadata.exit_code,
        ));
    }
    let metadata: CargoMetadata = serde_json::from_slice(&metadata.stdout)
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    let mut targets = metadata
        .packages
        .into_iter()
        .filter(|package| {
            package
                .manifest_path
                .canonicalize()
                .is_ok_and(|path| path.starts_with(&source.root))
        })
        .flat_map(|package| {
            let package_name = package.name;
            package.targets.into_iter().filter_map(move |target| {
                target
                    .kind
                    .iter()
                    .any(|kind| kind == "cdylib")
                    .then(|| (package_name.clone(), target))
            })
        })
        .collect::<Vec<_>>();
    if targets.len() != 1 {
        return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
    }
    let (package, target) = targets.pop().expect("one target checked");
    if !safe_cargo_name(&package) || !safe_cargo_name(&target.name) {
        return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
    }

    let build_args = vec![
        "build".to_owned(),
        "--release".to_owned(),
        "--locked".to_owned(),
        "--manifest-path".to_owned(),
        source.cargo_manifest.to_string_lossy().into_owned(),
        "--package".to_owned(),
        package,
        "--lib".to_owned(),
    ];
    let build = runner
        .run(
            &build_args,
            &source.root,
            &target_directory,
            CARGO_BUILD_TIMEOUT,
        )
        .await
        .map_err(|_| DevPluginError::build_failed(BuildStage::Compile, None))?;
    if !build.success {
        return Err(DevPluginError::build_failed(
            BuildStage::Compile,
            build.exit_code,
        ));
    }

    let artifact_name = format!(
        "{}{}.{}",
        std::env::consts::DLL_PREFIX,
        target.name,
        std::env::consts::DLL_EXTENSION
    );
    let artifact = target_directory.join("release").join(artifact_name);
    let canonical_artifact = artifact
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::ArtifactMissing))?;
    let metadata = std::fs::metadata(&canonical_artifact)
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::ArtifactMissing))?;
    if !canonical_artifact.starts_with(&source.root)
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_PLUGIN_BYTES
    {
        return Err(DevPluginError::new(DevPluginErrorCode::ArtifactMissing));
    }
    let bytes = std::fs::read(canonical_artifact)
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::ArtifactMissing))?;
    Ok(BuiltPlugin {
        repository: source.spec.repository.to_owned(),
        plugin_code: source.spec.plugin_code.to_owned(),
        bytes,
        elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    })
}

fn safe_cargo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('-')
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeRunner {
        responses: Mutex<VecDeque<Result<CommandOutput, ()>>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<Result<CommandOutput, ()>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl DevCommandRunner for FakeRunner {
        async fn run(
            &self,
            args: &[String],
            _working_directory: &Path,
            target_directory: &Path,
            _timeout: Duration,
        ) -> Result<CommandOutput, ()> {
            self.calls.lock().unwrap().push(args.to_vec());
            let output = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted command")?;
            if args.first().is_some_and(|arg| arg == "build") && output.success {
                let release_dir = target_directory.join("release");
                std::fs::create_dir_all(&release_dir).unwrap();
                let artifact = format!(
                    "{}base_plugin.{}",
                    std::env::consts::DLL_PREFIX,
                    std::env::consts::DLL_EXTENSION
                );
                std::fs::write(release_dir.join(artifact), b"plugin bytes").unwrap();
            }
            Ok(output)
        }
    }

    fn make_source(root: &Path) -> PathBuf {
        let source = root.join("pom-plugin-base");
        std::fs::create_dir_all(source.join("ui")).unwrap();
        std::fs::write(source.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        std::fs::write(
            source.join("ui/manifest.json"),
            r#"{"schema":"pom-plugin-ui/v1","plugin_code":"base"}"#,
        )
        .unwrap();
        source
    }

    #[test]
    fn mounted_source_paths_reject_traversal_unknown_names_and_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        make_source(temp.path());
        assert!(matches!(
            canonical_source(temp.path(), "../pom-plugin-base")
                .unwrap_err()
                .code,
            DevPluginErrorCode::UnknownRepository
        ));
        assert!(matches!(
            canonical_source(temp.path(), "not-a-plugin")
                .unwrap_err()
                .code,
            DevPluginErrorCode::UnknownRepository
        ));

        #[cfg(unix)]
        {
            let target_outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(
                target_outside.path(),
                temp.path().join("pom-plugin-base/target"),
            )
            .unwrap();
            assert!(matches!(
                canonical_target_directory(&temp.path().join("pom-plugin-base"))
                    .unwrap_err()
                    .code,
                DevPluginErrorCode::InvalidSource
            ));
            std::fs::remove_file(temp.path().join("pom-plugin-base/target")).unwrap();

            let outside = tempfile::tempdir().unwrap();
            std::fs::remove_dir_all(temp.path().join("pom-plugin-base")).unwrap();
            std::os::unix::fs::symlink(outside.path(), temp.path().join("pom-plugin-base"))
                .unwrap();
            assert!(matches!(
                canonical_source(temp.path(), "pom-plugin-base")
                    .unwrap_err()
                    .code,
                DevPluginErrorCode::InvalidSource
            ));
        }
    }

    #[test]
    fn missing_mount_is_reported_without_listing_unmounted_plugins() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("absent");
        let listing = list_mounted_sources(&missing, &[]);
        assert!(!listing.mount_available);
        assert!(listing.sources.is_empty());
    }

    #[test]
    fn source_listing_only_exposes_valid_allowlisted_directories_and_install_state() {
        let temp = tempfile::tempdir().unwrap();
        make_source(temp.path());
        std::fs::create_dir_all(temp.path().join("pom-plugin-not-allowed")).unwrap();
        let listing = list_mounted_sources(temp.path(), &[("base".into(), true, true)]);
        assert!(listing.mount_available);
        assert_eq!(listing.sources.len(), 1);
        assert_eq!(listing.sources[0].repository, "pom-plugin-base");
        assert!(listing.sources[0].active);
        assert_eq!(listing.sources[0].install_kind.as_deref(), Some("github"));
    }

    #[tokio::test]
    async fn build_uses_fixed_cargo_arguments_and_returns_only_the_in_tree_cdylib() {
        let temp = tempfile::tempdir().unwrap();
        let source = make_source(temp.path());
        let manifest = source.join("Cargo.toml").canonicalize().unwrap();
        let metadata = serde_json::json!({
            "packages": [{
                "name": "base-plugin",
                "manifest_path": manifest,
                "targets": [{"name":"base_plugin","kind":["cdylib"]}]
            }]
        });
        let runner = FakeRunner::new(vec![
            Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: serde_json::to_vec(&metadata).unwrap(),
            }),
            Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: Vec::new(),
            }),
        ]);

        let built = build_source(temp.path(), "pom-plugin-base", &runner)
            .await
            .unwrap();
        assert_eq!(built.repository, "pom-plugin-base");
        assert_eq!(built.plugin_code, "base");
        assert_eq!(built.bytes, b"plugin bytes");
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            [
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--locked",
                "--manifest-path",
                manifest.to_str().unwrap(),
            ]
        );
        assert_eq!(
            calls[1],
            [
                "build",
                "--release",
                "--locked",
                "--manifest-path",
                manifest.to_str().unwrap(),
                "--package",
                "base-plugin",
                "--lib",
            ]
        );
    }

    #[tokio::test]
    async fn build_failure_reports_only_safe_stage_and_exit_code() {
        let temp = tempfile::tempdir().unwrap();
        let source = make_source(temp.path());
        let metadata = serde_json::json!({
            "packages": [{
                "name": "base-plugin",
                "manifest_path": source.join("Cargo.toml"),
                "targets": [{"name":"base_plugin","kind":["cdylib"]}]
            }]
        });
        let runner = FakeRunner::new(vec![
            Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: serde_json::to_vec(&metadata).unwrap(),
            }),
            Ok(CommandOutput {
                success: false,
                exit_code: Some(101),
                stdout: Vec::new(),
            }),
        ]);
        let error = build_source(temp.path(), "pom-plugin-base", &runner)
            .await
            .unwrap_err();
        assert_eq!(error.code, DevPluginErrorCode::BuildFailed);
        assert_eq!(error.stage, Some(BuildStage::Compile));
        assert_eq!(error.exit_code, Some(101));
        assert!(!serde_json::to_string(&error).unwrap().contains("stderr"));
    }
}
