use enterprise_protocol::PluginManifest;
use semver::Version;
use sha2::Digest;
use std::path::{Path, PathBuf};

pub(crate) const CORE_PLUGIN_CODE: &str = "pom_core";
pub(crate) const CORE_RELEASE_CHANNEL: &str = "stable";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InstallResult {
    Replaced,
    #[cfg(windows)]
    Staged(PathBuf),
}

/// Decide whether a manifest identifies a build newer than this executable.
///
/// Release IDs are the build commit returned by `crate::build_commit()`. A
/// release with a higher SemVer wins; for the same version a different commit
/// is a new build, but an older version is never installed.
pub(crate) fn is_newer_release(
    current_version: &str,
    current_release_id: &str,
    manifest: &PluginManifest,
) -> bool {
    let Ok(current) = Version::parse(current_version) else {
        return false;
    };
    let Ok(candidate) = Version::parse(&manifest.version) else {
        return false;
    };
    match candidate.cmp(&current) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Equal => manifest.release_id != current_release_id,
        std::cmp::Ordering::Less => false,
    }
}

pub(crate) fn validate_manifest(
    manifest: &PluginManifest,
    current_version: &str,
) -> Result<(), String> {
    if manifest.schema != 1 {
        return Err("schema de manifest do pom_core não suportado".into());
    }
    if manifest.product != "pom"
        || manifest.channel != CORE_RELEASE_CHANNEL
        || manifest.os != std::env::consts::OS
        || manifest.arch != std::env::consts::ARCH
        || manifest.plugin_abi != enterprise_protocol::PLUGIN_ABI_VERSION
    {
        return Err("manifest do pom_core incompatível com esta instalação ou ABI".into());
    }
    if manifest.release_id.trim().is_empty() || manifest.signature.trim().is_empty() {
        return Err("manifest do pom_core sem release_id ou assinatura".into());
    }
    let current = Version::parse(current_version)
        .map_err(|_| "versão do pom em execução inválida".to_string())?;
    Version::parse(&manifest.version).map_err(|_| "versão do pom_core inválida".to_string())?;
    let minimum = Version::parse(&manifest.min_host_version)
        .map_err(|_| "min_host_version do pom_core inválido".to_string())?;
    if minimum > current {
        return Err("o pom_core exige uma versão do host mais nova".into());
    }
    if manifest
        .sha256
        .strip_prefix("sha256:")
        .unwrap_or(&manifest.sha256)
        .len()
        != 64
        || !manifest
            .sha256
            .strip_prefix("sha256:")
            .unwrap_or(&manifest.sha256)
            .chars()
            .all(|c| c.is_ascii_hexdigit())
    {
        return Err("sha256 do pom_core inválido".into());
    }
    if manifest.size == 0 {
        return Err("manifest do pom_core representa uma versão inválida".into());
    }
    Ok(())
}

pub(crate) fn validate_download(manifest: &PluginManifest, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 != manifest.size {
        return Err("tamanho do pom_core divergente".into());
    }
    let actual = hex::encode(sha2::Sha256::digest(bytes));
    let expected = manifest
        .sha256
        .strip_prefix("sha256:")
        .unwrap_or(&manifest.sha256);
    if actual != expected {
        return Err("hash do pom_core divergente".into());
    }
    Ok(())
}

pub(crate) fn install_atomically(
    destination: &Path,
    bytes: &[u8],
) -> std::io::Result<InstallResult> {
    let parent = destination
        .parent()
        .ok_or_else(|| std::io::Error::other("diretório do executável ausente"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = temporary_path(parent, destination);
    let result = write_temporary(&temporary, destination, bytes);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }

    #[cfg(windows)]
    {
        return Ok(InstallResult::Staged(temporary));
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(&temporary, destination)?;
        // The replacement is already complete. A directory fsync failure
        // cannot roll it back, so keep the successful install result.
        let _ = sync_directory(parent);
        Ok(InstallResult::Replaced)
    }
}

fn temporary_path(parent: &Path, destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pom");
    parent.join(format!(".{name}.update-{}", uuid::Uuid::new_v4()))
}

fn write_temporary(temporary: &Path, destination: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut file = std::fs::File::create(temporary)?;
    file.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mode = std::fs::metadata(destination)
            .map(|metadata| metadata.mode())
            .unwrap_or(0o755);
        std::fs::set_permissions(temporary, std::fs::Permissions::from_mode(mode))?;
    }
    file.sync_all()
}

#[cfg(unix)]
fn sync_directory(parent: &Path) -> std::io::Result<()> {
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn launch_staged_update(
    staged: &Path,
    destination: &Path,
    args: &[String],
) -> std::io::Result<()> {
    let arguments =
        serde_json::to_string(args).map_err(|error| std::io::Error::other(error.to_string()))?;
    let script = format!(
        "$stage='{stage}';$target='{target}';$arguments=ConvertFrom-Json -InputObject '{arguments}';$attempt=0;while($attempt -lt 120){{try{{Move-Item -LiteralPath $stage -Destination $target -Force -ErrorAction Stop;break}}catch{{Start-Sleep -Milliseconds 250;$attempt++}}}};if(Test-Path -LiteralPath $stage){{exit 1}};Remove-Item Env:POM_WINDOWS_SUPERVISED_CHILD -ErrorAction SilentlyContinue;Start-Process -FilePath $target -ArgumentList $arguments",
        stage = powershell_literal(staged),
        target = powershell_literal(destination),
        arguments = powershell_literal(&arguments),
    );
    std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(windows)]
fn powershell_literal(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_the_license_server_core_plugin_code() {
        assert_eq!(CORE_PLUGIN_CODE, "pom_core");
    }

    fn manifest(release_id: &str, version: &str) -> PluginManifest {
        PluginManifest {
            schema: 1,
            release_id: release_id.into(),
            product: "pom".into(),
            version: version.into(),
            channel: "stable".into(),
            feature_set: vec![],
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            min_host_version: "0.1.0".into(),
            plugin_abi: 1,
            sha256: "sha256:artifact".into(),
            size: 8,
            download_expires_at: None,
            changelog: None,
            preferences: None,
            key_id: "release-key".into(),
            signature: "signed".into(),
        }
    }

    #[test]
    fn selects_a_newer_semver_release() {
        assert!(is_newer_release(
            "0.1.0",
            "old-commit",
            &manifest("new-commit", "0.2.0")
        ));
    }

    #[test]
    fn selects_a_same_version_release_with_a_new_build_id() {
        assert!(is_newer_release(
            "0.1.0",
            "old-commit",
            &manifest("new-commit", "0.1.0")
        ));
    }

    #[test]
    fn does_not_downgrade_or_reinstall_the_running_release() {
        assert!(!is_newer_release(
            "0.2.0",
            "new-commit",
            &manifest("old-commit", "0.1.0")
        ));
        assert!(!is_newer_release(
            "0.1.0",
            "new-commit",
            &manifest("new-commit", "0.1.0")
        ));
    }

    #[test]
    fn rejects_a_manifest_that_requires_a_newer_host() {
        let mut candidate = manifest("new-commit", "0.2.0");
        candidate.min_host_version = "0.3.0".into();

        let error = validate_manifest(&candidate, "0.1.0").expect_err("must reject");

        assert!(error.contains("host mais nova"));
    }

    #[test]
    fn validates_the_download_size_and_sha256() {
        let bytes = b"new-pom";
        let mut candidate = manifest("new-commit", "0.2.0");
        candidate.size = bytes.len() as u64;
        candidate.sha256 = format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)));

        validate_download(&candidate, bytes).expect("valid artifact");
        assert!(validate_download(&candidate, b"tampered").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn writes_and_replaces_the_executable_atomically() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("pom");
        std::fs::write(&destination, b"old-pom").expect("old executable");

        let result = install_atomically(&destination, b"new-pom").expect("replace");

        assert_eq!(result, InstallResult::Replaced);
        assert_eq!(
            std::fs::read(destination).expect("new executable"),
            b"new-pom"
        );
    }
}
