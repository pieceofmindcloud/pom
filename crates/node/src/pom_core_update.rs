//! Detecção e instalação de uma nova release do binário `pom` em si.
//!
//! Deliberadamente SEPARADO de `crate::plugins`: o `pom_core` não é um plugin
//! e não faz parte do catálogo de N plugins independentes decidido em
//! 2026-09-18 (ver `crate::plugins`, cabeçalho do arquivo). Ele vai virar
//! GitHub Releases público, sem licença nenhuma - trabalho de outra tarefa
//! (`rdma-pom-core-github-releases`). Até essa migração, o mecanismo aqui
//! continua exatamente como antes: usa o manifest genérico
//! `/v1/plugins/:plugin_code/manifest` com `crate::core_update::CORE_PLUGIN_CODE`,
//! autenticado emprestando o lease do plugin [`crate::plugins::ENTERPRISE_PLUGIN_CODE`]
//! (era a única licença que existia quando isto foi escrito). Essa dependência é
//! transicional por natureza - a tarefa irmã substitui a autenticação inteira -
//! e por isso este arquivo não tenta desenhar uma fonte de licença mais genérica
//! para ela.
//!
//! A tela de Plugins e a lógica de atualização de plugins (`crate::plugins`,
//! por exemplo `PluginRuntime::detect_update`/`apply_update`) não devem
//! importar nada daqui, e vice-versa: são dois relógios diferentes, dois
//! `Mutex` diferentes, e a UI mostra os dois em lugares diferentes (ver
//! `apps/frontend/src/routes/About.tsx` para o `pom_core` e
//! `apps/frontend/src/routes/Plugins.tsx` para os plugins).

use crate::plugins::{LicenseClientError, LicenseService, PluginManager, ENTERPRISE_PLUGIN_CODE};
use enterprise_protocol::{Fingerprint, LicenseErrorCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Intervalo da checagem automática em segundo plano. Antes desta mudança a
/// checagem piggybackava no heartbeat da licença Enterprise (a cada poucas
/// horas, conforme o servidor); agora que este relógio é próprio e
/// desacoplado do heartbeat de qualquer plugin, um intervalo fixo é a escolha
/// mais simples - o valor exato não é contrato de produto, só a cadência de
/// um aviso em segundo plano. A checagem sob demanda (`POST
/// /api/ui/pom-update/check`) continua imediata.
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

    /// Constrói o checador e liga o relógio de fundo. Ponto de entrada usado
    /// pelo `main.rs`.
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
                    tracing::warn!(error = %error, "detecção automática de atualização do pom falhou");
                }
            }
        });
    }

    pub(crate) async fn status(&self) -> Option<CoreUpdate> {
        self.status.read().await.clone()
    }

    fn enterprise_lease_error() -> LicenseClientError {
        LicenseClientError::new(
            LicenseErrorCode::LeaseUnknown,
            "lease do plugin enterprise ausente para atualização do pom",
        )
    }

    /// Procura uma release do próprio binário somente quando o plugin
    /// enterprise está ativo (é dele que este mecanismo transicional empresta
    /// o lease). A detecção apenas publica o aviso no status; download, troca
    /// e reinício ficam reservados à ação explícita em `apply`.
    pub(crate) async fn detect(&self) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        let Some(enterprise) = self.plugins.runtime_for(ENTERPRISE_PLUGIN_CODE) else {
            return Ok(());
        };
        if !enterprise.status().await.active {
            *self.status.write().await = None;
            return Ok(());
        }
        let persisted = enterprise
            .persisted_state()
            .await
            .ok_or_else(Self::enterprise_lease_error)?;
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
            crate::BUILD_COMMIT,
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
                "release nova do pom encontrada"
            );
        }
        *self.status.write().await = available;
        Ok(())
    }

    /// Baixa e instala somente a release que o operador confirmou na tela.
    /// Reconsulta o manifest antes do download para não reutilizar uma
    /// concessão ou metadados antigos de uma detecção anterior.
    pub(crate) async fn apply(&self, requested_release_id: &str) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        let requested_release_id = requested_release_id.trim();
        let Some(available) = self.status.read().await.clone() else {
            return Err(update_unavailable(
                "nenhuma atualização do pom está disponível",
            ));
        };
        if available.release_id != requested_release_id {
            return Err(update_unavailable(
                "a atualização confirmada já não é a atualização disponível",
            ));
        }
        let Some(enterprise) = self.plugins.runtime_for(ENTERPRISE_PLUGIN_CODE) else {
            return Err(Self::enterprise_lease_error());
        };
        if !enterprise.status().await.active {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "uma licença do plugin enterprise ativa é necessária para atualizar o pom",
            ));
        }
        let persisted = enterprise
            .persisted_state()
            .await
            .ok_or_else(Self::enterprise_lease_error)?;
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
                crate::BUILD_COMMIT,
                &manifest.manifest,
            )
        {
            return Err(update_unavailable(
                "a atualização confirmada já não é uma release nova disponível",
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
                    "binário pom trocado; reinício autorizado pelo operador"
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
                    "binário pom preparado; reinício autorizado pelo operador"
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
        tracing::error!(error = %error, "reinício autorizado do pom falhou; o processo segue no ar");
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
// Rotas HTTP
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
            plugin_code: ENTERPRISE_PLUGIN_CODE.into(),
            activation_id: "act-1".into(),
            activation_token: "pat-test".into(),
            lease_id: "lease-1".into(),
            lease_token: "plt-test".into(),
            licensed_plugins: vec![ENTERPRISE_PLUGIN_CODE.into()],
            license_expires_at: Some("2099-12-31T00:00:00Z".into()),
            activation_count: Some(1),
            max_activations: Some(5),
            plugin_usage: vec![enterprise_protocol::PluginUsage {
                plugin_code: ENTERPRISE_PLUGIN_CODE.into(),
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
                feature_set: vec!["enterprise.request_logs".into()],
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
            key_id: "release-key".into(),
            signature: "signed-manifest".into(),
        }
    }

    #[tokio::test]
    async fn without_an_active_enterprise_plugin_no_core_manifest_is_checked() {
        let service = Arc::new(MockLicenseService::new());
        let manager = PluginManager::test_manager(&["enterprise"], service.clone());
        let updater = PomCoreUpdater::new(manager);

        updater.detect().await.expect("skip without active plugin");

        assert!(updater.status().await.is_none());
        assert!(service.manifest_calls().await.is_empty());
    }

    #[tokio::test]
    async fn active_enterprise_plugin_advertises_a_core_release_without_downloading() {
        let artifact = b"new-pom".to_vec();
        let service = Arc::new(MockLicenseService::single(
            ENTERPRISE_PLUGIN_CODE,
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
        let manager = PluginManager::test_manager(&["enterprise"], service.clone());
        let enterprise = manager
            .runtime_for(ENTERPRISE_PLUGIN_CODE)
            .expect("runtime");
        enterprise
            .activate("PM-test-key")
            .await
            .expect("activate enterprise");
        enterprise
            .mark_plugin_ready("0.1.0")
            .await
            .expect("mark ready");
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
            ENTERPRISE_PLUGIN_CODE,
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
        let manager = PluginManager::test_manager(&["enterprise"], service.clone());
        let enterprise = manager
            .runtime_for(ENTERPRISE_PLUGIN_CODE)
            .expect("runtime");
        enterprise
            .activate("PM-test-key")
            .await
            .expect("activate enterprise");
        enterprise
            .mark_plugin_ready("0.1.0")
            .await
            .expect("mark ready");
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
