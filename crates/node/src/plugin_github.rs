//! Local GitHub Releases sources for plugins. The unauthenticated GitHub client
//! reads a repository-root manifest and published release assets directly; this
//! path never consults the license-server catalog.

use base64::Engine as _;
use enterprise_protocol::PluginPreferencesSchema;
use reqwest::{header, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::Path;

const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_GITHUB_METADATA_BYTES: usize = 2 * 1024 * 1024;
const GITHUB_RELEASES_PER_PAGE: usize = 100;
const MAX_GITHUB_RELEASE_METADATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_GITHUB_RELEASE_NOTES_BYTES: usize = 8 * 1024;
const MAX_PLUGIN_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GithubRepositoryManifest {
    pub schema: u32,
    pub plugin_code: String,
    pub display_name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub plugin_abi: u32,
    #[serde(default)]
    pub feature_set: Vec<String>,
    /// Release asset name keyed by `<os>-<arch>`, for example
    /// `macos-aarch64: "plugin-macos-aarch64.dylib"`.
    pub artifacts: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub preferences: Option<PluginPreferencesSchema>,
    /// Capturas de tela: caminhos relativos à raiz do repositório ou URLs
    /// `https://`, com as mesmas regras de `pom-plugin-ui/v1`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubRepositoryRelease {
    pub release_id: String,
    pub tag_name: String,
    pub name: Option<String>,
    pub changelog: Option<String>,
    pub published_at: Option<String>,
    pub prerelease: bool,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
    pub available_for_node: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GithubRepository {
    pub id: String,
    pub repository_url: String,
    #[serde(default = "root_manifest_path")]
    pub manifest_path: String,
    pub manifest: GithubRepositoryManifest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_manifest: Option<serde_json::Value>,
    pub releases: Vec<GithubRepositoryRelease>,
    /// Descrição por idioma, vinda do manifesto do repositório.
    #[serde(default)]
    pub description: std::collections::BTreeMap<String, String>,
    /// URLs públicas das capturas de tela. Caminhos relativos viram
    /// `raw.githubusercontent.com/<owner>/<repo>/HEAD/<caminho>`: o navegador
    /// as carrega direto do GitHub, sem passar pelo nó nem pelo license-server.
    #[serde(default)]
    pub screenshot_urls: Vec<String>,
    pub checked_at: String,
    #[serde(default)]
    pub check_error: Option<String>,
}

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

#[derive(Debug, Clone)]
pub(crate) struct DownloadedRelease {
    pub manifest: GithubPluginManifest,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct GithubReleaseClient {
    client: reqwest::Client,
    api_base: reqwest::Url,
    /// `POM_GITHUB_TOKEN`: raises the API limit from 60 calls per hour per IP
    /// to 5,000 per hour for that token. Sent only to the API host.
    token: Option<String>,
    /// Last body and ETag per API URL. An unchanged answer comes back as 304,
    /// which GitHub does not count against the rate limit.
    etags: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (String, Vec<u8>)>>>,
    /// The POM license server, where every stable release of a POM plugin is
    /// also published: the fallback when GitHub refuses or rate-limits a
    /// lookup or a download (`/v1/plugin-releases`). `None` turns it off.
    license_base: Option<reqwest::Url>,
}

/// `release_id` prefix of a release listed by the license server instead of
/// GitHub (whose release IDs are numbers).
pub(crate) const LICENSE_RELEASE_PREFIX: &str = "license-server:";

/// `manifest_path` of a repository whose listing came from the license server.
const LICENSE_MANIFEST_PATH: &str = "license-server";

#[derive(Debug, Deserialize)]
struct LicensePluginReleases {
    plugin_code: String,
    display_name: String,
    #[serde(default)]
    description: String,
    releases: Vec<LicenseRelease>,
}

#[derive(Debug, Clone, Deserialize)]
struct LicenseRelease {
    release_id: String,
    version: String,
    plugin_abi: u32,
    size: u64,
    sha256: String,
    #[serde(default)]
    feature_set: Vec<String>,
    #[serde(default)]
    published_at: Option<String>,
    download_path: String,
}

/// An API answer: the status and the body (the cached one on a 304).
struct ApiAnswer {
    status: StatusCode,
    body: Vec<u8>,
    rate_limit: Option<String>,
}

#[derive(Debug)]
enum RepositoryManifestSource {
    Distribution(GithubRepositoryManifest),
    PluginUi {
        plugin_code: String,
        display_name: String,
        value: serde_json::Value,
    },
}

fn root_manifest_path() -> String {
    "manifest.json".into()
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
        let token = crate::brand::env("GITHUB_TOKEN")
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty());
        Ok(Self {
            client,
            api_base,
            token,
            etags: Default::default(),
            license_base: None,
        })
    }

    /// Turns on the license-server fallback at `base`.
    pub(crate) fn with_license_fallback(mut self, base: &str) -> Self {
        self.license_base = reqwest::Url::parse(base)
            .ok()
            .filter(|url| url.scheme() == "https" || cfg!(test));
        self
    }

    async fn license_releases(
        &self,
        query: &[(&str, &str)],
    ) -> Result<LicensePluginReleases, String> {
        let base = self
            .license_base
            .as_ref()
            .ok_or_else(|| "no license-server fallback is configured".to_owned())?;
        let url = base
            .join("/v1/plugin-releases")
            .map_err(|error| error.to_string())?;
        let response = self
            .client
            .get(url)
            .query(query)
            .send()
            .await
            .map_err(|error| format!("license server lookup failed: {error}"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err("the license server does not publish this plugin".into());
        }
        if response.status() != StatusCode::OK {
            return Err(format!("license server answered HTTP {}", response.status()));
        }
        let bytes = read_limited(response, MAX_GITHUB_METADATA_BYTES, "license server releases").await?;
        let listing: LicensePluginReleases = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid license server releases: {error}"))?;
        if !listing
            .plugin_code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || listing.plugin_code.is_empty()
        {
            return Err("license server returned an invalid plugin code".into());
        }
        Ok(listing)
    }

    /// The repository as the license server lists it, for when GitHub cannot
    /// be inspected. Without the repository's own manifest there are no
    /// preferences or screenshots; the plugin's screens come from the plugin
    /// itself once it runs.
    pub(crate) async fn inspect_from_license_server(
        &self,
        repository_url: &str,
        os: &str,
        arch: &str,
        github_error: &str,
    ) -> Result<GithubRepository, String> {
        let canonical_url = canonical_repository(repository_url)?;
        let parts = canonical_repository_parts(&canonical_url)?;
        let repository = format!("{}/{}", parts.owner, parts.repo);
        let listing = self
            .license_releases(&[("repository", repository.as_str()), ("os", os), ("arch", arch)])
            .await?;
        let latest = listing.releases.first();
        let target = format!("{os}-{arch}");
        let releases = listing
            .releases
            .iter()
            .filter(|release| safe_release_id(&release.release_id))
            .map(|release| GithubRepositoryRelease {
                release_id: format!("{LICENSE_RELEASE_PREFIX}{}", release.release_id),
                tag_name: format!("v{}", release.version.trim_start_matches('v')),
                name: None,
                changelog: None,
                published_at: release.published_at.clone(),
                prerelease: false,
                asset_name: Some(release.release_id.clone()),
                asset_size: Some(release.size),
                available_for_node: release.plugin_abi == enterprise_protocol::PLUGIN_ABI_VERSION,
            })
            .collect::<Vec<_>>();
        let description = Some(listing.description.trim().to_owned()).filter(|text| !text.is_empty());
        Ok(GithubRepository {
            id: hex::encode(sha2::Sha256::digest(canonical_url.as_bytes())),
            repository_url: canonical_url,
            manifest_path: LICENSE_MANIFEST_PATH.to_owned(),
            manifest: GithubRepositoryManifest {
                schema: 1,
                plugin_code: listing.plugin_code.clone(),
                display_name: listing.display_name.clone(),
                description: description.clone(),
                plugin_abi: latest.map_or(enterprise_protocol::PLUGIN_ABI_VERSION, |release| release.plugin_abi),
                feature_set: latest.map(|release| release.feature_set.clone()).unwrap_or_default(),
                artifacts: [(target, LICENSE_MANIFEST_PATH.to_owned())].into_iter().collect(),
                preferences: None,
                screenshots: Vec::new(),
            },
            ui_manifest: None,
            releases,
            description: description.map(|text| [("en".to_owned(), text)].into_iter().collect()).unwrap_or_default(),
            screenshot_urls: Vec::new(),
            checked_at: chrono::Utc::now().to_rfc3339(),
            check_error: Some(format!(
                "GitHub could not be read ({github_error}); releases come from the POM license server"
            )),
        })
    }

    /// One release from the license server: the listing is read again (never
    /// trusted from the saved repository), and the bytes are checked against
    /// its SHA-256 and size. `wanted` is the license server's release ID, or
    /// a version (`1.0.1` or `v1.0.1`) when GitHub failed for a GitHub release.
    async fn download_from_license_server(
        &self,
        repository: &GithubRepository,
        wanted: &str,
        os: &str,
        arch: &str,
    ) -> Result<DownloadedRelease, String> {
        let plugin_code = repository.manifest.plugin_code.as_str();
        let listing = self
            .license_releases(&[("plugin", plugin_code), ("os", os), ("arch", arch)])
            .await?;
        if listing.plugin_code != plugin_code {
            return Err("license server returned another plugin".into());
        }
        let version = wanted.trim_start_matches('v');
        let release = listing
            .releases
            .iter()
            .find(|release| release.release_id == wanted || release.version.trim_start_matches('v') == version)
            .ok_or_else(|| format!("the license server has no release {wanted} of {plugin_code} for {os}-{arch}"))?;
        if release.size == 0 || release.size > MAX_PLUGIN_BYTES || !valid_sha256(&release.sha256) {
            return Err("license server release has invalid integrity metadata".into());
        }
        let base = self
            .license_base
            .as_ref()
            .ok_or_else(|| "no license-server fallback is configured".to_owned())?;
        if !release.download_path.starts_with("/v1/plugin-releases/") {
            return Err("license server download path is invalid".into());
        }
        let url = base.join(&release.download_path).map_err(|error| error.to_string())?;
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| format!("license server download failed: {error}"))?;
        if response.status() != StatusCode::OK {
            return Err(format!("license server download answered HTTP {}", response.status()));
        }
        let mut bytes = Vec::with_capacity(release.size as usize);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("license server download failed: {error}"))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > release.size {
                return Err("license server artifact exceeds its declared size".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let extension = match os {
            "windows" => "dll",
            "macos" => "dylib",
            _ => "so",
        };
        let manifest = GithubPluginManifest {
            schema: 1,
            plugin_code: plugin_code.to_owned(),
            release_id: release.release_id.clone(),
            version: release.version.clone(),
            os: os.to_owned(),
            arch: arch.to_owned(),
            plugin_abi: release.plugin_abi,
            asset: format!("{}.{extension}", release.release_id),
            sha256: format!("sha256:{}", release.sha256.trim_start_matches("sha256:")),
            size: release.size,
            feature_set: release.feature_set.clone(),
            changelog: None,
            preferences: repository.manifest.preferences.clone(),
        };
        validate_manifest(&manifest, plugin_code, &release.release_id, os, arch)?;
        validate_integrity(&manifest, &bytes, None)?;
        Ok(DownloadedRelease { manifest, bytes })
    }

    /// One GET to the GitHub API: conditional on the last ETag for that URL,
    /// authenticated when a token is configured, and bounded to `max` bytes.
    async fn api_get(&self, url: &str, max: usize, what: &str) -> Result<ApiAnswer, String> {
        let mut request = self
            .client
            .get(url)
            .header(header::ACCEPT, "application/vnd.github+json");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let cached = self
            .etags
            .lock()
            .ok()
            .and_then(|cache| cache.get(url).cloned());
        if let Some((etag, _)) = &cached {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("GitHub {what} lookup failed: {error}"))?;
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED {
            if let Some((_, body)) = cached {
                return Ok(ApiAnswer {
                    status: StatusCode::OK,
                    body,
                    rate_limit: None,
                });
            }
        }
        if status != StatusCode::OK {
            return Ok(ApiAnswer {
                status,
                body: Vec::new(),
                rate_limit: rate_limit_message(response.headers(), self.token.is_some()),
            });
        }
        let etag = response
            .headers()
            .get(header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = read_limited(response, max, what).await?;
        if let Some(etag) = etag {
            if let Ok(mut cache) = self.etags.lock() {
                cache.insert(url.to_owned(), (etag, body.clone()));
            }
        }
        Ok(ApiAnswer {
            status,
            body,
            rate_limit: None,
        })
    }

    #[cfg(test)]
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

    pub(crate) async fn inspect_repository(
        &self,
        repository_url: &str,
        os: &str,
        arch: &str,
    ) -> Result<GithubRepository, String> {
        let canonical_url = canonical_repository(repository_url)?;
        let repository = canonical_repository_parts(&canonical_url)?;
        let api_root = self.api_base.as_str().trim_end_matches('/');
        let repo_api = format!("{api_root}/repos/{}/{}", repository.owner, repository.repo);
        self.ensure_public_repository(&repo_api).await?;

        let (manifest_path, source) = if let Some(bytes) = self
            .read_repository_manifest(&repo_api, "manifest.json")
            .await?
        {
            let manifest: GithubRepositoryManifest = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid repository manifest.json: {error}"))?;
            validate_repository_manifest(&manifest)?;
            (
                "manifest.json".to_owned(),
                RepositoryManifestSource::Distribution(manifest),
            )
        } else {
            let bytes = self
                .read_repository_manifest(&repo_api, "ui/manifest.json")
                .await?
                .ok_or_else(|| {
                    "public repository must contain manifest.json or ui/manifest.json".to_owned()
                })?;
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid ui/manifest.json: {error}"))?;
            let plugin_code = value
                .get("plugin_code")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "ui/manifest.json is missing plugin_code".to_owned())?
                .to_owned();
            let ui_manifest =
                crate::plugin_ui::PluginUiManifest::parse(&plugin_code, value.clone())
                    .map_err(|error| format!("invalid ui/manifest.json: {error}"))?;
            let display_name = ui_manifest
                .menu
                .iter()
                .find(|item| item.id == plugin_code)
                .or_else(|| ui_manifest.menu.first())
                .and_then(|item| item.label.get("en").or_else(|| item.label.values().next()))
                .cloned()
                .unwrap_or_else(|| plugin_code.clone());
            (
                "ui/manifest.json".to_owned(),
                RepositoryManifestSource::PluginUi {
                    plugin_code,
                    display_name,
                    value,
                },
            )
        };
        let target = format!("{os}-{arch}");
        let artifact_name = match &source {
            RepositoryManifestSource::Distribution(manifest) => {
                manifest.artifacts.get(&target).cloned()
            }
            RepositoryManifestSource::PluginUi { .. } => None,
        };
        let platform_manifest_asset = format!("pom-plugin-{os}-{arch}.json");

        let mut summaries = Vec::new();
        let mut ui_distribution_manifest = None;
        let mut total_metadata_bytes = 0usize;
        let mut seen_release_ids = std::collections::HashSet::new();
        let mut page = 1usize;
        loop {
            let answer = self
                .api_get(
                    &format!("{repo_api}/releases?per_page={GITHUB_RELEASES_PER_PAGE}&page={page}"),
                    MAX_GITHUB_METADATA_BYTES,
                    "releases",
                )
                .await?;
            if answer.status != StatusCode::OK {
                return Err(answer.error("releases"));
            }
            let release_bytes = answer.body;
            total_metadata_bytes = total_metadata_bytes.saturating_add(release_bytes.len());
            if total_metadata_bytes > MAX_GITHUB_RELEASE_METADATA_BYTES {
                return Err("GitHub release history exceeds the metadata limit".into());
            }
            let releases: Vec<GithubRelease> = serde_json::from_slice(&release_bytes)
                .map_err(|error| format!("invalid GitHub releases response: {error}"))?;
            let page_count = releases.len();
            for release in releases.into_iter().filter(|release| !release.draft) {
                if release.id == 0
                    || release.tag_name.trim().is_empty()
                    || release.tag_name.len() > 128
                    || !seen_release_ids.insert(release.id)
                {
                    continue;
                }
                let (asset_name, asset_size, available_for_node) = match &source {
                    RepositoryManifestSource::Distribution(_) => {
                        let asset = artifact_name.as_deref().and_then(|name| {
                            let matches = release
                                .assets
                                .iter()
                                .filter(|asset| asset.name == name)
                                .collect::<Vec<_>>();
                            match matches.as_slice() {
                                [asset]
                                    if asset.size > 0
                                        && asset.size <= MAX_PLUGIN_BYTES
                                        && asset
                                            .digest
                                            .as_deref()
                                            .is_some_and(valid_github_sha256) =>
                                {
                                    Some(*asset)
                                }
                                _ => None,
                            }
                        });
                        (
                            asset.map(|asset| asset.name.clone()),
                            asset.map(|asset| asset.size),
                            asset.is_some(),
                        )
                    }
                    RepositoryManifestSource::PluginUi { plugin_code, .. } => {
                        let metadata = release
                            .assets
                            .iter()
                            .filter(|asset| asset.name == platform_manifest_asset)
                            .collect::<Vec<_>>();
                        let metadata = match metadata.as_slice() {
                            [asset]
                                if asset.size > 0
                                    && asset.size <= MAX_MANIFEST_BYTES as u64
                                    && asset.digest.as_deref().is_some_and(valid_github_sha256) =>
                            {
                                Some(*asset)
                            }
                            _ => None,
                        };
                        if ui_distribution_manifest.is_none() {
                            if let Some(metadata_asset) = metadata {
                                let bytes = self.download_asset(metadata_asset).await?;
                                validate_api_digest(&bytes, metadata_asset.digest.as_deref())?;
                                let distribution_manifest: GithubPluginManifest =
                                    serde_json::from_slice(&bytes).map_err(|error| {
                                        format!(
                                            "invalid release asset {platform_manifest_asset}: {error}"
                                        )
                                    })?;
                                validate_manifest(
                                    &distribution_manifest,
                                    plugin_code,
                                    &release.tag_name,
                                    os,
                                    arch,
                                )?;
                                let binary = exactly_one_asset(
                                    &release.assets,
                                    &distribution_manifest.asset,
                                )?;
                                validate_release_binary_metadata(&distribution_manifest, binary)?;
                                ui_distribution_manifest = Some(distribution_manifest);
                            }
                        }
                        (None, None, metadata.is_some())
                    }
                };
                summaries.push(GithubRepositoryRelease {
                    release_id: release.id.to_string(),
                    tag_name: release.tag_name,
                    name: truncate_github_text(release.name, 512),
                    changelog: truncate_github_text(release.body, MAX_GITHUB_RELEASE_NOTES_BYTES),
                    published_at: release.published_at,
                    prerelease: release.prerelease,
                    asset_name,
                    asset_size,
                    available_for_node,
                });
            }
            if page_count < GITHUB_RELEASES_PER_PAGE {
                break;
            }
            page = page
                .checked_add(1)
                .ok_or_else(|| "GitHub release page index overflowed".to_owned())?;
        }

        let (manifest, ui_manifest) = match source {
            RepositoryManifestSource::Distribution(manifest) => (manifest, None),
            RepositoryManifestSource::PluginUi {
                plugin_code,
                display_name,
                value,
            } => {
                let distribution = ui_distribution_manifest.ok_or_else(|| {
                    format!(
                        "no published release contains a valid {platform_manifest_asset} for this machine"
                    )
                })?;
                let manifest = GithubRepositoryManifest {
                    schema: 1,
                    plugin_code,
                    display_name,
                    description: None,
                    plugin_abi: distribution.plugin_abi,
                    feature_set: distribution.feature_set,
                    artifacts: std::collections::BTreeMap::from([(target, distribution.asset)]),
                    preferences: distribution.preferences,
                    screenshots: Vec::new(),
                };
                validate_repository_manifest(&manifest)?;
                (manifest, Some(value))
            }
        };
        let (description, screenshots) = match &ui_manifest {
            Some(value) => {
                let parsed =
                    crate::plugin_ui::PluginUiManifest::parse(&manifest.plugin_code, value.clone())
                        .map_err(|error| format!("invalid ui/manifest.json: {error}"))?;
                (parsed.description, parsed.screenshots)
            }
            None => (
                manifest
                    .description
                    .iter()
                    .map(|text| ("en".to_owned(), text.clone()))
                    .collect(),
                manifest.screenshots.clone(),
            ),
        };
        let screenshot_urls = screenshots
            .iter()
            .map(|shot| screenshot_url(&repository, shot))
            .collect();
        let id = hex::encode(sha2::Sha256::digest(canonical_url.as_bytes()));
        Ok(GithubRepository {
            description,
            screenshot_urls,
            id,
            repository_url: canonical_url,
            manifest_path,
            manifest,
            ui_manifest,
            releases: summaries,
            checked_at: chrono::Utc::now().to_rfc3339(),
            check_error: None,
        })
    }

    async fn read_repository_manifest(
        &self,
        repo_api: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        let answer = self
            .api_get(
                &format!("{repo_api}/contents/{path}"),
                MAX_MANIFEST_BYTES * 2,
                path,
            )
            .await?;
        if answer.status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if answer.status != StatusCode::OK {
            return Err(answer.error(path));
        }
        let response_bytes = answer.body;
        let contents: GithubContentsFile = serde_json::from_slice(&response_bytes)
            .map_err(|error| format!("invalid GitHub {path} response: {error}"))?;
        if contents.file_type != "file"
            || contents.path != path
            || contents.encoding != "base64"
            || contents.size == 0
            || contents.size > MAX_MANIFEST_BYTES as u64
        {
            return Err(format!("repository {path} is not a bounded manifest file"));
        }
        let encoded = contents
            .content
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("invalid base64 in {path}: {error}"))?;
        if bytes.len() as u64 != contents.size || bytes.len() > MAX_MANIFEST_BYTES {
            return Err(format!(
                "repository {path} size does not match GitHub metadata"
            ));
        }
        Ok(Some(bytes))
    }

    /// The selected release, from GitHub or, when GitHub fails, the same
    /// version from the license server.
    pub(crate) async fn download_release(
        &self,
        repository: &GithubRepository,
        release_id: &str,
        os: &str,
        arch: &str,
    ) -> Result<DownloadedRelease, String> {
        if let Some(license_id) = release_id.strip_prefix(LICENSE_RELEASE_PREFIX) {
            return self
                .download_from_license_server(repository, license_id, os, arch)
                .await;
        }
        match self.download_github_release(repository, release_id, os, arch).await {
            Ok(downloaded) => Ok(downloaded),
            Err(github_error) if self.license_base.is_some() => {
                let tag = repository
                    .releases
                    .iter()
                    .find(|release| release.release_id == release_id)
                    .map(|release| release.tag_name.clone())
                    .ok_or(github_error.clone())?;
                tracing::warn!(%github_error, tag, "GitHub refused the plugin release; trying the license server");
                self.download_from_license_server(repository, &tag, os, arch)
                    .await
                    .map_err(|license_error| {
                        format!("{github_error}; license server fallback: {license_error}")
                    })
            }
            Err(error) => Err(error),
        }
    }

    async fn download_github_release(
        &self,
        repository: &GithubRepository,
        release_id: &str,
        os: &str,
        arch: &str,
    ) -> Result<DownloadedRelease, String> {
        if !release_id.bytes().all(|byte| byte.is_ascii_digit()) || release_id.is_empty() {
            return Err("GitHub release ID is invalid".into());
        }
        let selected = repository
            .releases
            .iter()
            .find(|release| release.release_id == release_id)
            .ok_or_else(|| "selected release is not published in this repository".to_owned())?;
        if !selected.available_for_node {
            return Err("selected release has no compatible artifact for this machine".into());
        }
        let canonical_url = canonical_repository(&repository.repository_url)?;
        let parts = canonical_repository_parts(&canonical_url)?;
        let release_api = format!(
            "{}/repos/{}/{}/releases/{release_id}",
            self.api_base.as_str().trim_end_matches('/'),
            parts.owner,
            parts.repo
        );
        let answer = self
            .api_get(&release_api, MAX_GITHUB_METADATA_BYTES, "release")
            .await?;
        if answer.status != StatusCode::OK {
            return Err(answer.error("release"));
        }
        let bytes = answer.body;
        let release: GithubRelease = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid GitHub release response: {error}"))?;
        if release.id.to_string() != release_id || release.draft {
            return Err("selected GitHub release is no longer published".into());
        }
        if repository.manifest_path == "ui/manifest.json" {
            let manifest_asset_name = format!("pom-plugin-{os}-{arch}.json");
            let manifest_asset = exactly_one_asset(&release.assets, &manifest_asset_name)?;
            if manifest_asset.size == 0
                || manifest_asset.size > MAX_MANIFEST_BYTES as u64
                || !manifest_asset
                    .digest
                    .as_deref()
                    .is_some_and(valid_github_sha256)
            {
                return Err(format!(
                    "release manifest asset {manifest_asset_name} has invalid integrity metadata"
                ));
            }
            let manifest_bytes = self.download_asset(manifest_asset).await?;
            validate_api_digest(&manifest_bytes, manifest_asset.digest.as_deref())?;
            let mut manifest: GithubPluginManifest = serde_json::from_slice(&manifest_bytes)
                .map_err(|error| format!("invalid release asset {manifest_asset_name}: {error}"))?;
            validate_manifest(
                &manifest,
                &repository.manifest.plugin_code,
                &release.tag_name,
                os,
                arch,
            )?;
            if manifest.changelog.is_none() {
                manifest.changelog = release.body.clone().filter(|body| !body.trim().is_empty());
            }
            let asset = exactly_one_asset(&release.assets, &manifest.asset)?;
            validate_release_binary_metadata(&manifest, asset)?;
            let binary = self.download_asset(asset).await?;
            validate_integrity(&manifest, &binary, asset.digest.as_deref())?;
            return Ok(DownloadedRelease {
                manifest,
                bytes: binary,
            });
        }
        let target = format!("{os}-{arch}");
        let asset_name = repository
            .manifest
            .artifacts
            .get(&target)
            .ok_or_else(|| "repository manifest does not support this machine".to_owned())?;
        let asset = exactly_one_asset(&release.assets, asset_name)?;
        if asset.size == 0
            || asset.size > MAX_PLUGIN_BYTES
            || !asset.digest.as_deref().is_some_and(valid_github_sha256)
        {
            return Err("release artifact must include a valid GitHub SHA-256 digest".into());
        }
        let binary = self.download_asset(asset).await?;
        validate_api_digest(&binary, asset.digest.as_deref())?;
        let digest = asset
            .digest
            .clone()
            .expect("validated GitHub SHA-256 digest");
        let manifest = GithubPluginManifest {
            schema: 1,
            plugin_code: repository.manifest.plugin_code.clone(),
            release_id: release_id.to_owned(),
            version: release.tag_name.clone(),
            os: os.to_owned(),
            arch: arch.to_owned(),
            plugin_abi: repository.manifest.plugin_abi,
            asset: asset.name.clone(),
            sha256: digest,
            size: asset.size,
            feature_set: repository.manifest.feature_set.clone(),
            changelog: release.body,
            preferences: repository.manifest.preferences.clone(),
        };
        validate_manifest(&manifest, &manifest.plugin_code, release_id, os, arch)?;
        validate_integrity(&manifest, &binary, asset.digest.as_deref())?;
        Ok(DownloadedRelease {
            manifest,
            bytes: binary,
        })
    }

    async fn ensure_public_repository(&self, repo_api: &str) -> Result<(), String> {
        let answer = self
            .api_get(repo_api, MAX_GITHUB_METADATA_BYTES, "repository")
            .await?;
        if answer.status != StatusCode::OK {
            return Err(answer.error("repository"));
        }
        let bytes = answer.body;
        let metadata: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid GitHub repository response: {error}"))?;
        if metadata.get("private").and_then(serde_json::Value::as_bool) != Some(false) {
            return Err("repository must be public and accessible".into());
        }
        Ok(())
    }

    async fn download_asset(&self, asset: &GithubAsset) -> Result<Vec<u8>, String> {
        let source = asset
            .browser_download_url
            .as_deref()
            .filter(|url| !url.is_empty())
            .unwrap_or(&asset.url);
        let url = reqwest::Url::parse(source)
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
    if segments.len() != 2 || segments.iter().any(|segment| !is_github_segment(segment)) {
        return Err("URL must be https://github.com/owner/repository".into());
    }
    let repository = if segments[1].to_ascii_lowercase().ends_with(".git") {
        &segments[1][..segments[1].len() - 4]
    } else {
        segments[1]
    };
    if !is_github_segment(repository) {
        return Err("URL must be https://github.com/owner/repository".into());
    }
    Ok(format!("https://github.com/{}/{}", segments[0], repository))
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

/// Resolve uma captura de tela declarada pelo repositório. Os caminhos já
/// foram validados por `plugin_ui::valid_screenshot`.
fn screenshot_url(repository: &CanonicalRepository, shot: &str) -> String {
    if shot.starts_with("https://") {
        shot.to_owned()
    } else {
        format!(
            "https://raw.githubusercontent.com/{}/{}/HEAD/{}",
            repository.owner, repository.repo, shot
        )
    }
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

fn truncate_github_text(value: Option<String>, limit: usize) -> Option<String> {
    value.map(|mut text| {
        if text.len() > limit {
            let mut end = limit;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push('…');
        }
        text
    })
}

#[derive(Debug, Deserialize)]
struct GithubContentsFile {
    #[serde(rename = "type")]
    file_type: String,
    path: String,
    encoding: String,
    size: u64,
    content: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    id: u64,
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
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
    /// `browser_download_url`. The download goes through the second: the
    /// CDN does not count against the API's hourly limit, and the bytes are
    /// still checked against the API's `digest`.
    url: String,
    #[serde(default)]
    browser_download_url: Option<String>,
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

fn validate_release_binary_metadata(
    manifest: &GithubPluginManifest,
    asset: &GithubAsset,
) -> Result<(), String> {
    let Some(api_digest) = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
    else {
        return Err("plugin release binary is missing its GitHub SHA-256 digest".into());
    };
    let manifest_digest = manifest
        .sha256
        .strip_prefix("sha256:")
        .unwrap_or(&manifest.sha256);
    if asset.size != manifest.size
        || asset.size == 0
        || asset.size > MAX_PLUGIN_BYTES
        || !valid_sha256(api_digest)
        || !api_digest.eq_ignore_ascii_case(manifest_digest)
    {
        return Err("plugin binary metadata does not match its release manifest".into());
    }
    Ok(())
}

pub(crate) fn validate_repository_manifest(
    manifest: &GithubRepositoryManifest,
) -> Result<(), String> {
    let valid_code = {
        let bytes = manifest.plugin_code.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 128
            && bytes[0].is_ascii_alphanumeric()
            && bytes[bytes.len() - 1].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    };
    if manifest.schema != 1
        || !valid_code
        || manifest.display_name.trim().is_empty()
        || manifest.display_name.len() > 200
        || manifest
            .description
            .as_ref()
            .is_some_and(|value| value.len() > 4000)
        || manifest.plugin_abi != enterprise_protocol::PLUGIN_ABI_VERSION
        || manifest.feature_set.len() > 128
        || manifest.artifacts.is_empty()
        || manifest.artifacts.len() > 32
        || manifest.feature_set.iter().any(|feature| {
            !feature.starts_with(&format!("{}.", manifest.plugin_code))
                || feature.trim() != feature
                || feature.len() > 200
        })
    {
        return Err("repository manifest has invalid plugin metadata".into());
    }
    let mut features = std::collections::HashSet::new();
    for feature in &manifest.feature_set {
        let prefix = format!("{}.", manifest.plugin_code);
        let Some(name) = feature.strip_prefix(&prefix) else {
            return Err("repository manifest has a feature outside its plugin code".into());
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || !features.insert(feature)
        {
            return Err("repository manifest has an invalid or duplicate feature".into());
        }
    }
    for (target, asset) in &manifest.artifacts {
        let Some((os, arch)) = target.split_once('-') else {
            return Err("repository manifest has an invalid artifact target".into());
        };
        let expected_extension = match os {
            "windows" => ".dll",
            "macos" => ".dylib",
            "linux" => ".so",
            _ => return Err("repository manifest has an unsupported artifact platform".into()),
        };
        if !matches!(arch, "aarch64" | "x86_64")
            || !is_github_segment(asset)
            || asset.len() > 255
            || !asset.ends_with(expected_extension)
        {
            return Err("repository manifest has an invalid plugin artifact name".into());
        }
    }
    if let Some(preferences) = &manifest.preferences {
        validate_preferences_schema(preferences)?;
    }
    if manifest.screenshots.len() > 8
        || !manifest
            .screenshots
            .iter()
            .all(|shot| crate::plugin_ui::valid_screenshot(shot))
    {
        return Err("repository manifest has invalid screenshots".into());
    }
    Ok(())
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
        || manifest.version.trim().is_empty()
        || manifest.version.len() > 128
        || !safe_release_id(&manifest.release_id)
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

fn valid_github_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|digest| valid_sha256(digest))
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

impl ApiAnswer {
    fn error(&self, action: &str) -> String {
        self.rate_limit
            .clone()
            .unwrap_or_else(|| github_status_error(self.status, action))
    }
}

/// The message for an exhausted API limit, with when it resets, or `None`
/// when the refusal was something else.
fn rate_limit_message(headers: &reqwest::header::HeaderMap, authenticated: bool) -> Option<String> {
    let value = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    if value("x-ratelimit-remaining") != Some("0") {
        return None;
    }
    let reset = value("x-ratelimit-reset")
        .and_then(|reset| reset.parse::<i64>().ok())
        .and_then(|reset| chrono::DateTime::from_timestamp(reset, 0))
        .map(|reset| format!(" until {} UTC", reset.format("%H:%M")));
    let limit = value("x-ratelimit-limit").unwrap_or("60");
    Some(format!(
        "GitHub API limit reached ({limit} calls per hour{}){}{}",
        if authenticated { " for this token" } else { " for this IP without a token" },
        reset.unwrap_or_default(),
        if authenticated {
            ""
        } else {
            "; set POM_GITHUB_TOKEN to raise it to 5000 per hour"
        }
    ))
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[tokio::test]
    async fn when_github_refuses_the_license_server_lists_and_serves_the_same_release() {
        use axum::http::StatusCode as HttpStatus;
        use axum::routing::get;

        let binary = b"plugin from the license server".to_vec();
        let sha = hex::encode(sha2::Sha256::digest(&binary));
        let listing = serde_json::json!({
            "plugin_code": "acme_plugin",
            "display_name": "Acme Plugin",
            "description": "From the license server.",
            "releases": [{
                "release_id": "acme_plugin-1.0.1-linux-x86_64",
                "version": "1.0.1",
                "plugin_abi": enterprise_protocol::PLUGIN_ABI_VERSION,
                "size": binary.len(),
                "sha256": sha,
                "feature_set": ["acme_plugin.core"],
                "published_at": "2026-10-07T12:00:00Z",
                "download_path": "/v1/plugin-releases/acme_plugin-1.0.1-linux-x86_64/download"
            }]
        });
        let served = binary.clone();
        let app = axum::Router::new()
            // GitHub: every API call refused, as when the hourly limit runs out.
            .route("/repos/*rest", get(|| async { HttpStatus::FORBIDDEN }))
            .route("/v1/plugin-releases", get(move || {
                let listing = listing.clone();
                async move { axum::Json(listing) }
            }))
            .route(
                "/v1/plugin-releases/acme_plugin-1.0.1-linux-x86_64/download",
                get(move || {
                    let served = served.clone();
                    async move { served }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = GithubReleaseClient::with_api_base(&base)
            .unwrap()
            .with_license_fallback(&base);

        let github_error = client
            .inspect_repository("https://github.com/acme/new-plugin", "linux", "x86_64")
            .await
            .unwrap_err();
        let repository = client
            .inspect_from_license_server("https://github.com/acme/new-plugin", "linux", "x86_64", &github_error)
            .await
            .unwrap();
        assert_eq!(repository.manifest.plugin_code, "acme_plugin");
        assert_eq!(repository.releases.len(), 1);
        assert_eq!(repository.releases[0].tag_name, "v1.0.1");
        assert!(repository.releases[0].available_for_node);
        assert!(repository.check_error.as_deref().unwrap().contains("license server"));

        let downloaded = client
            .download_release(&repository, &repository.releases[0].release_id, "linux", "x86_64")
            .await
            .unwrap();
        assert_eq!(downloaded.bytes, binary);
        assert_eq!(downloaded.manifest.version, "1.0.1");

        // A release listed by GitHub earlier: GitHub refuses now, so the same
        // version comes from the license server.
        let mut from_github = repository.clone();
        from_github.manifest_path = "manifest.json".into();
        from_github.releases[0].release_id = "123".into();
        let downloaded = client
            .download_release(&from_github, "123", "linux", "x86_64")
            .await
            .unwrap();
        assert_eq!(downloaded.bytes, binary);
    }

    #[test]
    fn an_exhausted_limit_says_when_it_resets_and_how_to_raise_it() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        headers.insert("x-ratelimit-limit", "60".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1791292156".parse().unwrap());
        let message = rate_limit_message(&headers, false).unwrap();
        assert!(message.contains("60 calls per hour"), "{message}");
        assert!(message.contains("until 13:09 UTC"), "{message}");
        assert!(message.contains("POM_GITHUB_TOKEN"), "{message}");
        assert!(!rate_limit_message(&headers, true).unwrap().contains("POM_GITHUB_TOKEN"));
        headers.insert("x-ratelimit-remaining", "12".parse().unwrap());
        assert!(rate_limit_message(&headers, false).is_none());
    }

    #[tokio::test]
    async fn api_answers_are_revalidated_with_their_etag_and_assets_come_from_the_cdn() {
        use axum::http::{HeaderMap, StatusCode as HttpStatus};
        use axum::routing::get;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let conditional = Arc::new(AtomicUsize::new(0));
        let seen = conditional.clone();
        let app = axum::Router::new()
            .route(
                "/repos/acme/plugin",
                get(move |headers: HeaderMap| {
                    let seen = seen.clone();
                    async move {
                        if headers.get("if-none-match").and_then(|v| v.to_str().ok()) == Some("\"v1\"") {
                            seen.fetch_add(1, Ordering::SeqCst);
                            return (HttpStatus::NOT_MODIFIED, [("etag", "\"v1\"")], String::new());
                        }
                        (HttpStatus::OK, [("etag", "\"v1\"")], "{\"private\":false}".to_owned())
                    }
                }),
            )
            .route("/api-asset", get(|| async { HttpStatus::FORBIDDEN }))
            .route("/cdn-asset", get(|| async { "plugin bytes" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = GithubReleaseClient::with_api_base(&base).unwrap();

        let url = format!("{base}/repos/acme/plugin");
        let first = client.api_get(&url, 1024, "repository").await.unwrap();
        let second = client.api_get(&url, 1024, "repository").await.unwrap();
        assert_eq!(second.status, StatusCode::OK);
        assert_eq!(second.body, first.body);
        assert_eq!(conditional.load(Ordering::SeqCst), 1, "the second call carried the ETag");

        let asset = GithubAsset {
            name: "plugin.so".into(),
            size: 12,
            url: format!("{base}/api-asset"),
            browser_download_url: Some(format!("{base}/cdn-asset")),
            digest: None,
        };
        assert_eq!(client.download_asset(&asset).await.unwrap(), b"plugin bytes");
    }

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
        assert_eq!(
            canonical_repository("https://github.com/pom-org/request-logs.git").unwrap(),
            "https://github.com/pom-org/request-logs"
        );
        for invalid in [
            "http://github.com/owner/repo",
            "https://github.com/owner/repo/tree/main",
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
                "id": 12,
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

    pub(crate) async fn github_repository_stub(
    ) -> (GithubReleaseClient, String, tokio::task::JoinHandle<()>) {
        use axum::extract::{Path as AxumPath, State};
        use axum::routing::get;
        use axum::{Json, Router};
        use std::sync::Arc;

        struct RepositoryState {
            manifest: Vec<u8>,
            older: Vec<u8>,
            newer: Vec<u8>,
            api_base: String,
        }

        async fn repository() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "private": false, "archived": false }))
        }

        async fn manifest_contents(
            State(state): State<Arc<RepositoryState>>,
        ) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "type": "file",
                "path": "manifest.json",
                "encoding": "base64",
                "size": state.manifest.len(),
                "content": base64::engine::general_purpose::STANDARD.encode(&state.manifest)
            }))
        }

        fn release_json(state: &RepositoryState, id: u64) -> serde_json::Value {
            let (tag, name, binary, path, prerelease) = if id == 11 {
                ("v0.9.0", "Earlier release", &state.older, "older", true)
            } else {
                ("v2.0.0", "Latest release", &state.newer, "newer", false)
            };
            let digest = hex::encode(sha2::Sha256::digest(binary));
            serde_json::json!({
                "id": id,
                "tag_name": tag,
                "name": name,
                "body": format!("Release notes for {tag}"),
                "published_at": "2026-09-24T00:00:00Z",
                "draft": false,
                "prerelease": prerelease,
                "assets": [{
                    "name": "acme-plugin.so",
                    "size": binary.len(),
                    "digest": format!("sha256:{digest}"),
                    "url": format!("{}/assets/{path}", state.api_base)
                }]
            })
        }

        async fn releases(State(state): State<Arc<RepositoryState>>) -> Json<serde_json::Value> {
            Json(serde_json::json!([
                release_json(&state, 22),
                release_json(&state, 11)
            ]))
        }

        async fn release(
            AxumPath(id): AxumPath<u64>,
            State(state): State<Arc<RepositoryState>>,
        ) -> Json<serde_json::Value> {
            Json(release_json(&state, id))
        }

        async fn artifact(
            AxumPath(name): AxumPath<String>,
            State(state): State<Arc<RepositoryState>>,
        ) -> impl axum::response::IntoResponse {
            let body = if name == "older" {
                state.older.clone()
            } else {
                state.newer.clone()
            };
            ([("content-type", "application/octet-stream")], body)
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let api_base = format!("http://{address}");
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "plugin_code": "acme_plugin",
            "display_name": "Acme Plugin",
            "description": "A repository-owned POM plugin.",
            "plugin_abi": enterprise_protocol::PLUGIN_ABI_VERSION,
            "feature_set": ["acme_plugin.core"],
            "artifacts": {"linux-x86_64": "acme-plugin.so"},
            "preferences": null
        }))
        .unwrap();
        let state = Arc::new(RepositoryState {
            manifest,
            older: b"older plugin binary".to_vec(),
            newer: b"newer plugin binary".to_vec(),
            api_base: api_base.clone(),
        });
        let app = Router::new()
            .route("/repos/acme/new-plugin", get(repository))
            .route(
                "/repos/acme/new-plugin/contents/manifest.json",
                get(manifest_contents),
            )
            .route("/repos/acme/new-plugin/releases", get(releases))
            .route("/repos/acme/new-plugin/releases/:id", get(release))
            .route("/assets/:name", get(artifact))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = GithubReleaseClient::with_api_base(&api_base).unwrap();
        (client, api_base, server)
    }

    pub(crate) async fn github_ui_manifest_repository_stub(
    ) -> (GithubReleaseClient, String, tokio::task::JoinHandle<()>) {
        use axum::extract::{Path as AxumPath, State};
        use axum::http::StatusCode as HttpStatus;
        use axum::routing::get;
        use axum::{Json, Router};
        use std::sync::Arc;

        struct RepositoryState {
            ui_manifest: Vec<u8>,
            release_manifest: Vec<u8>,
            binary: Vec<u8>,
            api_base: String,
        }

        async fn repository() -> Json<serde_json::Value> {
            Json(serde_json::json!({ "private": false, "archived": false }))
        }

        async fn root_manifest() -> HttpStatus {
            HttpStatus::NOT_FOUND
        }

        async fn ui_manifest_contents(
            State(state): State<Arc<RepositoryState>>,
        ) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "type": "file",
                "path": "ui/manifest.json",
                "encoding": "base64",
                "size": state.ui_manifest.len(),
                "content": base64::engine::general_purpose::STANDARD.encode(&state.ui_manifest)
            }))
        }

        fn release_json(state: &RepositoryState) -> serde_json::Value {
            let manifest_digest = hex::encode(sha2::Sha256::digest(&state.release_manifest));
            let binary_digest = hex::encode(sha2::Sha256::digest(&state.binary));
            serde_json::json!({
                "id": 505,
                "tag_name": "v0.0.5",
                "name": "Base Plugin 0.0.5",
                "body": "Published base plugin",
                "published_at": "2026-09-27T00:00:00Z",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {
                        "name": "pom-plugin-macos-aarch64.json",
                        "size": state.release_manifest.len(),
                        "digest": format!("sha256:{manifest_digest}"),
                        "url": format!("{}/assets/manifest", state.api_base)
                    },
                    {
                        "name": "pom-plugin-base-macos-aarch64.dylib",
                        "size": state.binary.len(),
                        "digest": format!("sha256:{binary_digest}"),
                        "url": format!("{}/assets/plugin", state.api_base)
                    }
                ]
            })
        }

        async fn releases(State(state): State<Arc<RepositoryState>>) -> Json<serde_json::Value> {
            Json(serde_json::json!([release_json(&state)]))
        }

        async fn release(
            AxumPath(_id): AxumPath<u64>,
            State(state): State<Arc<RepositoryState>>,
        ) -> Json<serde_json::Value> {
            Json(release_json(&state))
        }

        async fn asset(
            AxumPath(name): AxumPath<String>,
            State(state): State<Arc<RepositoryState>>,
        ) -> impl axum::response::IntoResponse {
            let (content_type, body) = if name == "manifest" {
                ("application/json", state.release_manifest.clone())
            } else {
                ("application/octet-stream", state.binary.clone())
            };
            ([("content-type", content_type)], body)
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let api_base = format!("http://{address}");
        let binary = b"base plugin binary".to_vec();
        let binary_digest = hex::encode(sha2::Sha256::digest(&binary));
        let release_manifest = serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "plugin_code": "base",
            "release_id": "v0.0.5",
            "version": "0.0.5",
            "os": "macos",
            "arch": "aarch64",
            "plugin_abi": enterprise_protocol::PLUGIN_ABI_VERSION,
            "asset": "pom-plugin-base-macos-aarch64.dylib",
            "sha256": format!("sha256:{binary_digest}"),
            "size": binary.len(),
            "feature_set": ["base.core"],
            "preferences": {
                "features": [{
                    "key": "example_feature_enabled",
                    "label": "Example feature",
                    "default": false,
                    "description": "An example boolean preference for plugin authors."
                }],
                "storage_directory": { "label": "Plugin data directory", "default": null }
            }
        }))
        .unwrap();
        let ui_manifest = serde_json::to_vec(&serde_json::json!({
            "schema": "pom-plugin-ui/v1",
            "plugin_code": "base",
            "icon_image": "ui/icon.png",
            "description": { "en": "Tutorial plugin.", "pt-BR": "Plugin tutorial." },
            "screenshots": ["docs/screenshots/tutorial.png", "https://example.com/shot.png"],
            "documentation": ["docs/README.md", "docs/guia-desenvolvimento.md", "docs/guia-workspace.md"],
            "menu": [
                { "id": "base", "label": { "en": "Base Plugin", "pt-BR": "Plugin de Base" }, "icon": "layout-template", "to": "/plugin-de-base" },
                { "id": "example", "label": { "en": "Example", "pt-BR": "Exemplo" }, "icon": "file-code-2", "to": "/example" }
            ],
            "routes": [
                { "path": "/plugin-de-base", "screen": "base", "full_bleed": true, "roles": ["admin"] },
                { "path": "/example", "screen": "example", "full_bleed": true }
            ],
            "screens": {
                "base": { "module": "ui/screens.js", "export": "base", "styles": ["ui/plugin.css"] },
                "example": { "module": "ui/screens.js", "export": "example", "styles": ["ui/plugin.css"] }
            },
            "i18n": { "en": "i18n/en.json", "pt-BR": "i18n/pt-BR.json" },
            "assets": ["ui/screens.js", "ui/plugin.css", "ui/icon.png", "i18n/en.json", "i18n/pt-BR.json", "docs/README.md", "docs/guia-desenvolvimento.md", "docs/guia-workspace.md"]
        }))
        .unwrap();
        let state = Arc::new(RepositoryState {
            ui_manifest,
            release_manifest,
            binary,
            api_base: api_base.clone(),
        });
        let app = Router::new()
            .route("/repos/pereirajair/pom-plugin-base", get(repository))
            .route(
                "/repos/pereirajair/pom-plugin-base/contents/manifest.json",
                get(root_manifest),
            )
            .route(
                "/repos/pereirajair/pom-plugin-base/contents/ui/manifest.json",
                get(ui_manifest_contents),
            )
            .route("/repos/pereirajair/pom-plugin-base/releases", get(releases))
            .route(
                "/repos/pereirajair/pom-plugin-base/releases/:id",
                get(release),
            )
            .route("/assets/:name", get(asset))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = GithubReleaseClient::with_api_base(&api_base).unwrap();
        (client, api_base, server)
    }

    #[tokio::test]
    async fn finds_ui_manifest_and_installs_the_selected_platform_release_manifest() {
        let (client, _api_base, server) = github_ui_manifest_repository_stub().await;
        let repository = client
            .inspect_repository(
                "https://github.com/pereirajair/pom-plugin-base",
                "macos",
                "aarch64",
            )
            .await
            .expect("read the plugin UI manifest and releases");
        assert_eq!(repository.manifest_path, "ui/manifest.json");
        assert_eq!(repository.manifest.plugin_code, "base");
        assert_eq!(repository.manifest.display_name, "Base Plugin");
        assert_eq!(repository.manifest.feature_set, ["base.core"]);
        assert_eq!(
            repository.manifest.preferences.as_ref().unwrap().features[0].key,
            "example_feature_enabled"
        );
        assert_eq!(
            repository.manifest.artifacts["macos-aarch64"],
            "pom-plugin-base-macos-aarch64.dylib"
        );
        assert_eq!(
            repository.ui_manifest.as_ref().unwrap()["documentation"][0],
            "docs/README.md"
        );
        assert_eq!(repository.releases.len(), 1);
        assert!(repository.releases[0].available_for_node);
        assert_eq!(repository.description["pt-BR"], "Plugin tutorial.");
        assert_eq!(
            repository.screenshot_urls,
            [
                "https://raw.githubusercontent.com/pereirajair/pom-plugin-base/HEAD/docs/screenshots/tutorial.png",
                "https://example.com/shot.png"
            ]
        );

        let selected = client
            .download_release(&repository, "505", "macos", "aarch64")
            .await
            .expect("download the binary named by the selected release manifest");
        assert_eq!(selected.manifest.version, "0.0.5");
        assert_eq!(selected.bytes, b"base plugin binary");
        server.abort();
    }

    #[tokio::test]
    async fn repository_manifest_and_all_published_release_choices_come_from_github() {
        let (client, _api_base, server) = github_repository_stub().await;
        let repository = client
            .inspect_repository("https://github.com/acme/new-plugin", "linux", "x86_64")
            .await
            .expect("read repository manifest and releases");
        assert_eq!(repository.manifest.plugin_code, "acme_plugin");
        assert_eq!(repository.manifest.display_name, "Acme Plugin");
        assert_eq!(repository.releases.len(), 2);
        assert!(repository
            .releases
            .iter()
            .all(|release| release.available_for_node));
        assert!(repository.releases.iter().any(|release| release.prerelease));

        let selected = client
            .download_release(&repository, "11", "linux", "x86_64")
            .await
            .expect("installable historical release");
        assert_eq!(selected.manifest.version, "v0.9.0");
        assert_eq!(selected.bytes, b"older plugin binary");
        server.abort();
    }

    #[test]
    fn repository_manifest_rejects_invalid_codes_targets_and_plugin_abi() {
        let mut manifest = GithubRepositoryManifest {
            schema: 1,
            plugin_code: "acme_plugin".into(),
            display_name: "Acme Plugin".into(),
            description: None,
            plugin_abi: enterprise_protocol::PLUGIN_ABI_VERSION,
            feature_set: vec!["acme_plugin.core".into()],
            artifacts: std::collections::BTreeMap::from([(
                "linux-x86_64".into(),
                "acme-plugin.so".into(),
            )]),
            preferences: None,
            screenshots: vec!["docs/screenshots/overview.png".into()],
        };
        validate_repository_manifest(&manifest).unwrap();
        manifest.screenshots = vec!["../secret.png".into()];
        assert!(validate_repository_manifest(&manifest).is_err());
        manifest.screenshots = vec!["https://user@example.com/shot.png".into()];
        assert!(validate_repository_manifest(&manifest).is_err());
        manifest.screenshots.clear();
        manifest.feature_set.clear();
        validate_repository_manifest(&manifest).expect("plugins may expose no optional features");
        manifest.plugin_code = "../bad".into();
        assert!(validate_repository_manifest(&manifest).is_err());
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
