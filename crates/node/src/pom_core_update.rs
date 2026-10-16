//! Detection and installation of a new release of the `pom` binary itself.
//!
//! Deliberately SEPARATE from `crate::plugins`: `pom_core` is not a plugin and is
//! not part of the catalog of N independent plugins decided on 2026-09-18 (see
//! `crate::plugins`, file header). It will become public GitHub Releases, with no
//! license at all - the work of another task (`rdma-pom-core-github-releases`).
//! Until that migration, the mechanism here stays exactly as before: it uses the
//! generic manifest `/v1/plugins/:plugin_code/manifest` with
//! `crate::core_update::CORE_PLUGIN_CODE`, authenticated by borrowing the lease
//! of the plugin [`crate::plugins::LOGS_PLUGIN_CODE`] (the only license that
//! existed when this was written). That dependency is transitional by nature -
//! the sibling task replaces the whole authentication - and for that reason this
//! file does not try to design a more generic license source for it.
//!
//! The Plugins screen and the plugin update logic (`crate::plugins`, for example
//! `PluginRuntime::detect_update`/`apply_update`) must not import anything from
//! here, and vice versa: they are two different clocks, two different `Mutex`es,
//! and the UI shows the two in different places (see
//! `apps/frontend/src/routes/About.tsx` for `pom_core` and
//! `apps/frontend/src/routes/Plugins.tsx` for the plugins).

use crate::plugins::{LicenseClientError, LicenseService, PluginManager, LOGS_PLUGIN_CODE};
use enterprise_protocol::{Fingerprint, LicenseErrorCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Interval of the automatic background check. Before this change the check
/// piggybacked on the Enterprise license heartbeat (every few hours, as the
/// server decided); now that this clock is its own and decoupled from any
/// plugin's heartbeat, a fixed interval is the simplest choice - the exact
/// value is not a product contract, only the cadence of a background notice.
/// The on-demand check (`POST /api/ui/pom-update/check`) stays immediate.
const AUTO_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CoreUpdate {
    pub release_id: String,
    pub version: String,
    pub changelog: Option<String>,
}

pub(crate) struct PomCoreUpdater {
    plugins: Arc<PluginManager>,
    service: Arc<dyn LicenseService>,
    fingerprint: Fingerprint,
    status: Arc<RwLock<Option<CoreUpdate>>>,
    update_lock: Arc<Mutex<()>>,
}

impl PomCoreUpdater {
    fn new(plugins: Arc<PluginManager>) -> Arc<Self> {
        let service = plugins.service();
        let fingerprint = plugins.fingerprint().clone();
        Arc::new(Self {
            plugins,
            service,
            fingerprint,
            status: Arc::new(RwLock::new(None)),
            update_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Builds the checker and starts the background clock. Entry point used by
    /// `main.rs`.
    pub(crate) fn runtime(plugins: Arc<PluginManager>) -> Arc<Self> {
        let updater = Self::new(plugins);
        updater.spawn_supervisor();
        updater
    }

    fn spawn_supervisor(self: &Arc<Self>) {
        let updater = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(AUTO_CHECK_INTERVAL).await;
                if let Err(error) = updater.detect().await {
                    tracing::warn!(error = %error, "automatic detection of the pom update failed");
                }
            }
        });
    }

    pub(crate) async fn status(&self) -> Option<CoreUpdate> {
        self.status.read().await.clone()
    }

    fn logs_lease_error() -> LicenseClientError {
        LicenseClientError::new(
            LicenseErrorCode::LeaseUnknown,
            "logs plugin lease missing for the pom update",
        )
    }

    /// Looks for a release of the binary itself only when the logs plugin is
    /// active (it is the one this transitional mechanism borrows the lease
    /// from). Detection only publishes the notice in the status; download, swap
    /// and restart are reserved for the explicit action in `apply`.
    pub(crate) async fn detect(&self) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        let Some(logs) = self.plugins.runtime_for(LOGS_PLUGIN_CODE) else {
            return Ok(());
        };
        if !logs.status().await.active {
            *self.status.write().await = None;
            return Ok(());
        }
        let persisted = logs
            .persisted_state()
            .await
            .ok_or_else(Self::logs_lease_error)?;
        let manifest = self
            .service
            .manifest(
                crate::core_update::CORE_PLUGIN_CODE,
                &persisted.lease_token,
                &persisted.lease_id,
                &self.fingerprint.platform,
                &self.fingerprint.arch,
            )
            .await?;
        crate::core_update::validate_manifest(&manifest.manifest, env!("CARGO_PKG_VERSION"))
            .map_err(core_update_error)?;
        let available = crate::core_update::is_newer_release(
            env!("CARGO_PKG_VERSION"),
            &crate::build_commit(),
            &manifest.manifest,
        )
        .then(|| CoreUpdate {
            release_id: manifest.manifest.release_id.clone(),
            version: manifest.manifest.version.clone(),
            changelog: manifest.manifest.changelog.clone(),
        });
        if let Some(update) = &available {
            tracing::info!(
                release_id = %update.release_id,
                version = %update.version,
                "new pom release found"
            );
        }
        *self.status.write().await = available;
        Ok(())
    }

    /// Downloads and installs only the release the operator confirmed on the
    /// screen. Re-queries the manifest before the download so as not to reuse a
    /// grant or stale metadata from an earlier detection.
    pub(crate) async fn apply(&self, requested_release_id: &str) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        let requested_release_id = requested_release_id.trim();
        let Some(available) = self.status.read().await.clone() else {
            return Err(update_unavailable("no pom update is available"));
        };
        if available.release_id != requested_release_id {
            return Err(update_unavailable(
                "the confirmed update is no longer the available update",
            ));
        }
        let Some(logs) = self.plugins.runtime_for(LOGS_PLUGIN_CODE) else {
            return Err(Self::logs_lease_error());
        };
        if !logs.status().await.active {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "an active logs plugin license is required to update pom",
            ));
        }
        let persisted = logs
            .persisted_state()
            .await
            .ok_or_else(Self::logs_lease_error)?;
        let manifest = self
            .service
            .manifest(
                crate::core_update::CORE_PLUGIN_CODE,
                &persisted.lease_token,
                &persisted.lease_id,
                &self.fingerprint.platform,
                &self.fingerprint.arch,
            )
            .await?;
        crate::core_update::validate_manifest(&manifest.manifest, env!("CARGO_PKG_VERSION"))
            .map_err(core_update_error)?;
        if manifest.manifest.release_id != requested_release_id
            || !crate::core_update::is_newer_release(
                env!("CARGO_PKG_VERSION"),
                &crate::build_commit(),
                &manifest.manifest,
            )
        {
            return Err(update_unavailable(
                "the confirmed update is no longer a new available release",
            ));
        }

        let bytes = self.service.download(&manifest.download).await?;
        crate::core_update::validate_download(&manifest.manifest, &bytes)
            .map_err(core_update_error)?;
        let executable = std::env::current_exe().map_err(io_error)?;
        match crate::core_update::install_atomically(&executable, &bytes).map_err(io_error)? {
            crate::core_update::InstallResult::Replaced => {
                tracing::info!(
                    release_id = %requested_release_id,
                    "pom binary swapped; restart authorized by the operator"
                );
                schedule_core_restart(executable);
                Ok(())
            }
            #[cfg(windows)]
            crate::core_update::InstallResult::Staged(staged) => {
                crate::core_update::launch_staged_update(
                    &staged,
                    &executable,
                    &crate::respawn::original_argv(),
                )
                .map_err(io_error)?;
                tracing::info!(
                    staged = %staged.display(),
                    release_id = %requested_release_id,
                    "pom binary prepared; restart authorized by the operator"
                );
                schedule_core_restart(executable);
                Ok(())
            }
        }
    }
}

fn core_update_error(message: String) -> LicenseClientError {
    LicenseClientError::new(LicenseErrorCode::ServerError, message)
}

fn update_unavailable(message: impl Into<String>) -> LicenseClientError {
    LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, message)
}

fn io_error(error: std::io::Error) -> LicenseClientError {
    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
}

#[cfg(not(windows))]
fn schedule_core_restart(executable: std::path::PathBuf) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let error = crate::respawn::respawn_at(&executable);
        tracing::error!(error = %error, "authorized pom restart failed; the process stays up");
    });
}

#[cfg(windows)]
fn schedule_core_restart(_executable: std::path::PathBuf) {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        std::process::exit(0);
    });
}

// ---------------------------------------------------------------------------
// HTTP routes
// ---------------------------------------------------------------------------

pub(crate) async fn pom_update_status(
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    axum::Json(serde_json::json!({ "core_update": state.pom_core_update.status().await }))
        .into_response()
}

pub(crate) async fn check_pom_update(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match state.pom_core_update.detect().await {
        Ok(()) => {
            axum::Json(serde_json::json!({ "core_update": state.pom_core_update.status().await }))
                .into_response()
        }
        Err(error) => crate::plugins::license_error_response(error),
    }
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
        Err(error) => crate::plugins::license_error_response(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::tests_support::MockLicenseService;
    use enterprise_protocol::{ActivationResponse, Grant, PluginManifest};
    use sha2::Digest;

    fn activation(release_id: &str) -> ActivationResponse {
        ActivationResponse {
            license_id: "lic-1".into(),
            plugin_code: LOGS_PLUGIN_CODE.into(),
            activation_id: "act-1".into(),
            activation_token: "pat-test".into(),
            lease_id: "lease-1".into(),
            lease_token: "plt-test".into(),
            licensed_plugins: vec![LOGS_PLUGIN_CODE.into()],
            license_expires_at: Some("2099-12-31T00:00:00Z".into()),
            activation_count: Some(1),
            max_activations: Some(5),
            plugin_usage: vec![enterprise_protocol::PluginUsage {
                plugin_code: LOGS_PLUGIN_CODE.into(),
                activation_count: 1,
                max_activations: 5,
            }],
            activation_candidates: Vec::new(),
            lease_expires_at: "2099-01-01T00:00:00Z".into(),
            heartbeat_due_at: "2099-01-01T00:00:00Z".into(),
            grace_seconds: 172800,
            plugin_release_id: release_id.into(),
            plugin_manifest: PluginManifest {
                schema: 1,
                release_id: release_id.into(),
                product: "pom".into(),
                version: "0.1.0".into(),
                channel: "stable".into(),
                feature_set: vec!["logs.request_logs".into()],
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                min_host_version: "0.1.0".into(),
                plugin_abi: 1,
                sha256: format!(
                    "sha256:{}",
                    hex::encode(sha2::Sha256::digest(b"plugin-binary"))
                ),
                size: b"plugin-binary".len() as u64,
                download_expires_at: None,
                changelog: None,
                preferences: None,
                key_id: "test".into(),
                signature: "test-signature".into(),
            },
            grant: Grant {
                alg: "Ed25519".into(),
                key_id: "test".into(),
                body: "grant-body".into(),
                signature: "grant-signature".into(),
            },
        }
    }

    fn core_manifest(artifact: &[u8]) -> PluginManifest {
        PluginManifest {
            schema: 1,
            release_id: "core-new-commit".into(),
            product: "pom".into(),
            version: "0.2.0".into(),
            channel: "stable".into(),
            feature_set: vec![],
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            min_host_version: env!("CARGO_PKG_VERSION").into(),
            plugin_abi: 1,
            sha256: format!("sha256:{}", hex::encode(sha2::Sha256::digest(artifact))),
            size: artifact.len() as u64,
            download_expires_at: None,
            changelog: None,
            preferences: None,
            key_id: "release-key".into(),
            signature: "signed-manifest".into(),
        }
    }

    #[tokio::test]
    async fn without_an_active_logs_plugin_no_core_manifest_is_checked() {
        let service = Arc::new(MockLicenseService::new());
        let manager = PluginManager::test_manager(&["logs"], service.clone());
        let updater = PomCoreUpdater::new(manager);

        updater.detect().await.expect("skip without active plugin");

        assert!(updater.status().await.is_none());
        assert!(service.manifest_calls().await.is_empty());
    }

    #[tokio::test]
    async fn active_logs_plugin_advertises_a_core_release_without_downloading() {
        let artifact = b"new-pom".to_vec();
        let service = Arc::new(MockLicenseService::single(
            LOGS_PLUGIN_CODE,
            activation("ent-1"),
        ));
        service
            .grant(
                crate::core_update::CORE_PLUGIN_CODE,
                {
                    let mut response = activation("ent-1");
                    response.plugin_code = crate::core_update::CORE_PLUGIN_CODE.into();
                    response.plugin_manifest = core_manifest(&artifact);
                    response
                },
                artifact,
            )
            .await;
        let manager = PluginManager::test_manager(&["logs"], service.clone());
        let logs = manager.runtime_for(LOGS_PLUGIN_CODE).expect("runtime");
        logs.activate("PM-test-key").await.expect("activate logs");
        logs.mark_plugin_ready("0.1.0").await.expect("mark ready");
        let updater = PomCoreUpdater::new(manager);

        updater.detect().await.expect("detect core update");

        let update = updater.status().await.expect("update should be advertised");
        assert_eq!(update.release_id, "core-new-commit");
        assert_eq!(update.version, "0.2.0");
        assert_eq!(service.download_calls().await, 0);
    }

    #[tokio::test]
    async fn applying_an_unconfirmed_release_is_rejected_without_downloading() {
        let artifact = b"new-pom".to_vec();
        let service = Arc::new(MockLicenseService::single(
            LOGS_PLUGIN_CODE,
            activation("ent-1"),
        ));
        service
            .grant(
                crate::core_update::CORE_PLUGIN_CODE,
                {
                    let mut response = activation("ent-1");
                    response.plugin_code = crate::core_update::CORE_PLUGIN_CODE.into();
                    response.plugin_manifest = core_manifest(&artifact);
                    response
                },
                artifact,
            )
            .await;
        let manager = PluginManager::test_manager(&["logs"], service.clone());
        let logs = manager.runtime_for(LOGS_PLUGIN_CODE).expect("runtime");
        logs.activate("PM-test-key").await.expect("activate logs");
        logs.mark_plugin_ready("0.1.0").await.expect("mark ready");
        let updater = PomCoreUpdater::new(manager);
        updater.detect().await.expect("detect core update");

        let error = updater
            .apply("core-not-confirmed")
            .await
            .expect_err("must require the advertised release");

        assert_eq!(error.code(), LicenseErrorCode::UpdateUnavailable);
        assert_eq!(service.download_calls().await, 0);
    }
}
