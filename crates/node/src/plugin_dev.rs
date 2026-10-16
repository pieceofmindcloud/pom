//! Local-only plugin rebuilds from the mounted source tree.
//!
//! The executable and arguments are fixed here. A mounted repository provides
//! Cargo source, but cannot choose a shell command, package path, or build
//! destination. Artifacts go to a POM-managed build directory outside the source.

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
/// Each rebuild step a plugin declares (`development.rebuild` in its manifest).
const PREPARE_STEP_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const MAX_PREPARE_STEPS: usize = 8;
const MAX_PREPARE_ARGS: usize = 16;
const MAX_PREPARE_ARG_CHARS: usize = 256;
/// The writable folder, inside the build directory, where rebuild steps write
/// (the mounted source is read-only) and that `cargo build` is told about.
const PREPARE_DIRECTORY: &str = "pom-prepare";

/// A source name is a single safe repository slug under the mounted plugin
/// root. Its plugin identity comes from the validated manifest, not a catalog
/// compiled into the node.
fn is_plugin_repository_name(repository: &str) -> bool {
    let Some(slug) = repository.strip_prefix("pom-plugin-") else {
        return false;
    };
    let bytes = slug.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.'))
}

fn display_name_for_repository(repository: &str) -> String {
    repository
        .strip_prefix("pom-plugin-")
        .unwrap_or(repository)
        .split(|character| matches!(character, '-' | '_' | '.'))
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            match characters.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), characters.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

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
    Prepare,
    Compile,
    Install,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DevPluginErrorCode {
    UnknownRepository,
    MountMissing,
    InvalidSource,
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
    repository: String,
    plugin_code: String,
    display_name: String,
    root: PathBuf,
    cargo_manifest: PathBuf,
    /// The manifest's rebuild steps for this machine's operating system.
    prepare: Vec<PrepareStep>,
}

/// One `development.rebuild` entry: a script inside the source and its
/// arguments, run before `cargo build`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrepareStep {
    program: PathBuf,
    args: Vec<String>,
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
    /// `cargo` with `args`; `env` is added to its environment.
    async fn run(
        &self,
        args: &[String],
        working_directory: &Path,
        target_directory: &Path,
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<CommandOutput, ()>;

    /// A rebuild step, its output appended to `log`.
    async fn run_step(
        &self,
        step: &PrepareStep,
        working_directory: &Path,
        env: &[(String, String)],
        log: &Path,
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
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<CommandOutput, ()> {
        let capture_stdout = args.first().is_some_and(|arg| arg == "metadata");
        let mut command = tokio::process::Command::new("cargo");
        command
            .args(args)
            .current_dir(working_directory)
            .envs(env.iter().map(|(key, value)| (key, value)))
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

    async fn run_step(
        &self,
        step: &PrepareStep,
        working_directory: &Path,
        env: &[(String, String)],
        log: &Path,
        timeout: Duration,
    ) -> Result<CommandOutput, ()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|_| ())?;
        let _ = writeln!(file, "$ {} {}", step.program.display(), step.args.join(" "));
        let stderr = file.try_clone().map_err(|_| ())?;
        // A shell script runs through bash, so it needs no executable bit.
        let mut command = if step
            .program
            .extension()
            .is_some_and(|extension| extension == "sh")
        {
            let mut command = tokio::process::Command::new("bash");
            command.arg(&step.program);
            command
        } else {
            tokio::process::Command::new(&step.program)
        };
        command
            .args(&step.args)
            .current_dir(working_directory)
            .envs(env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(stderr)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| ())?;
        let status = tokio::time::timeout(timeout, child.wait())
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
        Ok(CommandOutput {
            success: status.success(),
            exit_code: status.code(),
            stdout: Vec::new(),
        })
    }
}

fn canonical_source(root: &Path, repository: &str) -> Result<Source, DevPluginError> {
    if !is_plugin_repository_name(repository) {
        return Err(DevPluginError::new(DevPluginErrorCode::UnknownRepository));
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::MountMissing))?;
    if !canonical_root.is_dir() {
        return Err(DevPluginError::new(DevPluginErrorCode::MountMissing));
    }
    let candidate = canonical_root.join(repository);
    let source = candidate
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    if !source.starts_with(&canonical_root)
        || source.file_name().and_then(|name| name.to_str()) != Some(repository)
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
    let (plugin_code, prepare) = validate_plugin_manifests(&source)?;
    Ok(Source {
        repository: repository.to_owned(),
        plugin_code,
        display_name: display_name_for_repository(repository),
        root: source,
        cargo_manifest,
        prepare,
    })
}

fn canonical_target_directory(
    build_root: &Path,
    repository: &str,
    source_root: &Path,
) -> Result<PathBuf, DevPluginError> {
    if !is_plugin_repository_name(repository) {
        return Err(DevPluginError::new(DevPluginErrorCode::UnknownRepository));
    }
    std::fs::create_dir_all(build_root)
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    let canonical_root = build_root
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    let canonical_source = source_root
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    if !canonical_root.is_dir()
        || canonical_root.starts_with(&canonical_source)
        || canonical_source.starts_with(&canonical_root)
    {
        return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
    }

    let target = canonical_root.join(repository);
    std::fs::create_dir_all(&target)
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    let canonical = target
        .canonicalize()
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    if canonical.parent() == Some(canonical_root.as_path())
        && canonical.file_name().and_then(|name| name.to_str()) == Some(repository)
        && canonical.is_dir()
    {
        Ok(canonical)
    } else {
        Err(DevPluginError::new(DevPluginErrorCode::InvalidSource))
    }
}

/// The platform name rebuild steps get in `POM_PLUGIN_PLATFORM`, as plugin
/// release assets name it (`linux-x86_64`, `macos-aarch64`, ...).
fn build_platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// The `development.rebuild` steps of a manifest that run on this operating
/// system. A step is `{"run": [script, args...], "os": [...]}`: the script is a
/// file inside the source (no absolute path, no `..`), the arguments plain
/// strings, `os` (`linux`, `macos`, `windows`) optional.
fn rebuild_steps(
    root: &Path,
    manifest: &serde_json::Value,
) -> Result<Vec<PrepareStep>, DevPluginError> {
    let invalid = || DevPluginError::new(DevPluginErrorCode::InvalidSource);
    let Some(steps) = manifest
        .get("development")
        .and_then(|value| value.get("rebuild"))
    else {
        return Ok(Vec::new());
    };
    let steps = steps.as_array().ok_or_else(invalid)?;
    if steps.len() > MAX_PREPARE_STEPS {
        return Err(invalid());
    }
    let mut out = Vec::new();
    for step in steps {
        if let Some(systems) = step.get("os") {
            let systems = systems.as_array().ok_or_else(invalid)?;
            if !systems
                .iter()
                .any(|system| system.as_str() == Some(std::env::consts::OS))
            {
                continue;
            }
        }
        let run = step
            .get("run")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(invalid)?;
        let mut words = Vec::new();
        for word in run {
            let word = word.as_str().ok_or_else(invalid)?;
            if word.is_empty()
                || word.chars().count() > MAX_PREPARE_ARG_CHARS
                || word.contains('\0')
            {
                return Err(invalid());
            }
            words.push(word.to_owned());
        }
        if words.is_empty() || words.len() > MAX_PREPARE_ARGS + 1 {
            return Err(invalid());
        }
        let script = Path::new(&words[0]);
        if !script
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return Err(invalid());
        }
        let program = root.join(script).canonicalize().map_err(|_| invalid())?;
        if !program.starts_with(root) || !program.is_file() {
            return Err(invalid());
        }
        out.push(PrepareStep {
            program,
            args: words[1..].to_vec(),
        });
    }
    Ok(out)
}

fn validate_plugin_manifests(root: &Path) -> Result<(String, Vec<PrepareStep>), DevPluginError> {
    let mut plugin_code = None;
    let mut prepare = Vec::new();
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
        let Some(code) = value
            .get("plugin_code")
            .and_then(serde_json::Value::as_str)
            .filter(|code| crate::plugins::is_valid_plugin_code(code))
        else {
            return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
        };
        if plugin_code
            .as_deref()
            .is_some_and(|existing| existing != code)
        {
            return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
        }
        plugin_code = Some(code.to_owned());
        prepare.extend(rebuild_steps(root, &value)?);
    }
    if prepare.len() > MAX_PREPARE_STEPS {
        return Err(DevPluginError::new(DevPluginErrorCode::InvalidSource));
    }
    let plugin_code =
        plugin_code.ok_or_else(|| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    Ok((plugin_code, prepare))
}

pub(crate) fn list_mounted_sources(
    root: &Path,
    statuses: &[(String, bool, bool, bool)],
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

    let mut sources = std::fs::read_dir(&canonical_root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|repository| is_plugin_repository_name(repository))
        .filter_map(|repository| {
            let source = canonical_source(&canonical_root, &repository).ok()?;
            let (active, public_install, development_install) = statuses
                .iter()
                .find(|(code, _, _, _)| code == &source.plugin_code)
                .map(|(_, active, public_install, development_install)| {
                    (*active, *public_install, *development_install)
                })
                .unwrap_or((false, false, false));
            Some(MountedPluginSource {
                repository: source.repository,
                plugin_code: source.plugin_code,
                display_name: source.display_name,
                active,
                install_kind: if !active {
                    None
                } else if development_install {
                    Some("development".into())
                } else if public_install {
                    Some("github".into())
                } else {
                    Some("licensed".into())
                },
            })
        })
        .collect::<Vec<_>>();
    sources.sort_by(|left, right| left.repository.cmp(&right.repository));
    MountedPluginSources {
        mount_available: true,
        sources,
    }
}

pub(crate) async fn build_source(
    root: &Path,
    build_root: &Path,
    repository: &str,
    runner: &dyn DevCommandRunner,
) -> Result<BuiltPlugin, DevPluginError> {
    let started = std::time::Instant::now();
    let source = canonical_source(root, repository)?;
    let target_directory = canonical_target_directory(build_root, repository, &source.root)?;

    let metadata_args = vec![
        "metadata".to_owned(),
        "--no-deps".to_owned(),
        "--format-version".to_owned(),
        "1".to_owned(),
        "--locked".to_owned(),
        "--manifest-path".to_owned(),
        source.cargo_manifest.to_string_lossy().into_owned(),
    ];
    let prepare_directory = target_directory.join(PREPARE_DIRECTORY);
    std::fs::create_dir_all(&prepare_directory)
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::InvalidSource))?;
    // What rebuild steps and the plugin's build script are told: where to
    // write (the source is read-only), the platform, and the source itself.
    let env = vec![
        (
            "POM_PLUGIN_OUT_DIR".to_owned(),
            prepare_directory.to_string_lossy().into_owned(),
        ),
        ("POM_PLUGIN_PLATFORM".to_owned(), build_platform()),
        (
            "POM_PLUGIN_SOURCE_DIR".to_owned(),
            source.root.to_string_lossy().into_owned(),
        ),
    ];
    let metadata = runner
        .run(
            &metadata_args,
            &source.root,
            &target_directory,
            &env,
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

    let log = prepare_directory.join("rebuild.log");
    if !source.prepare.is_empty() {
        let _ = std::fs::write(&log, b"");
    }
    for step in &source.prepare {
        let output = runner
            .run_step(step, &source.root, &env, &log, PREPARE_STEP_TIMEOUT)
            .await
            .map_err(|_| DevPluginError::build_failed(BuildStage::Prepare, None))?;
        if !output.success {
            return Err(DevPluginError::build_failed(
                BuildStage::Prepare,
                output.exit_code,
            ));
        }
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
            &env,
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
    if !canonical_artifact.starts_with(&target_directory)
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_PLUGIN_BYTES
    {
        return Err(DevPluginError::new(DevPluginErrorCode::ArtifactMissing));
    }
    let bytes = std::fs::read(canonical_artifact)
        .map_err(|_| DevPluginError::new(DevPluginErrorCode::ArtifactMissing))?;
    Ok(BuiltPlugin {
        repository: source.repository,
        plugin_code: source.plugin_code,
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
        envs: Mutex<Vec<Vec<(String, String)>>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<Result<CommandOutput, ()>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(Vec::new()),
                envs: Mutex::new(Vec::new()),
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
            env: &[(String, String)],
            _timeout: Duration,
        ) -> Result<CommandOutput, ()> {
            self.calls.lock().unwrap().push(args.to_vec());
            self.envs.lock().unwrap().push(env.to_vec());
            let output = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted command")?;
            if args.first().is_some_and(|arg| arg == "build") && output.success {
                let release_dir = target_directory.join("release");
                std::fs::create_dir_all(&release_dir).unwrap();
                let package = args
                    .windows(2)
                    .find(|pair| pair[0] == "--package")
                    .map(|pair| pair[1].replace('-', "_"))
                    .expect("build package argument");
                let artifact = format!(
                    "{}{}.{}",
                    std::env::consts::DLL_PREFIX,
                    package,
                    std::env::consts::DLL_EXTENSION
                );
                std::fs::write(release_dir.join(artifact), b"plugin bytes").unwrap();
            }
            Ok(output)
        }

        async fn run_step(
            &self,
            step: &PrepareStep,
            _working_directory: &Path,
            env: &[(String, String)],
            _log: &Path,
            _timeout: Duration,
        ) -> Result<CommandOutput, ()> {
            let name = step
                .program
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let mut call = vec!["step".to_owned(), name];
            call.extend(step.args.iter().cloned());
            self.calls.lock().unwrap().push(call);
            self.envs.lock().unwrap().push(env.to_vec());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted step")
        }
    }

    fn ok() -> Result<CommandOutput, ()> {
        Ok(CommandOutput {
            success: true,
            exit_code: Some(0),
            stdout: Vec::new(),
        })
    }

    /// A source whose manifest declares `rebuild`, with `scripts/` holding
    /// every script those steps name.
    fn source_with_rebuild(
        root: &Path,
        rebuild: serde_json::Value,
    ) -> (PathBuf, serde_json::Value) {
        let source = make_source_with_identity(root, "pom-plugin-strata", "strata");
        std::fs::create_dir_all(source.join("scripts")).unwrap();
        for name in ["ui.sh", "runtime.sh", "mac.sh"] {
            std::fs::write(source.join("scripts").join(name), "#!/bin/sh\n").unwrap();
        }
        std::fs::write(
            source.join("ui/manifest.json"),
            serde_json::json!({
                "schema": "pom-plugin-ui/v1",
                "plugin_code": "strata",
                "development": {"rebuild": rebuild}
            })
            .to_string(),
        )
        .unwrap();
        let manifest = source.join("Cargo.toml").canonicalize().unwrap();
        let metadata = serde_json::json!({
            "packages": [{
                "name": "pom-plugin-strata",
                "manifest_path": manifest,
                "targets": [{"name":"pom_plugin_strata","kind":["cdylib"]}]
            }]
        });
        (source, metadata)
    }

    #[tokio::test]
    async fn the_manifests_rebuild_steps_run_before_the_build_with_a_writable_folder() {
        let temp = tempfile::tempdir().unwrap();
        let other_os = if std::env::consts::OS == "linux" {
            "windows"
        } else {
            "linux"
        };
        let (_, metadata) = source_with_rebuild(
            temp.path(),
            serde_json::json!([
                {"run": ["scripts/ui.sh"]},
                {"run": ["scripts/mac.sh"], "os": [other_os]},
                {"run": ["scripts/runtime.sh", "--quick"], "os": [std::env::consts::OS]},
            ]),
        );
        let build_root = temp.path().join("builds");
        let runner = FakeRunner::new(vec![
            Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: serde_json::to_vec(&metadata).unwrap(),
            }),
            ok(),
            ok(),
            ok(),
        ]);
        build_source(temp.path(), &build_root, "pom-plugin-strata", &runner)
            .await
            .unwrap();
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0][0], "metadata");
        assert_eq!(calls[1], ["step", "ui.sh"]);
        assert_eq!(calls[2], ["step", "runtime.sh", "--quick"]);
        assert_eq!(calls[3][0], "build");
        assert_eq!(calls.len(), 4);
        // The steps and the build see the same writable folder, outside the source.
        let envs = runner.envs.lock().unwrap();
        let out = envs[1]
            .iter()
            .find(|(key, _)| key == "POM_PLUGIN_OUT_DIR")
            .unwrap()
            .1
            .clone();
        let out_dir = build_root
            .canonicalize()
            .unwrap()
            .join("pom-plugin-strata")
            .join(PREPARE_DIRECTORY);
        assert_eq!(Path::new(&out), out_dir);
        assert!(out_dir.is_dir());
        assert!(envs[3]
            .iter()
            .any(|(key, value)| key == "POM_PLUGIN_OUT_DIR" && *value == out));
        assert!(envs[1]
            .iter()
            .any(|(key, value)| key == "POM_PLUGIN_PLATFORM" && *value == build_platform()));
    }

    #[tokio::test]
    async fn a_failed_rebuild_step_stops_before_the_build() {
        let temp = tempfile::tempdir().unwrap();
        let (_, metadata) =
            source_with_rebuild(temp.path(), serde_json::json!([{"run": ["scripts/ui.sh"]}]));
        let runner = FakeRunner::new(vec![
            Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: serde_json::to_vec(&metadata).unwrap(),
            }),
            Ok(CommandOutput {
                success: false,
                exit_code: Some(2),
                stdout: Vec::new(),
            }),
        ]);
        let error = build_source(
            temp.path(),
            &temp.path().join("builds"),
            "pom-plugin-strata",
            &runner,
        )
        .await
        .unwrap_err();
        assert_eq!(error.stage, Some(BuildStage::Prepare));
        assert_eq!(error.exit_code, Some(2));
        assert_eq!(runner.calls.lock().unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_rebuild_script_runs_through_bash_with_the_build_variables_and_a_log() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("step.sh");
        // No executable bit: bash runs it.
        std::fs::write(
            &script,
            "echo \"out=$POM_PLUGIN_OUT_DIR arg=$1\"\necho to-stderr >&2\nexit 3\n",
        )
        .unwrap();
        let log = temp.path().join("rebuild.log");
        let step = PrepareStep {
            program: script,
            args: vec!["x".into()],
        };
        let env = vec![("POM_PLUGIN_OUT_DIR".to_owned(), "/out".to_owned())];
        let output = CargoCommandRunner
            .run_step(&step, temp.path(), &env, &log, Duration::from_secs(30))
            .await
            .unwrap();
        assert!(!output.success);
        assert_eq!(output.exit_code, Some(3));
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("out=/out arg=x"), "{text}");
        assert!(text.contains("to-stderr"), "{text}");
    }

    #[test]
    fn a_rebuild_step_must_name_a_script_inside_the_source() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("outside.sh"), "#!/bin/sh\n").unwrap();
        for rebuild in [
            serde_json::json!([{"run": ["../outside.sh"]}]),
            serde_json::json!([{"run": ["/bin/sh"]}]),
            serde_json::json!([{"run": ["scripts/missing.sh"]}]),
            serde_json::json!([{"run": []}]),
            serde_json::json!([{"run": "scripts/ui.sh"}]),
            serde_json::json!({"run": ["scripts/ui.sh"]}),
        ] {
            let (source, _) = source_with_rebuild(temp.path(), rebuild.clone());
            let root = source.canonicalize().unwrap();
            assert_eq!(
                validate_plugin_manifests(&root).unwrap_err().code,
                DevPluginErrorCode::InvalidSource,
                "{rebuild}"
            );
        }
    }

    fn make_source(root: &Path) -> PathBuf {
        make_source_with_identity(root, "pom-plugin-base", "base")
    }

    fn make_source_with_identity(root: &Path, repository: &str, plugin_code: &str) -> PathBuf {
        let source = root.join(repository);
        std::fs::create_dir_all(source.join("ui")).unwrap();
        std::fs::write(source.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        std::fs::write(
            source.join("ui/manifest.json"),
            serde_json::json!({"schema":"pom-plugin-ui/v1","plugin_code":plugin_code}).to_string(),
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
            let build_root = temp.path().join("builds");
            std::fs::create_dir_all(&build_root).unwrap();
            std::os::unix::fs::symlink(target_outside.path(), build_root.join("pom-plugin-base"))
                .unwrap();
            assert!(matches!(
                canonical_target_directory(
                    &build_root,
                    "pom-plugin-base",
                    &temp.path().join("pom-plugin-base"),
                )
                .unwrap_err()
                .code,
                DevPluginErrorCode::InvalidSource
            ));
            std::fs::remove_file(build_root.join("pom-plugin-base")).unwrap();
            assert!(matches!(
                canonical_target_directory(
                    &temp.path().join("pom-plugin-base"),
                    "pom-plugin-base",
                    &temp.path().join("pom-plugin-base"),
                )
                .unwrap_err()
                .code,
                DevPluginErrorCode::InvalidSource
            ));

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
    fn any_valid_prefixed_source_is_listed_with_its_manifest_identity() {
        let temp = tempfile::tempdir().unwrap();
        make_source_with_identity(temp.path(), "pom-plugin-code-server", "code_server");
        make_source_with_identity(temp.path(), "pom-plugin-custom-tool", "custom.tool-v2");
        make_source_with_identity(temp.path(), "pom-plugin-bad-code", "../escape");
        let invalid = temp.path().join("pom-plugin-broken");
        std::fs::create_dir_all(&invalid).unwrap();

        let listing = list_mounted_sources(temp.path(), &[]);
        assert!(listing.mount_available);
        assert_eq!(listing.sources.len(), 2);
        assert_eq!(listing.sources[0].repository, "pom-plugin-code-server");
        assert_eq!(listing.sources[0].plugin_code, "code_server");
        assert_eq!(listing.sources[0].display_name, "Code Server");
        assert_eq!(listing.sources[1].repository, "pom-plugin-custom-tool");
        assert_eq!(listing.sources[1].plugin_code, "custom.tool-v2");
        assert_eq!(listing.sources[1].display_name, "Custom Tool");
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
    fn source_listing_exposes_only_valid_prefixed_directories_and_install_state() {
        let temp = tempfile::tempdir().unwrap();
        make_source(temp.path());
        std::fs::create_dir_all(temp.path().join("pom-plugin-not-allowed")).unwrap();
        let listing = list_mounted_sources(temp.path(), &[("base".into(), true, true, false)]);
        assert!(listing.mount_available);
        assert_eq!(listing.sources.len(), 1);
        assert_eq!(listing.sources[0].repository, "pom-plugin-base");
        assert!(listing.sources[0].active);
        assert_eq!(listing.sources[0].install_kind.as_deref(), Some("github"));

        let development = list_mounted_sources(temp.path(), &[("base".into(), true, false, true)]);
        assert_eq!(
            development.sources[0].install_kind.as_deref(),
            Some("development")
        );
    }

    #[tokio::test]
    async fn build_uses_fixed_cargo_arguments_and_keeps_artifacts_outside_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let source =
            make_source_with_identity(temp.path(), "pom-plugin-code-server", "code_server");
        let build_root = temp.path().join("builds");
        let manifest = source.join("Cargo.toml").canonicalize().unwrap();
        let metadata = serde_json::json!({
            "packages": [{
                "name": "pom-plugin-code-server",
                "manifest_path": manifest,
                "targets": [{"name":"pom_plugin_code_server","kind":["cdylib"]}]
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

        let built = build_source(temp.path(), &build_root, "pom-plugin-code-server", &runner)
            .await
            .unwrap();
        assert_eq!(built.repository, "pom-plugin-code-server");
        assert_eq!(built.plugin_code, "code_server");
        assert_eq!(built.bytes, b"plugin bytes");
        assert!(!source.join("target").exists());
        assert!(build_root.join("pom-plugin-code-server/release").is_dir());
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
                "pom-plugin-code-server",
                "--lib",
            ]
        );
    }

    #[tokio::test]
    async fn build_failure_reports_only_safe_stage_and_exit_code() {
        let temp = tempfile::tempdir().unwrap();
        let source = make_source(temp.path());
        let build_root = temp.path().join("builds");
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
        let error = build_source(temp.path(), &build_root, "pom-plugin-base", &runner)
            .await
            .unwrap_err();
        assert_eq!(error.code, DevPluginErrorCode::BuildFailed);
        assert_eq!(error.stage, Some(BuildStage::Compile));
        assert_eq!(error.exit_code, Some(101));
        assert!(!serde_json::to_string(&error).unwrap().contains("stderr"));
    }
}
