use async_trait::async_trait;
use enterprise_protocol::{
    ActivationPayload, ActivationResponse, ClientInfo, EnterpriseStatus, Fingerprint, Grant,
    HeartbeatPayload, LicenseError, LicenseErrorCode, LicenseState, RequestEnvelope,
    ResponseEnvelope,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

const REQUEST_LOGS_FEATURE: &str = "enterprise.request_logs";
const STATISTICS_FEATURE: &str = "enterprise.statistics";

#[derive(Debug, Clone)]
pub(crate) struct LicenseClientError {
    code: LicenseErrorCode,
    message: String,
}

impl LicenseClientError {
    fn new(code: LicenseErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn code(&self) -> LicenseErrorCode {
        self.code
    }
}

impl fmt::Display for LicenseClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "licensing request failed ({:?}): {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for LicenseClientError {}

#[async_trait]
pub(crate) trait LicenseService: Send + Sync {
    async fn activate(
        &self,
        request: RequestEnvelope<ActivationPayload>,
    ) -> Result<ResponseEnvelope<ActivationResponse>, LicenseClientError>;

    async fn heartbeat(
        &self,
        request: RequestEnvelope<HeartbeatPayload>,
    ) -> Result<ResponseEnvelope<()>, LicenseClientError>;
}

#[derive(Clone)]
pub(crate) struct HttpLicenseService {
    client: reqwest::Client,
    base_url: String,
}

impl HttpLicenseService {
    pub(crate) fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    fn activation_url(&self) -> String {
        format!("{}/v1/licenses/activations", self.base_url)
    }

    fn heartbeat_url(&self) -> String {
        format!("{}/v1/licenses/leases/heartbeat", self.base_url)
    }
}

#[async_trait]
impl LicenseService for HttpLicenseService {
    async fn activate(
        &self,
        request: RequestEnvelope<ActivationPayload>,
    ) -> Result<ResponseEnvelope<ActivationResponse>, LicenseClientError> {
        let response = self
            .client
            .post(self.activation_url())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })?;

        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
        })?;
        if status.is_success() {
            serde_json::from_slice(&body).map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })
        } else {
            let error =
                serde_json::from_slice::<LicenseError>(&body).unwrap_or_else(|_| LicenseError {
                    code: match status {
                        StatusCode::TOO_MANY_REQUESTS => LicenseErrorCode::RateLimited,
                        _ => LicenseErrorCode::ServerError,
                    },
                    message_for_user: "O servidor de licenças está indisponível.".into(),
                    retryable: status.is_server_error(),
                    request_id: request.request_id.clone(),
                });
            Err(LicenseClientError::new(error.code, error.message_for_user))
        }
    }

    async fn heartbeat(
        &self,
        request: RequestEnvelope<HeartbeatPayload>,
    ) -> Result<ResponseEnvelope<()>, LicenseClientError> {
        let response = self
            .client
            .post(self.heartbeat_url())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
        })?;
        if status.is_success() {
            serde_json::from_slice(&body).map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })
        } else {
            let error =
                serde_json::from_slice::<LicenseError>(&body).unwrap_or_else(|_| LicenseError {
                    code: match status {
                        StatusCode::TOO_MANY_REQUESTS => LicenseErrorCode::RateLimited,
                        _ => LicenseErrorCode::ServerError,
                    },
                    message_for_user: "O servidor de licenças está indisponível.".into(),
                    retryable: status.is_server_error(),
                    request_id: request.request_id.clone(),
                });
            Err(LicenseClientError::new(error.code, error.message_for_user))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedEnterpriseState {
    pub license_id: String,
    pub activation_id: String,
    pub lease_id: String,
    pub fingerprint: Fingerprint,
    pub signed_grant: Grant,
    pub lease_expires_at: String,
    pub plugin_release_id: String,
    pub plugin_sha256: String,
    pub plugin_path: Option<String>,
}

#[derive(Clone)]
pub(crate) struct EnterpriseState {
    service: Arc<dyn LicenseService>,
    fingerprint: Fingerprint,
    status: Arc<RwLock<EnterpriseStatus>>,
    pending: Arc<Mutex<Option<PendingActivation>>>,
    persisted: Arc<Mutex<Option<PersistedEnterpriseState>>>,
}

#[derive(Debug, Clone)]
struct PendingActivation {
    response: ActivationResponse,
    fingerprint: Fingerprint,
}

impl EnterpriseState {
    pub(crate) fn new(service: Arc<dyn LicenseService>, fingerprint: Fingerprint) -> Self {
        Self {
            service,
            fingerprint,
            status: Arc::new(RwLock::new(EnterpriseStatus::inactive())),
            pending: Arc::new(Mutex::new(None)),
            persisted: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) async fn status(&self) -> EnterpriseStatus {
        self.status.read().await.clone()
    }

    pub(crate) fn runtime(node_id: &str) -> Arc<Self> {
        let fingerprint = Fingerprint::from_components(
            std::env::consts::OS,
            std::env::consts::ARCH,
            node_id,
            &machine_component(),
            &board_component(),
            &installation_component(),
        );
        Arc::new(Self::new(
            Arc::new(HttpLicenseService::new("https://license.pieceofmind.cloud")),
            fingerprint,
        ))
    }

    pub(crate) async fn activate(&self, license_key: &str) -> Result<(), LicenseClientError> {
        if license_key.trim().is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::InvalidKey,
                "Informe uma chave de licença.",
            ));
        }

        let request_id = uuid::Uuid::new_v4().to_string();
        let idempotency_key = format!(
            "activate:{}:{}",
            stable_digest(license_key),
            self.fingerprint.hash
        );
        let client = client_info();
        let request = RequestEnvelope::new(
            request_id,
            idempotency_key,
            client,
            ActivationPayload {
                license_key: license_key.to_owned(),
                fingerprint: self.fingerprint.clone(),
                node_id: self.fingerprint.node_id.clone(),
                requested_features: vec![REQUEST_LOGS_FEATURE.into(), STATISTICS_FEATURE.into()],
            },
        );
        let response = self.service.activate(request).await?;
        if response.payload.grant.body.is_empty()
            || response.payload.grant.signature.is_empty()
            || response.payload.plugin_manifest.plugin_abi
                != enterprise_protocol::PLUGIN_ABI_VERSION
        {
            return Err(LicenseClientError::new(
                LicenseErrorCode::ServerError,
                "A resposta de ativação não contém um grant ou ABI válido.",
            ));
        }

        let response = response.payload;
        *self.pending.lock().await = Some(PendingActivation {
            response: response.clone(),
            fingerprint: self.fingerprint.clone(),
        });
        *self.persisted.lock().await = Some(PersistedEnterpriseState {
            license_id: response.license_id,
            activation_id: response.activation_id,
            lease_id: response.lease_id,
            fingerprint: self.fingerprint.clone(),
            signed_grant: response.grant,
            lease_expires_at: response.lease_expires_at,
            plugin_release_id: response.plugin_release_id,
            plugin_sha256: response.plugin_manifest.sha256,
            plugin_path: None,
        });
        let mut status = self.status.write().await;
        status.active = false;
        status.state = LicenseState::Unavailable;
        status.features = EnterpriseStatus::inactive().features;
        status.plugin_version = None;
        status.lease_expires_at = None;
        Ok(())
    }

    pub(crate) async fn mark_plugin_ready(
        &self,
        plugin_version: &str,
    ) -> Result<(), LicenseClientError> {
        let pending = self.pending.lock().await.take().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "Nenhuma ativação Enterprise aguardando o host.",
            )
        })?;
        if pending.fingerprint != self.fingerprint {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FingerprintMismatch,
                "A ativação não pertence a esta instalação.",
            ));
        }
        let features =
            EnterpriseFeaturesFromEntitlements::from(pending.response.entitlements.as_slice());
        if !features.0.request_logs && !features.0.statistics {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "A licença não inclui uma funcionalidade Enterprise.",
            ));
        }
        let mut status = self.status.write().await;
        status.active = true;
        status.state = LicenseState::Active;
        status.features = features.0;
        status.plugin_version = Some(plugin_version.to_owned());
        status.lease_expires_at = Some(pending.response.lease_expires_at);
        Ok(())
    }

    pub(crate) async fn mark_heartbeat_failure(&self, within_grace: bool) {
        let mut status = self.status.write().await;
        if !status.active {
            return;
        }
        status.state = if within_grace {
            LicenseState::Degraded
        } else {
            status.active = false;
            LicenseState::Expired
        };
    }

    pub(crate) async fn heartbeat(&self, nonce: &str) -> Result<(), LicenseClientError> {
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "Nenhum lease Enterprise local.",
            )
        })?;
        let request = RequestEnvelope::new(
            uuid::Uuid::new_v4().to_string(),
            format!("heartbeat:{}:{nonce}", persisted.lease_id),
            client_info(),
            HeartbeatPayload {
                activation_id: persisted.activation_id,
                lease_id: persisted.lease_id,
                fingerprint: self.fingerprint.clone(),
                node_id: self.fingerprint.node_id.clone(),
                nonce: nonce.to_owned(),
            },
        );
        match self.service.heartbeat(request).await {
            Ok(_) => Ok(()),
            Err(error) => {
                let mut status = self.status.write().await;
                match error.code {
                    LicenseErrorCode::Revoked | LicenseErrorCode::Suspended => {
                        status.active = false;
                        status.state = LicenseState::Revoked;
                    }
                    LicenseErrorCode::Expired => {
                        status.active = false;
                        status.state = LicenseState::Expired;
                    }
                    _ if status.active => {
                        status.state = LicenseState::Degraded;
                    }
                    _ => {}
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn persisted(&self) -> PersistedEnterpriseState {
        self.persisted
            .lock()
            .await
            .clone()
            .expect("estado persistido só existe após ativação")
    }
}

#[derive(Debug, Clone)]
struct EnterpriseFeaturesFromEntitlements(enterprise_protocol::EnterpriseFeatures);

impl From<&[String]> for EnterpriseFeaturesFromEntitlements {
    fn from(entitlements: &[String]) -> Self {
        Self(enterprise_protocol::EnterpriseFeatures {
            request_logs: entitlements
                .iter()
                .any(|feature| feature == REQUEST_LOGS_FEATURE),
            statistics: entitlements
                .iter()
                .any(|feature| feature == STATISTICS_FEATURE),
        })
    }
}

fn stable_digest(value: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{}", hex::encode(Sha256::digest(value.as_bytes())))
}

fn client_info() -> ClientInfo {
    ClientInfo {
        product: "pom".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
    }
}

fn machine_component() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(value) = std::fs::read_to_string("/etc/machine-id") {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_owned();
            }
        }
    }
    std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-machine".into())
}

fn board_component() -> String {
    std::env::var("POM_BOARD_ID").unwrap_or_else(|_| "unknown-board".into())
}

fn installation_component() -> String {
    std::env::var("POM_INSTALLATION_ID").unwrap_or_else(|_| "unknown-installation".into())
}

#[derive(Clone)]
pub(crate) struct MockLicenseService {
    response: Arc<Mutex<MockResponse>>,
    heartbeat_error: Arc<Mutex<Option<LicenseErrorCode>>>,
}

#[derive(Clone)]
enum MockResponse {
    Valid(ActivationResponse),
    Error(LicenseErrorCode),
}

impl MockLicenseService {
    pub(crate) fn valid(response: ActivationResponse) -> Self {
        Self {
            response: Arc::new(Mutex::new(MockResponse::Valid(response))),
            heartbeat_error: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn error(code: LicenseErrorCode) -> Self {
        Self {
            response: Arc::new(Mutex::new(MockResponse::Error(code))),
            heartbeat_error: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) async fn set_heartbeat_error(&self, code: LicenseErrorCode) {
        *self.heartbeat_error.lock().await = Some(code);
    }
}

#[async_trait]
impl LicenseService for MockLicenseService {
    async fn activate(
        &self,
        request: RequestEnvelope<ActivationPayload>,
    ) -> Result<ResponseEnvelope<ActivationResponse>, LicenseClientError> {
        match self.response.lock().await.clone() {
            MockResponse::Valid(payload) => Ok(ResponseEnvelope {
                protocol: enterprise_protocol::LICENSE_PROTOCOL.into(),
                request_id: request.request_id,
                server_time: "2099-01-01T00:00:00Z".into(),
                expires_at: Some(payload.lease_expires_at.clone()),
                payload,
            }),
            MockResponse::Error(code) => Err(LicenseClientError::new(code, "mock license error")),
        }
    }

    async fn heartbeat(
        &self,
        request: RequestEnvelope<HeartbeatPayload>,
    ) -> Result<ResponseEnvelope<()>, LicenseClientError> {
        if let Some(code) = *self.heartbeat_error.lock().await {
            return Err(LicenseClientError::new(code, "mock heartbeat error"));
        }
        Ok(ResponseEnvelope {
            protocol: enterprise_protocol::LICENSE_PROTOCOL.into(),
            request_id: request.request_id,
            server_time: "2099-01-01T00:00:00Z".into(),
            expires_at: None,
            payload: (),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enterprise_protocol::{
        ActivationResponse, EnterpriseFeatures, EnterpriseStatus, Fingerprint, Grant,
        LicenseErrorCode, LicenseState, PluginManifest,
    };
    use std::sync::Arc;

    fn fingerprint() -> Fingerprint {
        Fingerprint::from_components(
            "linux",
            "x86_64",
            "node-a",
            "machine",
            "board",
            "installation",
        )
    }

    fn activation() -> ActivationResponse {
        ActivationResponse {
            license_id: "lic-1".into(),
            activation_id: "act-1".into(),
            lease_id: "lease-1".into(),
            entitlements: vec![
                "enterprise.request_logs".into(),
                "enterprise.statistics".into(),
            ],
            lease_expires_at: "2099-01-01T00:00:00Z".into(),
            heartbeat_due_at: "2099-01-01T00:00:00Z".into(),
            plugin_release_id: "ent-test".into(),
            plugin_manifest: PluginManifest {
                schema: 1,
                release_id: "ent-test".into(),
                product: "pom".into(),
                feature_set: vec!["enterprise.request_logs".into()],
                os: "linux".into(),
                arch: "x86_64".into(),
                min_host_version: "0.1.0".into(),
                plugin_abi: 1,
                sha256: "sha256:test".into(),
                size: 12,
                download_expires_at: "2099-01-01T00:00:00Z".into(),
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

    #[tokio::test]
    async fn activation_requires_the_service_and_publishes_only_entitled_features() {
        let service = Arc::new(MockLicenseService::valid(activation()));
        let state = EnterpriseState::new(service, fingerprint());

        state.activate("PM-test-key").await.expect("activate");
        state
            .mark_plugin_ready("ent-test")
            .await
            .expect("plugin ready");

        let status = state.status().await;
        assert!(status.active);
        assert_eq!(status.state, LicenseState::Active);
        assert!(status.features.request_logs);
        assert!(status.features.statistics);
        assert_eq!(status.plugin_version.as_deref(), Some("ent-test"));
    }

    #[tokio::test]
    async fn invalid_license_keeps_free_mode_and_returns_a_stable_error() {
        let service = Arc::new(MockLicenseService::error(LicenseErrorCode::InvalidKey));
        let state = EnterpriseState::new(service, fingerprint());

        let error = state.activate("PM-invalid").await.expect_err("must reject");

        assert_eq!(error.code(), LicenseErrorCode::InvalidKey);
        assert_eq!(state.status().await, EnterpriseStatus::inactive());
    }

    #[tokio::test]
    async fn transient_heartbeat_failure_enters_degraded_without_disabling_free_mode() {
        let service = Arc::new(MockLicenseService::valid(activation()));
        let state = EnterpriseState::new(service, fingerprint());
        state.activate("PM-test-key").await.expect("activate");
        state
            .mark_plugin_ready("ent-test")
            .await
            .expect("plugin ready");

        state.mark_heartbeat_failure(true).await;

        let status = state.status().await;
        assert_eq!(status.state, LicenseState::Degraded);
        assert!(status.active);
        assert_eq!(
            status.features,
            EnterpriseFeatures {
                request_logs: true,
                statistics: true
            }
        );
    }

    #[tokio::test]
    async fn revoked_heartbeat_disables_enterprise_but_leaves_free_mode_running() {
        let service = Arc::new(MockLicenseService::valid(activation()));
        let state = EnterpriseState::new(service.clone(), fingerprint());
        state.activate("PM-test-key").await.expect("activate");
        state
            .mark_plugin_ready("ent-test")
            .await
            .expect("plugin ready");
        service.set_heartbeat_error(LicenseErrorCode::Revoked).await;

        let error = state.heartbeat("nonce-1").await.expect_err("revoked");

        assert_eq!(error.code(), LicenseErrorCode::Revoked);
        let status = state.status().await;
        assert!(!status.active);
        assert_eq!(status.state, LicenseState::Revoked);
    }

    #[tokio::test]
    async fn persisted_state_never_contains_the_license_key() {
        let service = Arc::new(MockLicenseService::valid(activation()));
        let state = EnterpriseState::new(service, fingerprint());
        state.activate("PM-secret-license").await.expect("activate");
        state
            .mark_plugin_ready("ent-test")
            .await
            .expect("plugin ready");

        let persisted = serde_json::to_vec(&state.persisted().await).expect("state JSON");

        assert!(!String::from_utf8_lossy(&persisted).contains("PM-secret-license"));
        assert!(String::from_utf8_lossy(&persisted).contains("lease-1"));
    }
}
