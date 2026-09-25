//! Public GitHub Releases source for plugin artifacts. The GitHub client is
//! unauthenticated; free installation eligibility is separately confirmed from
//! the public license-server catalog. Paid activation remains in `plugins`.

use enterprise_protocol::PluginPreferencesSchema;
use reqwest::{header, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::Path;

const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_GITHUB_METADATA_BYTES: usize = 2 * 1024 * 1024;
const MAX_PUBLIC_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_PLUGIN_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GithubPluginManifest {
    pub schema: u32,
    pub plugin_code: String,
    pub release_id: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub plugin_abi: u32,
    pub asset: String,
    pub sha256: String,
    pub size: u64,
    #[serde(default)]
    pub feature_set: Vec<String>,
    #[serde(default)]
    pub changelog: Option<String>,
    #[serde(default)]
    pub preferences: Option<PluginPreferencesSchema>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubUpdateInfo {
    pub release_id: String,
    pub version: String,
    pub changelog: Option<String>,
    pub downloaded: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubPluginState {
    pub repository_url: Option<String>,
    pub checked_at: Option<String>,
    /// Network or validation error from the most recent check. The last known
    /// update and downloaded artifact are deliberately retained on failure.
    pub check_error: Option<String>,
    pub update: Option<GithubUpdateInfo>,
    pub manifest: Option<GithubPluginManifest>,
}

#[derive(Debug, Clone)]
pub(crate) struct DownloadedRelease {
    pub manifest: GithubPluginManifest,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct GithubReleaseClient {
    client: reqwest::Client,
    api_base: reqwest::Url,
}

impl GithubReleaseClient {
    pub(crate) fn new() -> Self {
        Self::with_api_base("https://api.github.com").expect("static GitHub API URL")
    }

    pub(crate) fn with_api_base(base: &str) -> Result<Self, String> {
        let api_base = reqwest::Url::parse(base).map_err(|error| error.to_string())?;
        if api_base.scheme() != "https" && !cfg!(test) {
            return Err("GitHub API requires HTTPS".into());
        }
        let allowed_host = api_base.host_str().map(str::to_owned);
        let client = reqwest::Client::builder()
            .user_agent(concat!("pom/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                let Some(host) = attempt.url().host_str() else {
                    return attempt.stop();
                };
                let is_github_asset = matches!(
                    host,
                    "github.com"
                        | "objects.githubusercontent.com"
                        | "release-assets.githubusercontent.com"
                        | "github-releases.githubusercontent.com"
                );
                if Some(host) == allowed_host.as_deref() || is_github_asset {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { client, api_base })
    }

    pub(crate) async fn check_and_download(
        &self,
        repository: &str,
        plugin_code: &str,
        os: &str,
        arch: &str,
        installed_version: Option<&str>,
    ) -> Result<Option<DownloadedRelease>, String> {
        let repository = canonical_repository_parts(repository)?;
        let api_root = self.api_base.as_str().trim_end_matches('/');
        let repo_api = format!("{api_root}/repos/{}/{}", repository.owner, repository.repo);
        let repository_response = self
            .client
            .get(&repo_api)
            .header(header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|error| format!("GitHub repository lookup failed: {error}"))?;
        if repository_response.status() != StatusCode::OK {
            return Err(github_status_error(
                repository_response.status(),
                "repository",
            ));
        }
        let repository_bytes = read_limited(
            repository_response,
            MAX_GITHUB_METADATA_BYTES,
            "repository metadata",
        )
        .await?;
        let repository_json: serde_json::Value = serde_json::from_slice(&repository_bytes)
            .map_err(|error| format!("invalid GitHub repository response: {error}"))?;
        if repository_json
            .get("private")
            .and_then(|value| value.as_bool())
            != Some(false)
            || repository_json
                .get("archived")
                .and_then(|value| value.as_bool())
                == Some(true)
        {
            return Err("repository must be public and accessible".into());
        }

        let release_response = self
            .client
            .get(format!("{repo_api}/releases/latest"))
            .header(header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|error| format!("GitHub release lookup failed: {error}"))?;
        if release_response.status() != StatusCode::OK {
            return Err(github_status_error(
                release_response.status(),
                "latest release",
            ));
        }
        let release_bytes = read_limited(
            release_response,
            MAX_GITHUB_METADATA_BYTES,
            "release metadata",
        )
        .await?;
        let release: GithubRelease = serde_json::from_slice(&release_bytes)
            .map_err(|error| format!("invalid GitHub release response: {error}"))?;
        if release.draft || release.prerelease {
            return Err("latest release must be published and stable".into());
        }
        let manifest_asset_name = format!("pom-plugin-{os}-{arch}.json");
        let metadata = exactly_one_asset(&release.assets, &manifest_asset_name)?;
        if metadata.size == 0 || metadata.size > MAX_MANIFEST_BYTES as u64 {
            return Err(format!("{manifest_asset_name} asset size is invalid"));
        }
        let manifest_bytes = self.download_asset(metadata).await?;
        if manifest_bytes.len() > MAX_MANIFEST_BYTES {
            return Err(format!("{manifest_asset_name} exceeds the size limit"));
        }
        validate_api_digest(&manifest_bytes, metadata.digest.as_deref())?;
        let mut manifest: GithubPluginManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| format!("invalid {manifest_asset_name}: {error}"))?;
        if manifest.changelog.is_none() {
            manifest.changelog = release.body.clone().filter(|body| !body.trim().is_empty());
        }
        validate_manifest(&manifest, plugin_code, &release.tag_name, os, arch)?;
        if let Some(current) = installed_version {
            let current = semver::Version::parse(current)
                .map_err(|_| "installed plugin version is not valid SemVer".to_owned())?;
            let latest = semver::Version::parse(&manifest.version)
                .map_err(|_| "GitHub plugin version is not valid SemVer".to_owned())?;
            if latest <= current {
                return Ok(None);
            }
        }
        let asset = exactly_one_asset(&release.assets, &manifest.asset)?;
        if asset.size != manifest.size || asset.size == 0 || asset.size > MAX_PLUGIN_BYTES {
            return Err("plugin asset size does not match the manifest".into());
        }
        let bytes = self.download_asset(asset).await?;
        validate_integrity(&manifest, &bytes, asset.digest.as_deref())?;
        Ok(Some(DownloadedRelease { manifest, bytes }))
    }

    async fn download_asset(&self, asset: &GithubAsset) -> Result<Vec<u8>, String> {
        let url = reqwest::Url::parse(&asset.url)
            .map_err(|_| "GitHub release asset URL is invalid".to_owned())?;
        if (url.scheme() != "https" && !(cfg!(test) && url.scheme() == "http"))
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err("GitHub release assets must use HTTPS without user information".into());
        }
        let allowed = matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "objects.githubusercontent.com"
                    | "release-assets.githubusercontent.com"
                    | "github-releases.githubusercontent.com"
            )
        ) || (cfg!(test) && url.host_str() == self.api_base.host_str());
        if !allowed {
            return Err("GitHub release asset host is not trusted".into());
        }
        let mut response = self
            .client
            .get(url)
            .header(header::ACCEPT, "application/octet-stream")
            .send()
            .await
            .map_err(|error| format!("GitHub asset download failed: {error}"))?;
        if response.status() != StatusCode::OK {
            return Err(github_status_error(response.status(), "release asset"));
        }
        if asset.size == 0
            || asset.size > MAX_PLUGIN_BYTES
            || response
                .content_length()
                .is_some_and(|length| length > asset.size || length > MAX_PLUGIN_BYTES)
        {
            return Err("GitHub release asset size is invalid".into());
        }
        let mut bytes = Vec::with_capacity(asset.size as usize);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("GitHub asset download failed: {error}"))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > asset.size {
                return Err("GitHub release asset exceeds its declared size".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() as u64 != asset.size {
            return Err("GitHub release asset size does not match its declaration".into());
        }
        Ok(bytes)
    }
}

async fn read_limited(
    mut response: reqwest::Response,
    limit: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(format!("GitHub {label} exceeds the size limit"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("GitHub {label} download failed: {error}"))?
    {
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(format!("GitHub {label} exceeds the size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

struct CanonicalRepository {
    owner: String,
    repo: String,
}

pub(crate) async fn public_free_plugin_repository(
    license_base_url: &str,
    plugin_code: &str,
) -> Result<String, String> {
    let base = reqwest::Url::parse(license_base_url.trim_end_matches('/'))
        .map_err(|_| "license-server URL is invalid".to_owned())?;
    let allow_custom_http = std::env::var("POM_LICENSE_ALLOW_CUSTOM_BASE_URL")
        .ok()
        .as_deref()
        == Some("1");
    if base.scheme() != "https" && !cfg!(test) && !allow_custom_http {
        return Err("public plugin catalog requires HTTPS".into());
    }
    let catalog_url = format!("{}/v1/plugins", base.as_str().trim_end_matches('/'));
    let response = reqwest::Client::builder()
        .user_agent(concat!("pom/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?
        .get(catalog_url)
        .send()
        .await
        .map_err(|error| format!("public plugin catalog request failed: {error}"))?;
    if response.status() != StatusCode::OK {
        return Err(format!(
            "public plugin catalog request failed with HTTP {}",
            response.status()
        ));
    }
    let bytes = read_limited(response, MAX_PUBLIC_CATALOG_BYTES, "public plugin catalog").await?;
    let catalog: PublicPluginCatalogResponse = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid public plugin catalog: {error}"))?;
    let plugin = catalog
        .plugins
        .into_iter()
        .find(|plugin| plugin.code == plugin_code)
        .ok_or_else(|| "plugin is not listed in the public catalog".to_owned())?;
    if plugin.requires_license {
        return Err("this plugin requires an active license".into());
    }
    let repository = plugin
        .github_repository
        .ok_or_else(|| "this plugin has no public GitHub source".to_owned())?;
    let canonical = canonical_repository(&repository)?;
    if canonical != repository {
        return Err("public GitHub repository URL is not canonical".into());
    }
    Ok(canonical)
}

#[derive(Deserialize)]
struct PublicPluginCatalogResponse {
    plugins: Vec<PublicPluginCatalogEntry>,
}

#[derive(Deserialize)]
struct PublicPluginCatalogEntry {
    code: String,
    requires_license: bool,
    github_repository: Option<String>,
}

pub(crate) fn canonical_repository(value: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(value.trim())
        .map_err(|_| "enter a canonical public GitHub repository URL".to_owned())?;
    if parsed.scheme() != "https"
        || parsed.host_str() != Some("github.com")
        || parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("enter a canonical public GitHub repository URL".into());
    }
    let segments = parsed
        .path_segments()
        .ok_or_else(|| "enter a canonical public GitHub repository URL".to_owned())?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() != 2
        || segments.iter().any(|segment| !is_github_segment(segment))
        || segments[1].ends_with(".git")
    {
        return Err("URL must be https://github.com/owner/repository".into());
    }
    Ok(format!(
        "https://github.com/{}/{}",
        segments[0], segments[1]
    ))
}

fn canonical_repository_parts(value: &str) -> Result<CanonicalRepository, String> {
    let url = canonical_repository(value)?;
    let mut parts = url.trim_end_matches('/').rsplitn(2, '/');
    let repo = parts.next().unwrap_or_default().to_owned();
    let owner = parts
        .next()
        .and_then(|value| value.rsplit('/').next())
        .unwrap_or_default()
        .to_owned();
    Ok(CanonicalRepository { owner, repo })
}

fn is_github_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.len() <= 100
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
    /// GitHub's release API returns both `url` (the API asset endpoint) and
    /// `browser_download_url`; the POM must download through the API endpoint.
    url: String,
    #[serde(default)]
    digest: Option<String>,
}

fn exactly_one_asset<'a>(assets: &'a [GithubAsset], name: &str) -> Result<&'a GithubAsset, String> {
    let matching = assets
        .iter()
        .filter(|asset| asset.name == name)
        .collect::<Vec<_>>();
    match matching.as_slice() {
        [asset] => Ok(asset),
        [] => Err(format!("GitHub release is missing asset {name}")),
        _ => Err(format!("GitHub release contains duplicate asset {name}")),
    }
}

fn validate_manifest(
    manifest: &GithubPluginManifest,
    code: &str,
    tag: &str,
    os: &str,
    arch: &str,
) -> Result<(), String> {
    let expected_extension = match os {
        "windows" => ".dll",
        "macos" => ".dylib",
        "linux" => ".so",
        _ => return Err("unsupported operating system for plugin binary".into()),
    };
    if manifest.schema != 1
        || manifest.plugin_code != code
        || manifest.release_id != tag
        || manifest.os != os
        || manifest.arch != arch
        || manifest.plugin_abi != enterprise_protocol::PLUGIN_ABI_VERSION
        || manifest.size == 0
        || manifest.size > MAX_PLUGIN_BYTES
        || manifest.asset.is_empty()
        || manifest.asset.contains(['/', '\\'])
        || !manifest.asset.ends_with(expected_extension)
        || !manifest
            .asset
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
        || !valid_sha256(&manifest.sha256)
        || semver::Version::parse(&manifest.version).is_err()
        || !safe_release_id(&manifest.release_id)
        || !manifest
            .feature_set
            .iter()
            .any(|feature| feature.starts_with(&format!("{code}.")))
        || manifest
            .feature_set
            .iter()
            .any(|feature| !feature.starts_with(&format!("{code}.")) || feature.trim() != feature)
    {
        return Err("GitHub plugin metadata is incompatible with this node".into());
    }
    if let Some(preferences) = &manifest.preferences {
        validate_preferences_schema(preferences)?;
    }
    Ok(())
}

pub(crate) fn validate_preferences_schema(schema: &PluginPreferencesSchema) -> Result<(), String> {
    if schema.features.len() > 64 {
        return Err("plugin preference schema has too many boolean options".into());
    }
    let mut keys = std::collections::HashSet::new();
    for preference in &schema.features {
        if !valid_preference_key(&preference.key)
            || preference.label.trim().is_empty()
            || preference.label.len() > 200
            || preference
                .description
                .as_ref()
                .is_some_and(|text| text.len() > 1000)
            || !keys.insert(preference.key.as_str())
        {
            return Err("plugin preference schema contains an invalid boolean option".into());
        }
    }
    if let Some(directory) = &schema.storage_directory {
        if directory.label.trim().is_empty()
            || directory.label.len() > 200
            || directory.default.as_ref().is_some_and(|value| {
                value.is_empty()
                    || value.len() > 4096
                    || value.contains('\0')
                    || !Path::new(value).is_absolute()
            })
        {
            return Err("plugin preference schema contains an invalid storage directory".into());
        }
    }
    Ok(())
}

fn valid_preference_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.as_bytes()[0].is_ascii_lowercase()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_sha256(value: &str) -> bool {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(crate) fn validate_integrity(
    manifest: &GithubPluginManifest,
    bytes: &[u8],
    api_digest: Option<&str>,
) -> Result<(), String> {
    if bytes.len() as u64 != manifest.size {
        return Err("GitHub plugin asset size does not match its integrity metadata".into());
    }
    let expected = manifest
        .sha256
        .strip_prefix("sha256:")
        .unwrap_or(&manifest.sha256);
    let actual = hex::encode(sha2::Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(expected) {
        return Err("GitHub plugin asset SHA-256 does not match its integrity metadata".into());
    }
    validate_api_digest(bytes, api_digest)?;
    Ok(())
}

fn validate_api_digest(bytes: &[u8], api_digest: Option<&str>) -> Result<(), String> {
    if let Some(digest) = api_digest {
        let Some(digest) = digest.strip_prefix("sha256:") else {
            return Err("GitHub asset API returned an unsupported integrity digest".into());
        };
        let actual = hex::encode(sha2::Sha256::digest(bytes));
        if !valid_sha256(digest) || !actual.eq_ignore_ascii_case(digest) {
            return Err("GitHub asset API digest does not match the downloaded asset".into());
        }
    }
    Ok(())
}

fn safe_release_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+'))
}

fn github_status_error(status: StatusCode, action: &str) -> String {
    match status {
        StatusCode::NOT_FOUND => format!("GitHub {action} is not public or does not exist"),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
            format!("GitHub rate limit or access policy prevented {action} lookup")
        }
        _ => format!("GitHub {action} request failed with HTTP {status}"),
    }
}

pub(crate) fn downloaded_asset_path(
    root: &Path,
    code: &str,
    release_id: &str,
    sha256: &str,
    os: &str,
    arch: &str,
) -> Result<std::path::PathBuf, String> {
    let digest = sha256.strip_prefix("sha256:").unwrap_or(sha256);
    if !safe_release_id(release_id)
        || !is_github_segment(code)
        || !is_github_segment(arch)
        || !valid_sha256(digest)
    {
        return Err("invalid GitHub plugin asset path".into());
    }
    let extension = match os {
        "windows" => "dll",
        "macos" => "dylib",
        "linux" => "so",
        _ => return Err("unsupported operating system for plugin binary".into()),
    };
    let file_release_id = &release_id[..release_id.len().min(64)];
    Ok(root
        .join("plugins")
        .join(code)
        .join("github-downloads")
        .join(format!("{file_release_id}-{digest}-{arch}.{extension}")))
}

pub(crate) fn write_downloaded_asset(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "GitHub plugin download directory is missing".to_owned())?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("plugin");
    let temporary = parent.join(format!(".{name}.part-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        std::fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn github_assets_accept_both_api_and_browser_download_urls() {
        let asset: GithubAsset = serde_json::from_value(serde_json::json!({
            "name": "pom-plugin-macos-aarch64.json",
            "size": 123,
            "url": "https://api.github.com/repos/owner/repo/releases/assets/1",
            "browser_download_url": "https://github.com/owner/repo/releases/download/v1/manifest.json",
            "digest": "sha256:abc"
        }))
        .expect("GitHub API asset has both URL fields");
        assert_eq!(
            asset.url,
            "https://api.github.com/repos/owner/repo/releases/assets/1"
        );
    }

    #[test]
    fn repository_urls_are_canonical_and_public_only() {
        assert_eq!(
            canonical_repository("https://github.com/pom-org/request-logs/").unwrap(),
            "https://github.com/pom-org/request-logs"
        );
        for invalid in [
            "http://github.com/owner/repo",
            "https://github.com/owner/repo/tree/main",
            "https://github.com/owner/repo.git",
            "https://user@github.com/owner/repo",
            "https://github.com/owner/repo?tab=readme",
            "https://github.com.evil.example/owner/repo",
            "https://github.com/../repo",
        ] {
            assert!(canonical_repository(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn manifest_requires_compatible_assets_and_valid_integrity_metadata() {
        let bytes = b"a plugin binary";
        let manifest = GithubPluginManifest {
            schema: 1,
            plugin_code: "analytics".into(),
            release_id: "v1.2.0".into(),
            version: "1.2.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            plugin_abi: enterprise_protocol::PLUGIN_ABI_VERSION,
            asset: match std::env::consts::OS {
                "windows" => "plugin.dll",
                "macos" => "plugin.dylib",
                _ => "plugin.so",
            }
            .into(),
            sha256: format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes))),
            size: bytes.len() as u64,
            feature_set: vec!["analytics.reports".into()],
            changelog: None,
            preferences: None,
        };
        validate_manifest(
            &manifest,
            "analytics",
            "v1.2.0",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
        .unwrap();
        validate_integrity(&manifest, bytes, None).unwrap();
        assert!(validate_integrity(&manifest, b"tampered", None).is_err());
        let mut bad_hash = manifest.clone();
        bad_hash.sha256 = "sha256:invalid".into();
        assert!(validate_manifest(
            &bad_hash,
            "analytics",
            "v1.2.0",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
        .is_err());
        let mut wrong_asset = manifest.clone();
        wrong_asset.asset = "payload.zip".into();
        assert!(validate_manifest(
            &wrong_asset,
            "analytics",
            "v1.2.0",
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
        .is_err());
        assert!(validate_integrity(&manifest, bytes, Some("sha512:abcd")).is_err());
        assert!(validate_api_digest(bytes, Some(&format!("sha256:{}", "0".repeat(64)))).is_err());
    }

    #[tokio::test]
    async fn public_release_is_selected_by_platform_and_downloaded_with_verified_hash() {
        let (client, _api_base, server) = github_stub(200, false, None, false).await;
        let update = client
            .check_and_download(
                "https://github.com/pom/test-plugin",
                "analytics",
                "linux",
                "x86_64",
                Some("1.0.0"),
            )
            .await
            .expect("public compatible release")
            .expect("new version");
        assert_eq!(update.manifest.version, "1.2.0");
        assert_eq!(update.bytes, b"test plugin binary");
        server.abort();
    }

    #[tokio::test]
    async fn inaccessible_rate_limited_private_and_incompatible_repositories_are_rejected() {
        for (repo_status, private, bad_arch) in [
            (404, false, false),
            (403, false, false),
            (429, false, false),
            (200, true, false),
            (200, false, true),
        ] {
            let (client, _api_base, server) =
                github_stub(repo_status, private, None, bad_arch).await;
            assert!(client
                .check_and_download(
                    "https://github.com/pom/test-plugin",
                    "analytics",
                    "linux",
                    "x86_64",
                    None,
                )
                .await
                .is_err());
            server.abort();
        }
    }

    #[tokio::test]
    async fn malformed_integrity_metadata_is_rejected_before_installation() {
        let (client, _api_base, server) =
            github_stub(200, false, Some("sha256:not-a-hash"), false).await;
        assert!(client
            .check_and_download(
                "https://github.com/pom/test-plugin",
                "analytics",
                "linux",
                "x86_64",
                None,
            )
            .await
            .is_err());
        server.abort();
    }

    pub(crate) async fn github_stub(
        repo_status: u16,
        private: bool,
        bad_hash: Option<&str>,
        bad_arch: bool,
    ) -> (GithubReleaseClient, String, tokio::task::JoinHandle<()>) {
        use axum::extract::State;
        use axum::http::StatusCode as HttpStatus;
        use axum::response::IntoResponse;
        use axum::routing::get;
        use axum::{Json, Router};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct StubState {
            repository_status: u16,
            fail_repository: AtomicBool,
            private: bool,
            manifest: Vec<u8>,
            binary: Vec<u8>,
            asset_base: String,
        }

        async fn public_catalog() -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "plugins": [
                    {
                        "code": "analytics",
                        "requires_license": false,
                        "github_repository": "https://github.com/pom/test-plugin"
                    },
                    {
                        "code": "paid_plugin",
                        "requires_license": true,
                        "github_repository": "https://github.com/pom/paid-plugin"
                    }
                ]
            }))
        }

        async fn repository(State(state): State<Arc<StubState>>) -> axum::response::Response {
            let status = if state.fail_repository.load(Ordering::SeqCst) {
                503
            } else {
                state.repository_status
            };
            if status != 200 {
                return HttpStatus::from_u16(status).unwrap().into_response();
            }
            Json(serde_json::json!({ "private": state.private, "archived": false })).into_response()
        }

        async fn fail_repository(State(state): State<Arc<StubState>>) -> HttpStatus {
            state.fail_repository.store(true, Ordering::SeqCst);
            HttpStatus::NO_CONTENT
        }

        async fn release(State(state): State<Arc<StubState>>) -> Json<serde_json::Value> {
            let binary_digest = hex::encode(sha2::Sha256::digest(&state.binary));
            let manifest_digest = hex::encode(sha2::Sha256::digest(&state.manifest));
            Json(serde_json::json!({
                "tag_name": "v1.2.0",
                "body": "Test release",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {
                        "name": "pom-plugin-linux-x86_64.json",
                        "size": state.manifest.len(),
                        "digest": format!("sha256:{manifest_digest}"),
                        "url": format!("{}/assets/manifest", state.asset_base)
                    },
                    {
                        "name": "pom-plugin-base-linux-x86_64.so",
                        "size": state.binary.len(),
                        "digest": format!("sha256:{binary_digest}"),
                        "url": format!("{}/assets/plugin", state.asset_base)
                    }
                ]
            }))
        }

        async fn manifest_asset(State(state): State<Arc<StubState>>) -> impl IntoResponse {
            (
                [("content-type", "application/json")],
                state.manifest.clone(),
            )
        }

        async fn plugin_asset(State(state): State<Arc<StubState>>) -> impl IntoResponse {
            (
                [("content-type", "application/octet-stream")],
                state.binary.clone(),
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let binary = b"test plugin binary".to_vec();
        let digest = hex::encode(sha2::Sha256::digest(&binary));
        let manifest = serde_json::json!({
            "schema": 1,
            "plugin_code": "analytics",
            "release_id": "v1.2.0",
            "version": "1.2.0",
            "os": "linux",
            "arch": if bad_arch { "aarch64" } else { "x86_64" },
            "plugin_abi": enterprise_protocol::PLUGIN_ABI_VERSION,
            "asset": "pom-plugin-base-linux-x86_64.so",
            "sha256": bad_hash.map(str::to_owned).unwrap_or_else(|| format!("sha256:{digest}")),
            "size": binary.len(),
            "feature_set": ["analytics.reports"],
            "preferences": null
        });
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let state = Arc::new(StubState {
            repository_status: repo_status,
            fail_repository: AtomicBool::new(false),
            private,
            manifest,
            binary,
            asset_base: format!("http://{address}"),
        });
        let app = Router::new()
            .route("/v1/plugins", get(public_catalog))
            .route("/test/fail", axum::routing::post(fail_repository))
            .route("/repos/pom/test-plugin", get(repository))
            .route("/repos/pom/test-plugin/releases/latest", get(release))
            .route("/assets/manifest", get(manifest_asset))
            .route("/assets/plugin", get(plugin_asset))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = GithubReleaseClient::with_api_base(&format!("http://{address}")).unwrap();
        (client, format!("http://{address}"), server)
    }

    #[tokio::test]
    async fn free_install_source_comes_only_from_the_public_license_catalog() {
        let (_client, api_base, server) = github_stub(200, false, None, false).await;
        assert_eq!(
            public_free_plugin_repository(&api_base, "analytics")
                .await
                .expect("free GitHub plugin"),
            "https://github.com/pom/test-plugin"
        );
        assert!(public_free_plugin_repository(&api_base, "enterprise")
            .await
            .is_err());
        assert!(public_free_plugin_repository(&api_base, "paid_plugin")
            .await
            .is_err());
        server.abort();
    }

    #[test]
    fn preference_schemas_are_typed_unique_and_bounded() {
        let schema = PluginPreferencesSchema {
            features: vec![enterprise_protocol::PluginBooleanPreference {
                key: "telemetry_enabled".into(),
                label: "Telemetry".into(),
                default: true,
                description: None,
            }],
            storage_directory: None,
        };
        validate_preferences_schema(&schema).unwrap();
        let mut duplicate = schema.clone();
        duplicate.features.push(duplicate.features[0].clone());
        assert!(validate_preferences_schema(&duplicate).is_err());
        let invalid_directory = PluginPreferencesSchema {
            features: Vec::new(),
            storage_directory: Some(enterprise_protocol::PluginStorageDirectoryPreference {
                label: "Storage".into(),
                default: Some("relative/path".into()),
            }),
        };
        assert!(validate_preferences_schema(&invalid_directory).is_err());
    }
}
