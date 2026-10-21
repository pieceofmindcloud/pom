//! Detection and installation of a new release of the `pom` program itself.
//!
//! Deliberately SEPARATE from `crate::plugins`: `pom` is not a plugin, needs no
//! license and has its own clock and lock. The Plugins screen shows it in the
//! same "Check for updates" dialog as the plugins, and About shows it alone,
//! but both only call the routes at the end of this file.
//!
//! Where releases come from, in this order:
//!
//! 1. The GitHub releases of [`GITHUB_REPOSITORY`], read as a public repository
//!    (no token): the assets of a release name their platform
//!    (`pom-v0.5.6-x86_64-unknown-linux-gnu-cuda.tar.gz`,
//!    `pom-v0.5.6-x86_64.AppImage`, `...-pc-windows-msvc-cuda-vulkan.zip`,
//!    `...-aarch64-apple-darwin.dmg`), and not every release carries every
//!    platform, so the newest release with an asset for this installation wins.
//! 2. The site's download listing (`<license server>/v1/pom/latest`), which
//!    names the newest file per platform. While downloads need an invitation
//!    it gives no address, and the update is shown as found but not
//!    installable here.
//!
//! The version this binary is comes from `POM_RELEASE_VERSION`, stamped by
//! `build.rs`. Installing is the operator's explicit act: the asset is
//! downloaded and checked (size, and SHA-256 when the source publishes it),
//! unpacked next to the installation, and `pom-plugin-host --apply-update`
//! (a copy outside the install, detached) waits for this process to exit,
//! swaps the files, keeps a backup until the swap succeeds and starts POM
//! again (`crates/plugin-host/src/update.rs`). A node in a container is
//! updated by rebuilding its image, never from here.

use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, RwLock};

/// The repository whose releases are POM's.
pub(crate) const GITHUB_REPOSITORY: &str = "pieceofmindcloud/pom";
/// The release this binary was compiled as, as its tag (`v0.5.6`).
pub(crate) const RELEASE_VERSION: &str = env!("POM_RELEASE_VERSION");

/// The release this node runs: the image's runtime `POM_RELEASE_VERSION` when
/// it is set, else the compiled one. A Docker build has no `.git` and no tags,
/// so its compiled value falls back to the crate version (`v0.1.0`);
/// `scripts/build-docker.sh` reads the newest tag on the host and the final
/// stage sets it, like `POM_BUILD_COMMIT`, without invalidating Cargo.
pub(crate) fn release_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        release_version_from(std::env::var("POM_RELEASE_VERSION").ok().as_deref(), RELEASE_VERSION)
    })
}

fn release_version_from(runtime: Option<&str>, compiled: &str) -> String {
    runtime
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(compiled)
        .to_owned()
}

/// Cadence of the background check; the screen's check is immediate.
const AUTO_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Hosts a POM asset may be fetched from, after redirects.
const TRUSTED_HOSTS: [&str; 6] = [
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
    "license.pieceofmind.cloud",
];

/// A newer release found for this installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CoreUpdate {
    /// The release's tag (`v0.5.7`); what the operator confirms.
    pub release_id: String,
    pub version: String,
    pub changelog: Option<String>,
    pub published_at: Option<String>,
    /// `github` or `site`.
    pub source: String,
    pub asset: String,
    pub size: u64,
    /// Whether this node can install it itself; when not, `blocked` says why
    /// (`container`, `invitation_required`).
    pub installable: bool,
    pub blocked: Option<String>,
    #[serde(skip)]
    download_url: Option<String>,
    #[serde(skip)]
    sha256: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct CoreUpdateStatus {
    pub core_update: Option<CoreUpdate>,
    pub current_version: String,
    pub checked_at: Option<String>,
    /// Why the last check found nothing to compare with (both sources failed).
    pub check_error: Option<String>,
}

/// Where this binary was installed from, which decides the asset and the swap.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Install {
    /// A Docker image: rebuilt, never swapped in place.
    Container,
    /// The Linux AppImage file the process runs from.
    AppImage(PathBuf),
    /// The macOS bundle (`POM.app`) the executable lives in.
    MacBundle(PathBuf),
    /// The folder of an unpacked `.tar.gz` (Linux) or `.zip` (Windows).
    Directory(PathBuf),
}

impl Install {
    fn detect() -> Install {
        if Path::new("/.dockerenv").exists() || std::env::var_os("container").is_some() {
            return Install::Container;
        }
        if cfg!(target_os = "linux") {
            if let Some(image) = std::env::var_os("APPIMAGE").map(PathBuf::from) {
                return Install::AppImage(image);
            }
        }
        let executable = std::env::current_exe().unwrap_or_default();
        if cfg!(target_os = "macos") {
            if let Some(bundle) = executable
                .ancestors()
                .find(|path| path.extension().is_some_and(|extension| extension == "app"))
            {
                return Install::MacBundle(bundle.to_path_buf());
            }
        }
        Install::Directory(
            executable
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        )
    }
}

/// What this build is, for picking the asset: OS, architecture and variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Target {
    os: &'static str,
    arch: &'static str,
    /// The variant suffix of an archive (`cuda`, `cpu`, `cuda-vulkan`, or none).
    variant: Option<&'static str>,
    /// Built with the Vulkan backend too: on Linux only the CUDA + Vulkan
    /// AppImage of `release-linux.yml` is.
    vulkan: bool,
    appimage: bool,
}

impl Target {
    fn current(install: &Install) -> Target {
        let gpu = cfg!(feature = "cuda");
        let variant = match std::env::consts::OS {
            "linux" => Some(if gpu { "cuda" } else { "cpu" }),
            "windows" => gpu.then_some("cuda-vulkan"),
            _ => None,
        };
        Target {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            variant,
            vulkan: cfg!(feature = "vulkan"),
            appimage: matches!(install, Install::AppImage(_)),
        }
    }

    /// Whether `name` is this build's asset.
    fn matches(&self, name: &str) -> bool {
        let Some(rest) = name.strip_prefix("pom-") else {
            return false;
        };
        // The CUDA + Vulkan AppImage of `release-linux.yml` keeps the bare
        // name; the ksia AppImages are named like the tarballs they replaced
        // (`...-unknown-linux-gnu-{cpu|cuda}.AppImage`).
        if self.appimage && self.vulkan {
            return name.ends_with(&format!("-{}.AppImage", self.arch));
        }
        let (triples, extension): (&[&str], &str) = match self.os {
            "linux" if self.appimage => (&["unknown-linux-gnu"], ".AppImage"),
            "linux" => (&["unknown-linux-gnu"], ".tar.gz"),
            "windows" => (&["pc-windows-msvc"], ".zip"),
            "macos" => (&["apple-darwin"], ".dmg"),
            _ => return false,
        };
        let Some(stem) = rest.strip_suffix(extension) else {
            return false;
        };
        // The macOS disk image carries a universal bundle, named after arm64.
        let arches: &[&str] = if self.os == "macos" {
            &[self.arch, "universal", "aarch64"]
        } else {
            std::slice::from_ref(&self.arch)
        };
        arches.iter().any(|arch| {
            triples.iter().any(|triple| {
                let platform = format!("-{arch}-{triple}");
                match stem.find(&platform) {
                    Some(at) => {
                        let suffix = &stem[at + platform.len()..];
                        match self.variant {
                            Some(variant) => suffix == format!("-{variant}"),
                            None => suffix.is_empty(),
                        }
                    }
                    None => false,
                }
            })
        })
    }
}

/// One installable file of a release, from either source.
#[derive(Debug, Clone)]
struct Candidate {
    tag: String,
    version: semver::Version,
    changelog: Option<String>,
    published_at: Option<String>,
    asset: String,
    size: u64,
    sha256: Option<String>,
    download_url: Option<String>,
}

fn parse_version(tag: &str) -> Option<semver::Version> {
    semver::Version::parse(tag.trim().trim_start_matches(['v', 'V'])).ok()
}

/// The newest of `candidates` when it is newer than `current`.
fn newest_after(current: &str, candidates: Vec<Candidate>) -> Option<Candidate> {
    let current = parse_version(current).unwrap_or_else(|| semver::Version::new(0, 0, 0));
    candidates
        .into_iter()
        .filter(|candidate| candidate.version > current)
        .max_by(|left, right| left.version.cmp(&right.version))
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    body: Option<String>,
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
    browser_download_url: Option<String>,
    digest: Option<String>,
}

/// The releases of the GitHub listing that carry this build's asset.
fn github_candidates(releases: Vec<GithubRelease>, target: &Target) -> Vec<Candidate> {
    releases
        .into_iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| {
            let version = parse_version(&release.tag_name)?;
            let asset = release
                .assets
                .into_iter()
                .find(|asset| target.matches(&asset.name))?;
            Some(Candidate {
                tag: release.tag_name,
                version,
                changelog: release.body.filter(|body| !body.trim().is_empty()),
                published_at: release.published_at,
                asset: asset.name,
                size: asset.size,
                sha256: asset
                    .digest
                    .and_then(|digest| digest.strip_prefix("sha256:").map(str::to_owned)),
                download_url: asset.browser_download_url,
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct SiteListing {
    #[serde(default)]
    available: bool,
    #[serde(default)]
    prerelease: bool,
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<SiteAsset>,
}

#[derive(Deserialize)]
struct SiteAsset {
    name: String,
    tag: String,
    size: u64,
    download_url: Option<String>,
}

/// The site lists the newest file of each platform, each with its own tag.
fn site_candidates(listing: SiteListing, target: &Target) -> Vec<Candidate> {
    if !listing.available || listing.prerelease {
        return Vec::new();
    }
    listing
        .assets
        .into_iter()
        .filter(|asset| target.matches(&asset.name))
        .filter_map(|asset| {
            Some(Candidate {
                version: parse_version(&asset.tag)?,
                tag: asset.tag,
                changelog: None,
                published_at: listing.published_at.clone(),
                asset: asset.name,
                size: asset.size,
                sha256: None,
                download_url: asset.download_url,
            })
        })
        .collect()
}

#[derive(Clone)]
struct Sources {
    github_api: Option<String>,
    site: Option<String>,
}

impl Sources {
    fn production() -> Sources {
        if cfg!(test) {
            return Sources {
                github_api: None,
                site: None,
            };
        }
        Sources {
            github_api: Some("https://api.github.com".into()),
            site: Some(crate::plugins::license_base_url()),
        }
    }
}

pub(crate) struct PomCoreUpdater {
    client: reqwest::Client,
    sources: Sources,
    status: Arc<RwLock<CoreUpdateStatus>>,
    update_lock: Arc<Mutex<()>>,
}

impl PomCoreUpdater {
    fn new(sources: Sources) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("pom/", env!("POM_RELEASE_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.url().scheme() == "https"
                    && attempt
                        .url()
                        .host_str()
                        .is_some_and(|host| TRUSTED_HOSTS.contains(&host))
                {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Arc::new(Self {
            client,
            sources,
            status: Arc::new(RwLock::new(CoreUpdateStatus {
                current_version: release_version().into(),
                ..Default::default()
            })),
            update_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Builds the checker and starts the background clock. Entry point used by
    /// `main.rs`; `_plugins` is kept so the call sites read as before.
    pub(crate) fn runtime(_plugins: Arc<crate::plugins::PluginManager>) -> Arc<Self> {
        let updater = Self::new(Sources::production());
        let background = updater.clone();
        if !cfg!(test) {
            tokio::spawn(async move {
                loop {
                    if let Err(error) = background.detect().await {
                        tracing::debug!(error = %error, "automatic check of the pom update failed");
                    }
                    tokio::time::sleep(AUTO_CHECK_INTERVAL).await;
                }
            });
        }
        updater
    }

    pub(crate) async fn status(&self) -> CoreUpdateStatus {
        self.status.read().await.clone()
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status().as_u16()));
        }
        let bytes = response.bytes().await.map_err(|error| error.to_string())?;
        if bytes.len() > MAX_LISTING_BYTES {
            return Err("listing too large".into());
        }
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())
    }

    /// GitHub first, then the site; the first that answers decides.
    async fn find(&self, target: &Target) -> Result<(Option<Candidate>, &'static str), String> {
        let mut errors = Vec::new();
        if let Some(api) = &self.sources.github_api {
            let url = format!("{api}/repos/{GITHUB_REPOSITORY}/releases?per_page=30");
            match self.get_json::<Vec<GithubRelease>>(&url).await {
                Ok(releases) => {
                    let found = newest_after(release_version(), github_candidates(releases, target));
                    return Ok((found, "github"));
                }
                Err(error) => errors.push(format!("GitHub: {error}")),
            }
        }
        if let Some(site) = &self.sources.site {
            match self
                .get_json::<SiteListing>(&format!("{site}/v1/pom/latest"))
                .await
            {
                Ok(listing) => {
                    let found = newest_after(release_version(), site_candidates(listing, target));
                    return Ok((found, "site"));
                }
                Err(error) => errors.push(format!("site: {error}")),
            }
        }
        if errors.is_empty() {
            errors.push("no release source is configured".into());
        }
        Err(errors.join("; "))
    }

    /// Looks for a newer release and publishes it in the status; nothing is
    /// downloaded here.
    pub(crate) async fn detect(&self) -> Result<(), String> {
        let _guard = self.update_lock.lock().await;
        let install = Install::detect();
        let target = Target::current(&install);
        let checked_at = Some(chrono::Utc::now().to_rfc3339());
        match self.find(&target).await {
            Ok((found, source)) => {
                let update = found.map(|candidate| {
                    let blocked = if install == Install::Container {
                        Some("container".to_owned())
                    } else if candidate.download_url.is_none() {
                        Some("invitation_required".to_owned())
                    } else {
                        None
                    };
                    CoreUpdate {
                        release_id: candidate.tag.clone(),
                        version: candidate.version.to_string(),
                        changelog: candidate.changelog,
                        published_at: candidate.published_at,
                        source: source.into(),
                        asset: candidate.asset,
                        size: candidate.size,
                        installable: blocked.is_none(),
                        blocked,
                        download_url: candidate.download_url,
                        sha256: candidate.sha256,
                    }
                });
                if let Some(update) = &update {
                    tracing::info!(release = %update.release_id, source = %update.source, "new pom release found");
                }
                *self.status.write().await = CoreUpdateStatus {
                    core_update: update,
                    current_version: release_version().into(),
                    checked_at,
                    check_error: None,
                };
                Ok(())
            }
            Err(error) => {
                let mut status = self.status.write().await;
                status.checked_at = checked_at;
                status.check_error = Some(error.clone());
                Err(error)
            }
        }
    }

    /// Installs the release the operator confirmed, then hands over to the
    /// plugin host and exits. Checks again first so a stale screen never
    /// installs something other than what the sources publish now.
    pub(crate) async fn apply(&self, requested_release_id: &str) -> Result<(), String> {
        self.detect().await?;
        let _guard = self.update_lock.lock().await;
        let update = self
            .status
            .read()
            .await
            .core_update
            .clone()
            .filter(|update| update.release_id == requested_release_id.trim())
            .ok_or("the confirmed release is no longer the available update")?;
        if !update.installable {
            return Err(match update.blocked.as_deref() {
                Some("container") => {
                    "this node runs in a container: rebuild its image to update".into()
                }
                _ => "this release cannot be downloaded by the node yet".into(),
            });
        }
        let install = Install::detect();
        let staging = staging_dir(&install)?;
        let _ = tokio::fs::remove_dir_all(&staging).await;
        tokio::fs::create_dir_all(&staging).await.map_err(|error| {
            format!(
                "cannot write next to the installation ({}): {error}",
                staging.display()
            )
        })?;
        let archive = staging.join(&update.asset);
        self.download(&update, &archive).await?;
        let plan = prepare(&install, &archive, &staging).await?;
        launch_updater(&staging, plan)?;
        tracing::info!(release = %update.release_id, "pom update staged; restarting through pom-plugin-host");
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            std::process::exit(0);
        });
        Ok(())
    }

    async fn download(&self, update: &CoreUpdate, destination: &Path) -> Result<(), String> {
        let url = update
            .download_url
            .as_deref()
            .ok_or("no download address")?;
        let parsed = reqwest::Url::parse(url).map_err(|_| "invalid download address")?;
        if parsed.scheme() != "https"
            || !parsed
                .host_str()
                .is_some_and(|host| TRUSTED_HOSTS.contains(&host))
        {
            return Err("the download address is not a trusted POM host".into());
        }
        if update.size == 0 || update.size > MAX_ASSET_BYTES {
            return Err("the release asset has an invalid size".into());
        }
        let mut response = self
            .client
            .get(parsed)
            .header(reqwest::header::ACCEPT, "application/octet-stream")
            .send()
            .await
            .map_err(|error| format!("download failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "download failed: HTTP {}",
                response.status().as_u16()
            ));
        }
        // The site tells the digest in a header when GitHub's listing did not.
        let expected = update.sha256.clone().or_else(|| {
            response
                .headers()
                .get("x-content-sha256")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        });
        let partial = destination.with_extension("part");
        let mut file = tokio::fs::File::create(&partial)
            .await
            .map_err(|error| error.to_string())?;
        let mut hasher = sha2::Sha256::new();
        let mut written = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("download failed: {error}"))?
        {
            written += chunk.len() as u64;
            if written > update.size {
                return Err("the download is larger than the release says".into());
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
        }
        file.flush().await.map_err(|error| error.to_string())?;
        drop(file);
        if written != update.size {
            return Err("the download is incomplete".into());
        }
        if let Some(expected) = expected {
            if hex::encode(hasher.finalize()) != expected.to_ascii_lowercase() {
                return Err("the download does not match the release's SHA-256".into());
            }
        }
        tokio::fs::rename(&partial, destination)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Next to what is replaced, so the swap is a rename on the same disk.
fn staging_dir(install: &Install) -> Result<PathBuf, String> {
    let base = match install {
        Install::Container => return Err("this node runs in a container".into()),
        Install::AppImage(image) => image.parent().map(Path::to_path_buf),
        Install::MacBundle(bundle) => bundle.parent().map(Path::to_path_buf),
        Install::Directory(directory) => Some(directory.clone()),
    }
    .ok_or("the installation folder is unknown")?;
    Ok(base.join(".pom-update"))
}

#[derive(Serialize)]
struct Plan {
    wait_pid: u32,
    replacements: Vec<Replacement>,
    backup_dir: PathBuf,
    relaunch: Relaunch,
    log: PathBuf,
}

#[derive(Serialize)]
struct Replacement {
    from: PathBuf,
    to: PathBuf,
}

#[derive(Serialize)]
struct Relaunch {
    program: PathBuf,
    args: Vec<String>,
    cwd: Option<PathBuf>,
}

async fn run_tool(program: &str, args: &[&std::ffi::OsStr]) -> Result<(), String> {
    let output = crate::console::hidden_async(&mut tokio::process::Command::new(program))
        .args(args)
        .output()
        .await
        .map_err(|error| format!("{program}: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Unpacks the asset and says which staged entry replaces which installed one.
async fn prepare(install: &Install, archive: &Path, staging: &Path) -> Result<Plan, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let executable_name = executable
        .file_name()
        .map(std::ffi::OsStr::to_owned)
        .ok_or("the executable has no name")?;
    let (replacements, program) = match install {
        Install::Container => return Err("this node runs in a container".into()),
        Install::AppImage(image) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(archive, std::fs::Permissions::from_mode(0o755))
                    .map_err(|error| error.to_string())?;
            }
            (
                vec![Replacement {
                    from: archive.to_path_buf(),
                    to: image.clone(),
                }],
                image.clone(),
            )
        }
        Install::MacBundle(bundle) => {
            let mount = staging.join("mount");
            std::fs::create_dir_all(&mount).map_err(|error| error.to_string())?;
            run_tool(
                "hdiutil",
                &[
                    "attach".as_ref(),
                    "-nobrowse".as_ref(),
                    "-readonly".as_ref(),
                    "-mountpoint".as_ref(),
                    mount.as_os_str(),
                    archive.as_os_str(),
                ],
            )
            .await?;
            let app = std::fs::read_dir(&mount)
                .map_err(|error| error.to_string())?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| path.extension().is_some_and(|extension| extension == "app"));
            let staged = staging.join("POM.app");
            let copied = match &app {
                Some(app) => run_tool("ditto", &[app.as_os_str(), staged.as_os_str()]).await,
                None => Err("the disk image has no application".into()),
            };
            let _ = run_tool("hdiutil", &["detach".as_ref(), mount.as_os_str()]).await;
            copied?;
            (
                vec![Replacement {
                    from: staged,
                    to: bundle.clone(),
                }],
                bundle.join("Contents/MacOS").join(&executable_name),
            )
        }
        Install::Directory(directory) => {
            let unpacked = staging.join("unpacked");
            std::fs::create_dir_all(&unpacked).map_err(|error| error.to_string())?;
            // bsdtar, also shipped with Windows 10 and later, reads both a
            // .tar.gz and a .zip.
            run_tool(
                "tar",
                &[
                    "-xf".as_ref(),
                    archive.as_os_str(),
                    "-C".as_ref(),
                    unpacked.as_os_str(),
                ],
            )
            .await?;
            let package = single_child_dir(&unpacked).unwrap_or(unpacked);
            if !package.join(&executable_name).is_file() {
                return Err("the release package does not carry this program".into());
            }
            let replacements = std::fs::read_dir(&package)
                .map_err(|error| error.to_string())?
                .filter_map(Result::ok)
                .map(|entry| Replacement {
                    to: directory.join(entry.file_name()),
                    from: entry.path(),
                })
                .collect();
            (replacements, directory.join(&executable_name))
        }
    };
    Ok(Plan {
        wait_pid: std::process::id(),
        replacements,
        backup_dir: staging.join("backup"),
        relaunch: Relaunch {
            program,
            args: crate::respawn::original_argv(),
            cwd: std::env::current_dir().ok(),
        },
        log: crate::config::project_root().join("pom.log"),
    })
}

/// An archive that unpacks into one folder is that folder's contents.
fn single_child_dir(directory: &Path) -> Option<PathBuf> {
    let mut entries = std::fs::read_dir(directory).ok()?.filter_map(Result::ok);
    let only = entries.next()?;
    (entries.next().is_none() && only.path().is_dir()).then(|| only.path())
}

/// Starts a copy of `pom-plugin-host` from the staging folder (so it may
/// replace the installed one too), detached from this process.
fn launch_updater(staging: &Path, plan: Plan) -> Result<(), String> {
    let host = crate::plugin_host::host_executable().map_err(|error| error.to_string())?;
    let updater = staging.join(host.file_name().ok_or("the plugin host has no name")?);
    std::fs::copy(&host, &updater).map_err(|error| format!("copy the plugin host: {error}"))?;
    let plan_path = staging.join("plan.json");
    std::fs::write(
        &plan_path,
        serde_json::to_vec_pretty(&plan).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let mut command = std::process::Command::new(&updater);
    command
        .arg("--apply-update")
        .arg(&plan_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("start the plugin host: {error}"))
}

// ---------------------------------------------------------------------------
// HTTP routes
// ---------------------------------------------------------------------------

fn error_response(status: axum::http::StatusCode, error: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    (status, axum::Json(serde_json::json!({ "error": error }))).into_response()
}

pub(crate) async fn pom_update_status(
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    axum::Json(state.pom_core_update.status().await).into_response()
}

pub(crate) async fn check_pom_update(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    // A check that reaches no source still answers: the status says why.
    let _ = state.pom_core_update.detect().await;
    axum::Json(state.pom_core_update.status().await).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct CoreUpdateApplyRequest {
    pub release_id: String,
}

pub(crate) async fn apply_pom_update(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::Json(request): axum::Json<CoreUpdateApplyRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match state.pom_core_update.apply(&request.release_id).await {
        Ok(()) => axum::Json(serde_json::json!({
            "restarting": true,
            "release_id": request.release_id,
        }))
        .into_response(),
        Err(error) => error_response(axum::http::StatusCode::CONFLICT, error),
    }
}

#[cfg(test)]
mod release_version_tests {
    use super::release_version_from;

    #[test]
    fn the_image_release_overrides_the_compiled_fallback() {
        assert_eq!(release_version_from(Some(" v0.5.7 "), "v0.1.0"), "v0.5.7");
        assert_eq!(release_version_from(Some(""), "v0.5.6"), "v0.5.6");
        assert_eq!(release_version_from(None, "v0.5.6"), "v0.5.6");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX_CUDA: Target = Target {
        os: "linux",
        arch: "x86_64",
        variant: Some("cuda"),
        vulkan: false,
        appimage: false,
    };
    const APPIMAGE: Target = Target {
        os: "linux",
        arch: "x86_64",
        variant: Some("cuda"),
        vulkan: true,
        appimage: true,
    };
    const APPIMAGE_CUDA: Target = Target {
        os: "linux",
        arch: "x86_64",
        variant: Some("cuda"),
        vulkan: false,
        appimage: true,
    };
    const APPIMAGE_CPU: Target = Target {
        os: "linux",
        arch: "x86_64",
        variant: Some("cpu"),
        vulkan: false,
        appimage: true,
    };
    const WINDOWS_GPU: Target = Target {
        os: "windows",
        arch: "x86_64",
        variant: Some("cuda-vulkan"),
        vulkan: true,
        appimage: false,
    };
    const WINDOWS_CPU: Target = Target {
        os: "windows",
        arch: "x86_64",
        variant: None,
        vulkan: false,
        appimage: false,
    };
    const MACOS: Target = Target {
        os: "macos",
        arch: "aarch64",
        variant: None,
        vulkan: false,
        appimage: false,
    };

    #[test]
    fn each_build_recognizes_only_its_own_asset() {
        assert!(LINUX_CUDA.matches("pom-v0.5.3-x86_64-unknown-linux-gnu-cuda.tar.gz"));
        assert!(!LINUX_CUDA.matches("pom-v0.5.3-x86_64-unknown-linux-gnu-cpu.tar.gz"));
        assert!(!LINUX_CUDA.matches("pom-v0.5.5-x86_64.AppImage"));
        assert!(APPIMAGE.matches("pom-v0.5.5-x86_64.AppImage"));
        assert!(!APPIMAGE.matches("pom-v0.5.3-x86_64-unknown-linux-gnu-cuda.tar.gz"));
        assert!(!APPIMAGE.matches("pom-v0.6.0-x86_64-unknown-linux-gnu-cuda.AppImage"));
        assert!(APPIMAGE_CUDA.matches("pom-v0.6.0-x86_64-unknown-linux-gnu-cuda.AppImage"));
        assert!(!APPIMAGE_CUDA.matches("pom-v0.6.0-x86_64-unknown-linux-gnu-cpu.AppImage"));
        assert!(!APPIMAGE_CUDA.matches("pom-v0.6.0-x86_64.AppImage"));
        assert!(!APPIMAGE_CUDA.matches("pom-v0.6.0-x86_64-unknown-linux-gnu-cuda.tar.gz"));
        assert!(APPIMAGE_CPU.matches("pom-v0.6.0-x86_64-unknown-linux-gnu-cpu.AppImage"));
        assert!(!APPIMAGE_CPU.matches("pom-v0.6.0-x86_64-unknown-linux-gnu-cuda.AppImage"));
        assert!(!LINUX_CUDA.matches("pom-v0.6.0-x86_64-unknown-linux-gnu-cuda.AppImage"));
        assert!(WINDOWS_GPU.matches("pom-v0.5.6-x86_64-pc-windows-msvc-cuda-vulkan.zip"));
        assert!(!WINDOWS_GPU.matches("pom-v0.5.6-x86_64-pc-windows-msvc.zip"));
        assert!(WINDOWS_CPU.matches("pom-v0.5.6-x86_64-pc-windows-msvc.zip"));
        assert!(!WINDOWS_CPU.matches("pom-v0.5.6-x86_64-pc-windows-msvc-cuda-vulkan.zip"));
        assert!(MACOS.matches("pom-v0.5.3-aarch64-apple-darwin.dmg"));
        assert!(!MACOS.matches("pom-v-debug-quic-x86_64-pc-windows-msvc.zip"));
    }

    fn github(tag: &str, assets: &[&str]) -> GithubRelease {
        GithubRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            body: Some(format!("notes {tag}")),
            published_at: None,
            assets: assets
                .iter()
                .map(|name| GithubAsset {
                    name: (*name).into(),
                    size: 10,
                    browser_download_url: Some(format!("https://github.com/{name}")),
                    digest: Some("sha256:ab".into()),
                })
                .collect(),
        }
    }

    #[test]
    fn the_newest_release_carrying_this_platform_wins_over_a_newer_one_without_it() {
        let releases = vec![
            github(
                "v0.5.6",
                &["pom-v0.5.6-x86_64-pc-windows-msvc-cuda-vulkan.zip"],
            ),
            github("v0.5.5", &["pom-v0.5.5-x86_64.AppImage"]),
            github(
                "v0.5.3",
                &["pom-v0.5.3-x86_64-unknown-linux-gnu-cuda.tar.gz"],
            ),
        ];
        let found = newest_after("v0.5.2", github_candidates(releases, &APPIMAGE)).unwrap();
        assert_eq!(found.tag, "v0.5.5");
        assert_eq!(found.sha256.as_deref(), Some("ab"));
        assert_eq!(found.changelog.as_deref(), Some("notes v0.5.5"));
    }

    #[test]
    fn the_running_release_and_older_ones_are_not_updates() {
        let releases = vec![github(
            "v0.5.3",
            &["pom-v0.5.3-x86_64-unknown-linux-gnu-cuda.tar.gz"],
        )];
        assert!(newest_after("v0.5.3", github_candidates(releases, &LINUX_CUDA)).is_none());
        let releases = vec![github(
            "v0.5.3",
            &["pom-v0.5.3-x86_64-unknown-linux-gnu-cuda.tar.gz"],
        )];
        assert!(newest_after("v0.6.0", github_candidates(releases, &LINUX_CUDA)).is_none());
    }

    #[test]
    fn drafts_prereleases_and_bad_tags_are_skipped() {
        let mut draft = github(
            "v0.9.0",
            &["pom-v0.9.0-x86_64-unknown-linux-gnu-cuda.tar.gz"],
        );
        draft.draft = true;
        let mut pre = github(
            "v0.8.0",
            &["pom-v0.8.0-x86_64-unknown-linux-gnu-cuda.tar.gz"],
        );
        pre.prerelease = true;
        let odd = github(
            "v-debug",
            &["pom-v-debug-x86_64-unknown-linux-gnu-cuda.tar.gz"],
        );
        assert!(github_candidates(vec![draft, pre, odd], &LINUX_CUDA).is_empty());
    }

    #[test]
    fn the_site_listing_says_the_file_but_not_always_where_to_get_it() {
        let listing: SiteListing = serde_json::from_value(serde_json::json!({
            "available": true, "mode": "invite", "access": "required", "tag": "v0.5.6",
            "prerelease": false, "published_at": "2026-10-20T18:39:28Z",
            "assets": [
                {"name": "pom-v0.5.6-x86_64-pc-windows-msvc-cuda-vulkan.zip", "tag": "v0.5.6", "size": 6, "download_url": null},
                {"name": "pom-v0.5.5-x86_64.AppImage", "tag": "v0.5.5", "size": 7, "download_url": null}
            ]
        }))
        .unwrap();
        let found = newest_after("v0.5.0", site_candidates(listing, &APPIMAGE)).unwrap();
        assert_eq!(found.tag, "v0.5.5");
        assert!(found.download_url.is_none());
    }

    #[tokio::test]
    async fn a_check_without_any_source_reports_why_and_offers_nothing() {
        let updater = PomCoreUpdater::new(Sources {
            github_api: None,
            site: None,
        });
        assert!(updater.detect().await.is_err());
        let status = updater.status().await;
        assert!(status.core_update.is_none());
        assert_eq!(status.current_version, release_version());
        assert!(status.check_error.is_some());
    }

    #[test]
    fn a_package_that_unpacks_into_one_folder_is_that_folder() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("pom-v1-x86_64/apps")).unwrap();
        assert_eq!(
            single_child_dir(root.path()),
            Some(root.path().join("pom-v1-x86_64"))
        );
        std::fs::write(root.path().join("README.md"), "").unwrap();
        assert_eq!(single_child_dir(root.path()), None);
    }
}
