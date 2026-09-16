use async_trait::async_trait;
use axum::response::IntoResponse;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use enterprise_protocol::{
    ActivationPayload, ActivationResponse, ClientInfo, DownloadGrant, EnterpriseStatus,
    Fingerprint, Grant, HeartbeatPayload, LicenseErrorCode, LicenseState, ManifestResponse,
    RequestEnvelope, ResponseEnvelope,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::fmt;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

const REQUEST_LOGS_FEATURE: &str = "enterprise.request_logs";
const STATISTICS_FEATURE: &str = "enterprise.statistics";
const PERSISTED_SETTING: &str = "enterprise_license";

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HeartbeatResponse {
    pub license_id: String,
    pub activation_id: String,
    pub lease_id: String,
    pub lease_token: String,
    pub entitlements: Vec<String>,
    pub lease_expires_at: String,
    pub heartbeat_due_at: String,
    pub grace_seconds: u64,
    pub plugin_release_id: String,
    pub release_changed: bool,
    pub available_release_id: Option<String>,
    pub grant: Grant,
}

#[async_trait]
pub(crate) trait LicenseService: Send + Sync {
    async fn validate(
        &self,
        _request: RequestEnvelope<enterprise_protocol::ValidatePayload>,
    ) -> Result<ResponseEnvelope<enterprise_protocol::ValidateResponse>, LicenseClientError> {
        Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "validação de licença não disponível",
        ))
    }

    async fn activate(
        &self,
        request: RequestEnvelope<ActivationPayload>,
    ) -> Result<ResponseEnvelope<ActivationResponse>, LicenseClientError>;

    async fn heartbeat(
        &self,
        request: RequestEnvelope<HeartbeatPayload>,
        lease_token: &str,
    ) -> Result<ResponseEnvelope<HeartbeatResponse>, LicenseClientError>;

    async fn status(
        &self,
        _persisted: &PersistedEnterpriseState,
        _fingerprint: &Fingerprint,
    ) -> Result<ResponseEnvelope<enterprise_protocol::LicenseStatusResponse>, LicenseClientError>
    {
        Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "status de licença não disponível",
        ))
    }

    async fn manifest(
        &self,
        _plugin_code: &str,
        _lease_token: &str,
        _lease_id: &str,
        _os: &str,
        _arch: &str,
    ) -> Result<ManifestResponse, LicenseClientError> {
        Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "manifest do plugin não disponível",
        ))
    }

    async fn verify_grant(&self, _response: &ActivationResponse) -> Result<(), LicenseClientError> {
        Ok(())
    }

    async fn verify_manifest(
        &self,
        _manifest: &enterprise_protocol::PluginManifest,
    ) -> Result<(), LicenseClientError> {
        Ok(())
    }

    async fn download(&self, _grant: &DownloadGrant) -> Result<Vec<u8>, LicenseClientError> {
        Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "download do plugin não disponível",
        ))
    }

    async fn deactivate(
        &self,
        _activation_id: &str,
        _activation_token: &str,
        _fingerprint: &Fingerprint,
    ) -> Result<(), LicenseClientError> {
        Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "desativação de licença não disponível",
        ))
    }
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

    fn validate_url(&self) -> String {
        format!("{}/v1/licenses/validate", self.base_url)
    }

    fn manifest_url(&self, plugin_code: &str, lease_id: &str, os: &str, arch: &str) -> String {
        let mut url = reqwest::Url::parse(&format!(
            "{}/v1/plugins/{plugin_code}/manifest",
            self.base_url
        ))
        .expect("base URL de licenças deve ser válida");
        url.query_pairs_mut()
            .append_pair("lease_id", lease_id)
            .append_pair("os", os)
            .append_pair("arch", arch)
            .append_pair("channel", "stable")
            .append_pair("host_version", env!("CARGO_PKG_VERSION"))
            .append_pair(
                "plugin_abi",
                &enterprise_protocol::PLUGIN_ABI_VERSION.to_string(),
            );
        url.into()
    }
}

#[async_trait]
impl LicenseService for HttpLicenseService {
    async fn validate(
        &self,
        request: RequestEnvelope<enterprise_protocol::ValidatePayload>,
    ) -> Result<ResponseEnvelope<enterprise_protocol::ValidateResponse>, LicenseClientError> {
        let response = self
            .client
            .post(self.validate_url())
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
            Err(parse_license_error(status, &body, &request.request_id))
        }
    }

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
            Err(parse_license_error(status, &body, &request.request_id))
        }
    }

    async fn verify_grant(&self, response: &ActivationResponse) -> Result<(), LicenseClientError> {
        let keys = self.public_keys().await?;
        let body = verify_signed_bytes(&keys, &response.grant, "grant")?;
        let grant: enterprise_protocol::GrantBody =
            serde_json::from_slice(&body).map_err(|_| {
                LicenseClientError::new(LicenseErrorCode::ServerError, "grant de licença inválido")
            })?;
        if grant.product != "pom"
            || grant.iss != "license.pieceofmind.cloud"
            || grant.activation_id != response.activation_id
            || grant.lease_id != response.lease_id
            || grant.node_id.is_empty()
            || grant.fingerprint_scheme != enterprise_protocol::FINGERPRINT_SCHEME
            || !grant.fingerprint_hash.starts_with("sha256:")
            || grant.release_id != response.plugin_release_id
        {
            return Err(LicenseClientError::new(
                LicenseErrorCode::ServerError,
                "grant de licença não corresponde à ativação",
            ));
        }
        Ok(())
    }

    async fn verify_manifest(
        &self,
        manifest: &enterprise_protocol::PluginManifest,
    ) -> Result<(), LicenseClientError> {
        let keys = self.public_keys().await?;
        let value = serde_json::to_value(manifest).map_err(|error| {
            LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
        })?;
        let signature = value
            .get("signature")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    "manifest do plugin sem assinatura",
                )
            })?
            .to_owned();
        let mut unsigned = value;
        unsigned
            .as_object_mut()
            .expect("manifest serializa como objeto")
            .remove("signature");
        let bytes = canonical_json(&unsigned)
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::ServerError, error))?;
        verify_public_signature(&keys, &manifest.key_id, &signature, &bytes, "manifest")
    }

    async fn heartbeat(
        &self,
        request: RequestEnvelope<HeartbeatPayload>,
        lease_token: &str,
    ) -> Result<ResponseEnvelope<HeartbeatResponse>, LicenseClientError> {
        let response = self
            .client
            .post(self.heartbeat_url())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .bearer_auth(lease_token)
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
            Err(parse_license_error(status, &body, &request.request_id))
        }
    }

    async fn status(
        &self,
        persisted: &PersistedEnterpriseState,
        fingerprint: &Fingerprint,
    ) -> Result<ResponseEnvelope<enterprise_protocol::LicenseStatusResponse>, LicenseClientError>
    {
        let mut url = reqwest::Url::parse(&format!("{}/v1/licenses/status", self.base_url))
            .expect("base URL de licenças deve ser válida");
        url.query_pairs_mut()
            .append_pair("activation_id", &persisted.activation_id)
            .append_pair("lease_id", &persisted.lease_id)
            .append_pair("fingerprint_scheme", &fingerprint.scheme)
            .append_pair("fingerprint", &fingerprint.hash)
            .append_pair("os", &fingerprint.platform)
            .append_pair("arch", &fingerprint.arch)
            .append_pair("host_version", env!("CARGO_PKG_VERSION"));
        let response = self
            .client
            .get(url)
            .bearer_auth(&persisted.lease_token)
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
            Err(parse_license_error(status, &body, "status"))
        }
    }

    async fn manifest(
        &self,
        plugin_code: &str,
        lease_token: &str,
        lease_id: &str,
        os: &str,
        arch: &str,
    ) -> Result<ManifestResponse, LicenseClientError> {
        let response = self
            .client
            .get(self.manifest_url(plugin_code, lease_id, os, arch))
            .bearer_auth(lease_token)
            .send()
            .await
            .map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
        })?;
        if !status.is_success() {
            return Err(parse_license_error(status, &body, "manifest"));
        }
        let manifest = serde_json::from_slice::<ManifestResponse>(&body)
            .or_else(|_| {
                serde_json::from_slice::<ResponseEnvelope<ManifestResponse>>(&body)
                    .map(|envelope| envelope.payload)
            })
            .map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })?;
        self.verify_manifest(&manifest.manifest).await?;
        Ok(manifest)
    }

    async fn download(&self, grant: &DownloadGrant) -> Result<Vec<u8>, LicenseClientError> {
        let url = if grant.url.starts_with("http://") || grant.url.starts_with("https://") {
            grant.url.clone()
        } else {
            format!("{}/{}", self.base_url, grant.url.trim_start_matches('/'))
        };
        let response = self
            .client
            .get(url)
            .bearer_auth(&grant.token)
            .send()
            .await
            .map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
        })?;
        if !status.is_success() {
            return Err(parse_license_error(status, &body, "download"));
        }
        Ok(body.to_vec())
    }

    async fn deactivate(
        &self,
        activation_id: &str,
        activation_token: &str,
        fingerprint: &Fingerprint,
    ) -> Result<(), LicenseClientError> {
        let request = RequestEnvelope::new(
            uuid::Uuid::new_v4().to_string(),
            format!("deactivate:{activation_id}:{}", fingerprint.hash),
            client_info(),
            enterprise_protocol::DeactivatePayload {
                fingerprint_scheme: fingerprint.scheme.clone(),
                fingerprint: fingerprint.hash.clone(),
                reason: "operator_request".into(),
            },
        );
        let response = self
            .client
            .delete(format!(
                "{}/v1/licenses/activations/{activation_id}",
                self.base_url
            ))
            .bearer_auth(activation_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.bytes().await.unwrap_or_default();
            Err(parse_license_error(status, &body, "deactivate"))
        }
    }
}

#[derive(Debug, Deserialize)]
struct PublicKeysResponse {
    keys: Vec<PublicKeyInfo>,
}

#[derive(Debug, Deserialize)]
struct PublicKeyInfo {
    key_id: String,
    alg: String,
    public_key: String,
}

impl HttpLicenseService {
    async fn public_keys(&self) -> Result<Vec<PublicKeyInfo>, LicenseClientError> {
        let response = self
            .client
            .get(format!("{}/v1/keys", self.base_url))
            .send()
            .await
            .map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })?;
        let status = response.status();
        let body = response.bytes().await.map_err(|error| {
            LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
        })?;
        if !status.is_success() {
            return Err(parse_license_error(status, &body, "keys"));
        }
        serde_json::from_slice::<PublicKeysResponse>(&body)
            .map(|response| response.keys)
            .map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })
    }
}

fn verify_signed_bytes(
    keys: &[PublicKeyInfo],
    grant: &enterprise_protocol::Grant,
    kind: &str,
) -> Result<Vec<u8>, LicenseClientError> {
    let body = URL_SAFE_NO_PAD.decode(&grant.body).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("{kind} de licença malformado"),
        )
    })?;
    verify_public_signature(keys, &grant.key_id, &grant.signature, &body, kind)?;
    Ok(body)
}

fn verify_public_signature(
    keys: &[PublicKeyInfo],
    key_id: &str,
    signature: &str,
    bytes: &[u8],
    kind: &str,
) -> Result<(), LicenseClientError> {
    let key = keys
        .iter()
        .find(|key| key.key_id == key_id)
        .ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::ServerError,
                format!("chave pública de {kind} desconhecida"),
            )
        })?;
    if key.alg != "Ed25519" {
        return Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("algoritmo de {kind} não suportado"),
        ));
    }
    let raw_key = URL_SAFE_NO_PAD.decode(&key.public_key).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("chave pública de {kind} inválida"),
        )
    })?;
    let raw_key: [u8; 32] = raw_key.try_into().map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("tamanho da chave pública de {kind} inválido"),
        )
    })?;
    let verifying_key = VerifyingKey::from_bytes(&raw_key).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("chave pública de {kind} inválida"),
        )
    })?;
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("assinatura de {kind} inválida"),
        )
    })?;
    let signature = Signature::from_slice(&signature).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("tamanho da assinatura de {kind} inválido"),
        )
    })?;
    verifying_key.verify(bytes, &signature).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("assinatura de {kind} não confere"),
        )
    })
}

fn canonical_json(value: &serde_json::Value) -> Result<Vec<u8>, String> {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            serde_json::to_vec(value).map_err(|error| error.to_string())
        }
        serde_json::Value::String(value) => {
            serde_json::to_vec(value).map_err(|error| error.to_string())
        }
        serde_json::Value::Array(values) => {
            let mut output = vec![b'['];
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend(canonical_json(value)?);
            }
            output.push(b']');
            Ok(output)
        }
        serde_json::Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            let mut output = vec![b'{'];
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend(serde_json::to_vec(key).map_err(|error| error.to_string())?);
                output.push(b':');
                output.extend(canonical_json(&values[*key])?);
            }
            output.push(b'}');
            Ok(output)
        }
    }
}

fn parse_license_error(status: StatusCode, body: &[u8], _request_id: &str) -> LicenseClientError {
    let body = serde_json::from_slice::<serde_json::Value>(body).ok();
    let wire_code = body
        .as_ref()
        .and_then(|body| body.get("code"))
        .and_then(serde_json::Value::as_str);
    let code = wire_code
        .and_then(license_error_code_from_wire)
        .unwrap_or_else(|| match status {
            StatusCode::TOO_MANY_REQUESTS => LicenseErrorCode::RateLimited,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => LicenseErrorCode::InvalidKey,
            _ => LicenseErrorCode::ServerError,
        });
    let message = body
        .as_ref()
        .and_then(|body| body.get("message_for_user"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("O servidor de licenças está indisponível.");
    LicenseClientError::new(code, message)
}

fn license_error_code_from_wire(code: &str) -> Option<LicenseErrorCode> {
    Some(match code {
        "INVALID_KEY" => LicenseErrorCode::InvalidKey,
        "FEATURE_NOT_ENTITLED" => LicenseErrorCode::FeatureNotEntitled,
        "TOO_MANY_ACTIVATIONS" => LicenseErrorCode::TooManyActivations,
        "EXPIRED" | "LEASE_EXPIRED" => LicenseErrorCode::Expired,
        "SUSPENDED" => LicenseErrorCode::Suspended,
        "REVOKED" | "RELEASE_REVOKED" => LicenseErrorCode::Revoked,
        "FINGERPRINT_MISMATCH" => LicenseErrorCode::FingerprintMismatch,
        "LEASE_UNKNOWN" => LicenseErrorCode::LeaseUnknown,
        "RATE_LIMITED" => LicenseErrorCode::RateLimited,
        _ => return None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedEnterpriseState {
    pub license_id: String,
    pub activation_id: String,
    pub activation_token: String,
    pub lease_id: String,
    pub lease_token: String,
    pub fingerprint: Fingerprint,
    pub signed_grant: Grant,
    pub lease_expires_at: String,
    pub heartbeat_due_at: String,
    pub grace_seconds: u64,
    pub last_success_at: Option<String>,
    pub plugin_release_id: String,
    pub plugin_version: String,
    pub features: enterprise_protocol::EnterpriseFeatures,
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
    database: Option<Arc<crate::db::PomDb>>,
    host: Arc<Mutex<Option<crate::enterprise_host::EnterpriseHost>>>,
    core_update_lock: Arc<Mutex<()>>,
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
            database: None,
            host: Arc::new(Mutex::new(None)),
            core_update_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn runtime_with_database(
        node_id: &str,
        database: Arc<crate::db::PomDb>,
    ) -> Arc<Self> {
        let fingerprint = Fingerprint::from_components(
            std::env::consts::OS,
            std::env::consts::ARCH,
            node_id,
            &machine_component(),
            &board_component(),
            &installation_component(),
        );
        let persisted = database
            .local_setting_json(PERSISTED_SETTING)
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<PersistedEnterpriseState>(&json).ok())
            .filter(|state| state.fingerprint == fingerprint);
        let mut state = Self::new(
            Arc::new(HttpLicenseService::new(license_base_url())),
            fingerprint,
        );
        state.database = Some(database);
        if let Ok(mut slot) = state.persisted.try_lock() {
            *slot = persisted;
        }
        let state = Arc::new(state);
        state.spawn_heartbeat_supervisor();
        if let Some(persisted) = state
            .persisted
            .try_lock()
            .ok()
            .and_then(|slot| slot.clone())
        {
            if let Some(path) = persisted.plugin_path.clone().map(std::path::PathBuf::from) {
                if path.is_file() {
                    let state_for_host = state.clone();
                    let database_for_host = state.database.clone();
                    let service = state.service.clone();
                    let fingerprint = state.fingerprint.clone();
                    tokio::spawn(async move {
                        let Some(database) = database_for_host else {
                            return;
                        };
                        let remote_state = service.status(&persisted, &fingerprint).await;
                        let allowed = match remote_state {
                            Ok(remote) => {
                                remote.payload.activation_id == persisted.activation_id
                                    && remote.payload.lease_id == persisted.lease_id
                                    && matches!(remote.payload.state.as_str(), "active" | "grace")
                            }
                            Err(_) => lease_allows_offline(&persisted),
                        };
                        if !allowed {
                            let mut status = state_for_host.status.write().await;
                            status.active = false;
                            status.state = LicenseState::Expired;
                            return;
                        }
                        let database_path = database.path().with_file_name("enterprise.db");
                        match crate::enterprise_host::EnterpriseHost::start(&path, &database_path)
                            .await
                        {
                            Ok(host) => {
                                *state_for_host.host.lock().await = Some(host);
                                state_for_host.restore_ready(&persisted).await;
                            }
                            Err(error) => {
                                tracing::warn!(error = %error, "plugin Enterprise persistido não pôde ser restaurado")
                            }
                        }
                    });
                }
            }
        }
        state
    }

    pub(crate) async fn status(&self) -> EnterpriseStatus {
        self.status.read().await.clone()
    }

    pub(crate) fn peer_proof(
        &self,
        challenge: &crate::enterprise_p2p::PeerFingerprintChallenge,
        signing_key: &ed25519_dalek::SigningKey,
    ) -> crate::enterprise_p2p::PeerFingerprintProof {
        crate::enterprise_p2p::answer_challenge(
            challenge,
            &self.fingerprint,
            env!("CARGO_PKG_VERSION"),
            signing_key,
        )
    }

    pub(crate) fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
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
        let state = Arc::new(Self::new(
            Arc::new(HttpLicenseService::new(license_base_url())),
            fingerprint,
        ));
        state.spawn_heartbeat_supervisor();
        state
    }

    pub(crate) async fn activate(&self, license_key: &str) -> Result<(), LicenseClientError> {
        if license_key.trim().is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::InvalidKey,
                "Informe uma chave de licença.",
            ));
        }

        let validate_request = RequestEnvelope::new(
            uuid::Uuid::new_v4().to_string(),
            format!("validate:{}", stable_digest(license_key)),
            client_info(),
            enterprise_protocol::ValidatePayload {
                license_key: license_key.to_owned(),
                requested_features: vec![REQUEST_LOGS_FEATURE.into(), STATISTICS_FEATURE.into()],
                fingerprint_scheme: Some(self.fingerprint.scheme.clone()),
                fingerprint: Some(self.fingerprint.hash.clone()),
            },
        );
        let validation = self.service.validate(validate_request).await?;
        let requested_features = [REQUEST_LOGS_FEATURE, STATISTICS_FEATURE];
        if !requested_features.iter().any(|feature| {
            validation
                .payload
                .entitlements
                .iter()
                .any(|entitlement| entitlement == feature)
        }) {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "A chave não inclui uma funcionalidade Enterprise.",
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
                fingerprint_scheme: self.fingerprint.scheme.clone(),
                fingerprint: self.fingerprint.hash.clone(),
                installation_id_hash: Some(self.fingerprint.installation_id_hash.clone()),
                node_id: self.fingerprint.node_id.clone(),
                cluster_id_hash: None,
                requested_features: vec![REQUEST_LOGS_FEATURE.into(), STATISTICS_FEATURE.into()],
            },
        );
        let response = self.service.activate(request).await?;
        self.service.verify_grant(&response.payload).await?;
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
        let features = EnterpriseFeaturesFromEntitlements::from(response.entitlements.as_slice()).0;
        *self.pending.lock().await = Some(PendingActivation {
            response: response.clone(),
            fingerprint: self.fingerprint.clone(),
        });
        let persisted = PersistedEnterpriseState {
            license_id: response.license_id,
            activation_id: response.activation_id,
            activation_token: response.activation_token,
            lease_id: response.lease_id,
            lease_token: response.lease_token,
            fingerprint: self.fingerprint.clone(),
            signed_grant: response.grant,
            lease_expires_at: response.lease_expires_at,
            heartbeat_due_at: response.heartbeat_due_at,
            grace_seconds: response.grace_seconds,
            last_success_at: None,
            plugin_release_id: response.plugin_release_id,
            plugin_version: response.plugin_manifest.version.clone(),
            features,
            plugin_sha256: response.plugin_manifest.sha256,
            plugin_path: None,
        };
        if let Some(database) = &self.database {
            database
                .save_local_setting(PERSISTED_SETTING, &persisted)
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self.persisted.lock().await = Some(persisted);
        let mut status = self.status.write().await;
        status.active = false;
        status.state = LicenseState::Unavailable;
        status.features = EnterpriseStatus::inactive().features;
        status.plugin_version = None;
        status.lease_expires_at = None;
        status.core_update = None;
        Ok(())
    }

    pub(crate) async fn activate_and_install(
        &self,
        license_key: &str,
    ) -> Result<(), LicenseClientError> {
        self.activate(license_key).await?;
        let pending = self.pending.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "Nenhuma ativação Enterprise aguardando o download.",
            )
        })?;
        let manifest = self
            .service
            .manifest(
                "enterprise",
                &pending.response.lease_token,
                &pending.response.lease_id,
                &self.fingerprint.platform,
                &self.fingerprint.arch,
            )
            .await?;
        validate_manifest(&pending.response, &manifest.manifest)?;
        let bytes = self.service.download(&manifest.download).await?;
        validate_download(&manifest.manifest, &bytes)?;
        let path = plugin_path(
            &manifest.manifest.release_id,
            &self.fingerprint.platform,
            &self.fingerprint.arch,
        )?;
        install_plugin_atomically(&path, &bytes)?;
        let database_path = self
            .database
            .as_ref()
            .map(|database| database.path().with_file_name("enterprise.db"))
            .unwrap_or_else(|| crate::config::project_root().join("enterprise.db"));
        let host = match crate::enterprise_host::EnterpriseHost::start(&path, &database_path).await
        {
            Ok(host) => host,
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    error.to_string(),
                ));
            }
        };
        *self.host.lock().await = Some(host);
        let mut persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Ativação local ausente.")
        })?;
        persisted.plugin_release_id = manifest.manifest.release_id;
        persisted.plugin_version = manifest.manifest.version.clone();
        persisted.plugin_sha256 = manifest.manifest.sha256.clone();
        persisted.plugin_path = Some(path.to_string_lossy().into_owned());
        persisted.last_success_at = Some(chrono::Utc::now().to_rfc3339());
        if let Some(database) = &self.database {
            if let Err(error) = database.save_local_setting(PERSISTED_SETTING, &persisted) {
                if let Some(host) = self.host.lock().await.take() {
                    let _ = host.stop().await;
                }
                let _ = std::fs::remove_file(&path);
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    error.to_string(),
                ));
            }
        }
        *self.persisted.lock().await = Some(persisted);
        self.mark_plugin_ready(&manifest.manifest.version).await
    }

    pub(crate) async fn deactivate(&self) -> Result<(), LicenseClientError> {
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Nenhuma licença ativa.")
        })?;
        self.service
            .deactivate(
                &persisted.activation_id,
                &persisted.activation_token,
                &self.fingerprint,
            )
            .await?;
        if let Some(host) = self.host.lock().await.take() {
            let _ = host.stop().await;
        }
        if let Some(database) = &self.database {
            database
                .delete_local_setting(PERSISTED_SETTING)
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self.pending.lock().await = None;
        *self.persisted.lock().await = None;
        *self.status.write().await = EnterpriseStatus::inactive();
        Ok(())
    }

    async fn restore_ready(&self, persisted: &PersistedEnterpriseState) {
        let mut status = self.status.write().await;
        status.active = true;
        status.state = LicenseState::Active;
        status.features = persisted.features.clone();
        status.plugin_version = Some(persisted.plugin_version.clone());
        status.lease_expires_at = Some(persisted.lease_expires_at.clone());
        status.core_update = None;
    }

    pub(crate) async fn ingest_audit_event(&self, event: crate::reqlog::AuditEvent) -> bool {
        let Some(host) = self.host.lock().await.clone() else {
            return false;
        };
        let event = match event {
            crate::reqlog::AuditEvent::Begin(request) => {
                serde_json::json!({"type": "begin", "request": request})
            }
            crate::reqlog::AuditEvent::Complete { id, completion } => {
                serde_json::json!({"type": "complete", "id": id, "completion": completion})
            }
        };
        if let Err(error) = host.ingest(event).await {
            tracing::warn!(error = %error, "evento Enterprise não entrou no plugin");
            return false;
        }
        true
    }

    pub(crate) async fn query_audit(
        &self,
        operation: &str,
        query: serde_json::Value,
    ) -> Option<serde_json::Value> {
        let status = self.status().await;
        if !status.active {
            return None;
        }
        let host = self.host.lock().await.clone()?;
        let mut payload = query;
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "operation".into(),
                serde_json::Value::String(operation.into()),
            );
        }
        match host.query(payload).await {
            Ok(result) => Some(result),
            Err(error) => {
                tracing::warn!(error = %error, "consulta do plugin Enterprise falhou");
                None
            }
        }
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
        status.core_update = None;
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
            status.core_update = None;
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
                fingerprint_scheme: self.fingerprint.scheme.clone(),
                fingerprint: self.fingerprint.hash.clone(),
                node_id: self.fingerprint.node_id.clone(),
                nonce: nonce.to_owned(),
                client_time: chrono::Utc::now().to_rfc3339(),
            },
        );
        match self
            .service
            .heartbeat(request, &persisted.lease_token)
            .await
        {
            Ok(response) => {
                let HeartbeatResponse {
                    lease_token,
                    entitlements,
                    lease_expires_at,
                    heartbeat_due_at,
                    grace_seconds,
                    plugin_release_id,
                    release_changed,
                    available_release_id,
                    grant,
                    ..
                } = response.payload;
                let features = EnterpriseFeaturesFromEntitlements::from(entitlements.as_slice()).0;
                let mut persisted = self.persisted.lock().await;
                if let Some(state) = persisted.as_mut() {
                    state.lease_token = lease_token;
                    state.lease_expires_at = lease_expires_at.clone();
                    state.heartbeat_due_at = heartbeat_due_at;
                    state.grace_seconds = grace_seconds;
                    state.plugin_release_id = plugin_release_id.clone();
                    state.signed_grant = grant;
                    state.features = features.clone();
                    state.last_success_at = Some(chrono::Utc::now().to_rfc3339());
                    if let Some(database) = &self.database {
                        let _ = database.save_local_setting(PERSISTED_SETTING, state);
                    }
                }
                drop(persisted);
                if release_changed {
                    tracing::info!(
                        plugin_release_id = %plugin_release_id,
                        available_release_id = ?available_release_id,
                        "heartbeat Enterprise indicou alteração de release"
                    );
                }
                let mut status = self.status.write().await;
                if status.active {
                    status.state = LicenseState::Active;
                }
                status.features = features;
                status.lease_expires_at = Some(lease_expires_at);
                Ok(())
            }
            Err(error) => {
                let mut status = self.status.write().await;
                let stop_host = matches!(
                    error.code,
                    LicenseErrorCode::Revoked
                        | LicenseErrorCode::Suspended
                        | LicenseErrorCode::Expired
                );
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
                if !status.active {
                    status.core_update = None;
                }
                drop(status);
                if stop_host {
                    if let Some(host) = self.host.lock().await.take() {
                        let _ = host.stop().await;
                    }
                }
                Err(error)
            }
        }
    }

    fn spawn_heartbeat_supervisor(self: &Arc<Self>) {
        let state = self.clone();
        tokio::spawn(async move {
            loop {
                let delay = state.heartbeat_delay().await;
                tokio::time::sleep(delay).await;
                if state.persisted.lock().await.is_none() {
                    continue;
                }
                if let Err(error) = state.heartbeat(&uuid::Uuid::new_v4().to_string()).await {
                    tracing::warn!(error = %error, "heartbeat Enterprise falhou");
                } else if let Err(error) = state.detect_core_update().await {
                    tracing::warn!(error = %error, "detecção de atualização do pom falhou");
                }
            }
        });
    }

    async fn heartbeat_delay(&self) -> std::time::Duration {
        let persisted = self.persisted.lock().await.clone();
        let Some(persisted) = persisted else {
            return std::time::Duration::from_secs(300);
        };
        let Some(due_at) = chrono::DateTime::parse_from_rfc3339(&persisted.heartbeat_due_at).ok()
        else {
            return std::time::Duration::from_secs(24 * 60 * 60);
        };
        let seconds = (due_at.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
        std::time::Duration::from_secs(seconds.max(30) as u64)
    }

    /// Procura uma release do próprio binário somente depois de um heartbeat
    /// aceito e com o entitlement ativo. A detecção apenas publica o aviso no
    /// status; download, troca e reinício ficam reservados à ação explícita.
    async fn detect_core_update(&self) -> Result<(), LicenseClientError> {
        let _guard = self.core_update_lock.lock().await;
        if !self.status().await.active {
            return Ok(());
        }
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "lease Enterprise local ausente para atualização do pom",
            )
        })?;
        let manifest = self.core_manifest(&persisted).await?;
        crate::core_update::validate_manifest(&manifest.manifest, env!("CARGO_PKG_VERSION"))
            .map_err(core_update_error)?;
        let available = crate::core_update::is_newer_release(
            env!("CARGO_PKG_VERSION"),
            crate::BUILD_COMMIT,
            &manifest.manifest,
        )
        .then(|| enterprise_protocol::CoreUpdate {
            release_id: manifest.manifest.release_id.clone(),
            version: manifest.manifest.version.clone(),
            changelog: manifest.manifest.changelog.clone(),
        });
        let mut status = self.status.write().await;
        if status.active {
            if let Some(update) = &available {
                tracing::info!(
                    release_id = %update.release_id,
                    version = %update.version,
                    "release nova do pom encontrada"
                );
            }
            status.core_update = available;
        }
        Ok(())
    }

    async fn core_manifest(
        &self,
        persisted: &PersistedEnterpriseState,
    ) -> Result<ManifestResponse, LicenseClientError> {
        self.service
            .manifest(
                crate::core_update::CORE_PLUGIN_CODE,
                &persisted.lease_token,
                &persisted.lease_id,
                &self.fingerprint.platform,
                &self.fingerprint.arch,
            )
            .await
    }

    /// Baixa e instala somente a release que o operador confirmou na UI.
    /// Reconsulta o manifest antes do download para não reutilizar uma
    /// concessão ou metadados antigos de uma detecção diária.
    pub(crate) async fn apply_core_update(
        &self,
        requested_release_id: &str,
    ) -> Result<(), LicenseClientError> {
        let _guard = self.core_update_lock.lock().await;
        let status = self.status().await;
        if !status.active {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "uma licença Enterprise ativa é necessária para atualizar o pom",
            ));
        }
        let requested_release_id = requested_release_id.trim();
        let Some(available) = status.core_update.as_ref() else {
            return Err(update_unavailable(
                "nenhuma atualização do pom está disponível",
            ));
        };
        if available.release_id != requested_release_id {
            return Err(update_unavailable(
                "a atualização confirmada já não é a atualização disponível",
            ));
        }
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "lease Enterprise local ausente para atualizar o pom",
            )
        })?;
        let manifest = self.core_manifest(&persisted).await?;
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

    pub(crate) async fn persisted(&self) -> PersistedEnterpriseState {
        self.persisted
            .lock()
            .await
            .clone()
            .expect("estado persistido só existe após ativação")
    }
}

fn core_update_error(message: String) -> LicenseClientError {
    LicenseClientError::new(LicenseErrorCode::ServerError, message)
}

fn update_unavailable(message: impl Into<String>) -> LicenseClientError {
    LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, message)
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

#[derive(Debug, Deserialize)]
pub(crate) struct LicenseActivationRequest {
    pub license_key: String,
}

pub(crate) async fn activate_license(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::Json(request): axum::Json<LicenseActivationRequest>,
) -> axum::response::Response {
    match state
        .enterprise
        .activate_and_install(&request.license_key)
        .await
    {
        Ok(()) => axum::Json(state.enterprise.status().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn refresh_license(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    match state
        .enterprise
        .heartbeat(&uuid::Uuid::new_v4().to_string())
        .await
    {
        Ok(()) => {
            if let Err(error) = state.enterprise.detect_core_update().await {
                tracing::warn!(error = %error, "detecção de atualização do pom falhou");
            }
            axum::Json(state.enterprise.status().await).into_response()
        }
        Err(error) => license_error_response(error),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct CoreUpdateApplyRequest {
    pub release_id: String,
}

pub(crate) async fn apply_core_update(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::Json(request): axum::Json<CoreUpdateApplyRequest>,
) -> axum::response::Response {
    match state
        .enterprise
        .apply_core_update(&request.release_id)
        .await
    {
        Ok(()) => axum::Json(serde_json::json!({
            "restarting": true,
            "release_id": request.release_id,
        }))
        .into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn deactivate_license(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    match state.enterprise.deactivate().await {
        Ok(()) => axum::Json(state.enterprise.status().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn enterprise_peer_fingerprint(
    _auth: crate::auth::InternalNodeAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Query(challenge): axum::extract::Query<
        crate::enterprise_p2p::PeerFingerprintChallenge,
    >,
) -> axum::response::Response {
    if challenge.protocol != crate::enterprise_p2p::PEER_PROTOCOL
        || challenge.nonce.len() < 16
        || challenge.nonce.len() > 256
    {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "desafio Enterprise inválido"})),
        )
            .into_response();
    }
    let identity = crate::identity::Identity::load_or_create(&crate::identity::key_path());
    if identity.node_id.0 != state.enterprise.fingerprint().node_id {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": "identidade do peer divergente"})),
        )
            .into_response();
    }
    axum::Json(
        state
            .enterprise
            .peer_proof(&challenge, &identity.signing_key),
    )
    .into_response()
}

fn license_error_response(error: LicenseClientError) -> axum::response::Response {
    use axum::http::StatusCode;
    let status = match error.code() {
        LicenseErrorCode::InvalidKey
        | LicenseErrorCode::FingerprintMismatch
        | LicenseErrorCode::FeatureNotEntitled => StatusCode::UNPROCESSABLE_ENTITY,
        LicenseErrorCode::Revoked | LicenseErrorCode::Suspended | LicenseErrorCode::Expired => {
            StatusCode::CONFLICT
        }
        LicenseErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        LicenseErrorCode::UpdateUnavailable => StatusCode::CONFLICT,
        _ => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        axum::Json(serde_json::json!({
            "code": license_error_code(error.code()),
            "error": error.to_string(),
            "manage_url": enterprise_protocol::ENTERPRISE_MANAGE_URL,
        })),
    )
        .into_response()
}

fn license_error_code(code: LicenseErrorCode) -> &'static str {
    match code {
        LicenseErrorCode::InvalidKey => "invalid_key",
        LicenseErrorCode::FeatureNotEntitled => "feature_not_entitled",
        LicenseErrorCode::TooManyActivations => "too_many_activations",
        LicenseErrorCode::Expired => "expired",
        LicenseErrorCode::Suspended => "suspended",
        LicenseErrorCode::Revoked => "revoked",
        LicenseErrorCode::FingerprintMismatch => "fingerprint_mismatch",
        LicenseErrorCode::LeaseUnknown => "lease_unknown",
        LicenseErrorCode::RateLimited => "rate_limited",
        LicenseErrorCode::UpdateUnavailable => "update_unavailable",
        LicenseErrorCode::ServerError => "server_error",
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

fn license_base_url() -> String {
    std::env::var("POM_LICENSE_BASE_URL")
        .ok()
        .filter(|_| {
            cfg!(test)
                || std::env::var("POM_LICENSE_ALLOW_CUSTOM_BASE_URL")
                    .ok()
                    .as_deref()
                    == Some("1")
        })
        .unwrap_or_else(|| "https://license.pieceofmind.cloud".into())
}

fn plugin_path(
    release_id: &str,
    os: &str,
    arch: &str,
) -> Result<std::path::PathBuf, LicenseClientError> {
    if release_id.is_empty()
        || !release_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "release_id de plugin inválido",
        ));
    }
    let extension = match os {
        "windows" => "dll",
        "macos" => "dylib",
        _ => "so",
    };
    Ok(crate::config::project_root()
        .join("plugins")
        .join("enterprise")
        .join(format!("{release_id}-{arch}.{extension}")))
}

fn validate_manifest(
    activation: &ActivationResponse,
    manifest: &enterprise_protocol::PluginManifest,
) -> Result<(), LicenseClientError> {
    if manifest.product != "pom"
        || manifest.plugin_abi != enterprise_protocol::PLUGIN_ABI_VERSION
        || manifest.release_id != activation.plugin_release_id
        || manifest.os != std::env::consts::OS
        || manifest.arch != std::env::consts::ARCH
        || manifest.signature.trim().is_empty()
    {
        return Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "manifest do plugin incompatível com esta instalação",
        ));
    }
    Ok(())
}

fn validate_download(
    manifest: &enterprise_protocol::PluginManifest,
    bytes: &[u8],
) -> Result<(), LicenseClientError> {
    if bytes.len() as u64 != manifest.size {
        return Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "tamanho do plugin divergente",
        ));
    }
    let actual = hex::encode(sha2::Sha256::digest(bytes));
    let expected = manifest
        .sha256
        .strip_prefix("sha256:")
        .unwrap_or(&manifest.sha256);
    if actual != expected {
        return Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "hash do plugin divergente",
        ));
    }
    Ok(())
}

fn io_error(error: std::io::Error) -> LicenseClientError {
    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
}

fn install_plugin_atomically(
    destination: &std::path::Path,
    bytes: &[u8],
) -> Result<(), LicenseClientError> {
    let parent = destination
        .parent()
        .ok_or_else(|| io_error(std::io::Error::other("diretório do plugin ausente")))?;
    std::fs::create_dir_all(parent).map_err(io_error)?;
    let temporary = parent.join(format!(
        ".{}.part-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("enterprise-plugin"),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::File::create(&temporary).map_err(io_error)?;
        file.write_all(bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        std::fs::rename(&temporary, destination).map_err(io_error)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn lease_allows_offline(persisted: &PersistedEnterpriseState) -> bool {
    let Ok(expires_at) = chrono::DateTime::parse_from_rfc3339(&persisted.lease_expires_at) else {
        return false;
    };
    let grace_end = expires_at.with_timezone(&chrono::Utc)
        + chrono::Duration::seconds(persisted.grace_seconds as i64);
    chrono::Utc::now() <= grace_end
}

#[derive(Clone)]
pub(crate) struct MockLicenseService {
    response: Arc<Mutex<MockResponse>>,
    heartbeat_error: Arc<Mutex<Option<LicenseErrorCode>>>,
    artifact: Arc<Mutex<Option<Vec<u8>>>>,
    manifest_plugins: Arc<Mutex<Vec<String>>>,
    core_manifest: Arc<Mutex<Option<ManifestResponse>>>,
    download_calls: Arc<Mutex<u32>>,
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
            artifact: Arc::new(Mutex::new(None)),
            manifest_plugins: Arc::new(Mutex::new(Vec::new())),
            core_manifest: Arc::new(Mutex::new(None)),
            download_calls: Arc::new(Mutex::new(0)),
        }
    }

    pub(crate) fn error(code: LicenseErrorCode) -> Self {
        Self {
            response: Arc::new(Mutex::new(MockResponse::Error(code))),
            heartbeat_error: Arc::new(Mutex::new(None)),
            artifact: Arc::new(Mutex::new(None)),
            manifest_plugins: Arc::new(Mutex::new(Vec::new())),
            core_manifest: Arc::new(Mutex::new(None)),
            download_calls: Arc::new(Mutex::new(0)),
        }
    }

    #[cfg(test)]
    pub(crate) fn valid_with_artifact(response: ActivationResponse, artifact: Vec<u8>) -> Self {
        Self {
            response: Arc::new(Mutex::new(MockResponse::Valid(response))),
            heartbeat_error: Arc::new(Mutex::new(None)),
            artifact: Arc::new(Mutex::new(Some(artifact))),
            manifest_plugins: Arc::new(Mutex::new(Vec::new())),
            core_manifest: Arc::new(Mutex::new(None)),
            download_calls: Arc::new(Mutex::new(0)),
        }
    }

    #[cfg(test)]
    pub(crate) async fn manifest_plugins(&self) -> Vec<String> {
        self.manifest_plugins.lock().await.clone()
    }

    #[cfg(test)]
    pub(crate) async fn set_core_manifest(
        &self,
        manifest: enterprise_protocol::PluginManifest,
        artifact: Vec<u8>,
    ) {
        *self.core_manifest.lock().await = Some(ManifestResponse {
            manifest,
            download: DownloadGrant {
                token: "mock-core-download".into(),
                url: "/mock/core-download".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        });
        *self.artifact.lock().await = Some(artifact);
    }

    #[cfg(test)]
    pub(crate) async fn download_calls(&self) -> u32 {
        *self.download_calls.lock().await
    }

    pub(crate) async fn set_heartbeat_error(&self, code: LicenseErrorCode) {
        *self.heartbeat_error.lock().await = Some(code);
    }
}

#[async_trait]
impl LicenseService for MockLicenseService {
    async fn validate(
        &self,
        request: RequestEnvelope<enterprise_protocol::ValidatePayload>,
    ) -> Result<ResponseEnvelope<enterprise_protocol::ValidateResponse>, LicenseClientError> {
        match self.response.lock().await.clone() {
            MockResponse::Valid(payload) => Ok(ResponseEnvelope {
                protocol: enterprise_protocol::LICENSE_PROTOCOL.into(),
                request_id: request.request_id,
                server_time: "2099-01-01T00:00:00Z".into(),
                expires_at: Some(payload.lease_expires_at.clone()),
                payload: enterprise_protocol::ValidateResponse {
                    license_id: payload.license_id,
                    status: "active".into(),
                    entitlements: payload.entitlements,
                    activation_required: true,
                    activation_id: Some(payload.activation_id),
                    eligible_release: Some(enterprise_protocol::EligibleRelease {
                        release_id: payload.plugin_release_id,
                        version: payload.plugin_manifest.version,
                    }),
                },
            }),
            MockResponse::Error(code) => Err(LicenseClientError::new(code, "mock license error")),
        }
    }

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
        _lease_token: &str,
    ) -> Result<ResponseEnvelope<HeartbeatResponse>, LicenseClientError> {
        if let Some(code) = *self.heartbeat_error.lock().await {
            return Err(LicenseClientError::new(code, "mock heartbeat error"));
        }
        let MockResponse::Valid(response) = self.response.lock().await.clone() else {
            return Err(LicenseClientError::new(
                LicenseErrorCode::InvalidKey,
                "mock license error",
            ));
        };
        Ok(ResponseEnvelope {
            protocol: enterprise_protocol::LICENSE_PROTOCOL.into(),
            request_id: request.request_id,
            server_time: "2099-01-01T00:00:00Z".into(),
            expires_at: Some("2099-01-08T00:00:00Z".into()),
            payload: HeartbeatResponse {
                license_id: response.license_id,
                activation_id: response.activation_id,
                lease_id: response.lease_id,
                lease_token: "plt-renewed".into(),
                entitlements: response.entitlements,
                lease_expires_at: "2099-01-08T00:00:00Z".into(),
                heartbeat_due_at: "2099-01-02T00:00:00Z".into(),
                grace_seconds: 86400,
                plugin_release_id: "ent-heartbeat".into(),
                release_changed: false,
                available_release_id: None,
                grant: response.grant,
            },
        })
    }

    async fn manifest(
        &self,
        plugin_code: &str,
        _lease_token: &str,
        _lease_id: &str,
        _os: &str,
        _arch: &str,
    ) -> Result<ManifestResponse, LicenseClientError> {
        self.manifest_plugins
            .lock()
            .await
            .push(plugin_code.to_owned());
        if plugin_code == crate::core_update::CORE_PLUGIN_CODE {
            if let Some(manifest) = self.core_manifest.lock().await.clone() {
                return Ok(manifest);
            }
        }
        let response = self.response.lock().await.clone();
        let MockResponse::Valid(response) = response else {
            return Err(LicenseClientError::new(
                LicenseErrorCode::InvalidKey,
                "mock license error",
            ));
        };
        Ok(ManifestResponse {
            manifest: response.plugin_manifest,
            download: DownloadGrant {
                token: "mock-download".into(),
                url: "/mock/download".into(),
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
        })
    }

    async fn download(&self, _grant: &DownloadGrant) -> Result<Vec<u8>, LicenseClientError> {
        *self.download_calls.lock().await += 1;
        self.artifact.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::ServerError,
                "artefato mock não configurado",
            )
        })
    }

    async fn deactivate(
        &self,
        _activation_id: &str,
        _activation_token: &str,
        _fingerprint: &Fingerprint,
    ) -> Result<(), LicenseClientError> {
        Ok(())
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
            activation_token: "pat-test".into(),
            lease_id: "lease-1".into(),
            lease_token: "plt-test".into(),
            entitlements: vec![
                "enterprise.request_logs".into(),
                "enterprise.statistics".into(),
            ],
            lease_expires_at: "2099-01-01T00:00:00Z".into(),
            heartbeat_due_at: "2099-01-01T00:00:00Z".into(),
            grace_seconds: 172800,
            plugin_release_id: "ent-test".into(),
            plugin_manifest: PluginManifest {
                schema: 1,
                release_id: "ent-test".into(),
                product: "pom".into(),
                version: "0.1.0".into(),
                channel: "stable".into(),
                feature_set: vec!["enterprise.request_logs".into()],
                os: "linux".into(),
                arch: "x86_64".into(),
                min_host_version: "0.1.0".into(),
                plugin_abi: 1,
                sha256: "sha256:test".into(),
                size: 12,
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

    #[test]
    fn manifest_url_uses_the_requested_plugin_code() {
        let service = HttpLicenseService::new("https://license.example");

        let url = service.manifest_url("pom-core", "lease-1", "macos", "aarch64");

        assert_eq!(
            url,
            "https://license.example/v1/plugins/pom-core/manifest?lease_id=lease-1&os=macos&arch=aarch64&channel=stable&host_version=0.1.0&plugin_abi=1"
        );
    }

    #[test]
    fn manifest_signature_is_verified_with_the_published_ed25519_key() {
        use ed25519_dalek::{Signer, SigningKey};

        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let bytes = b"canonical manifest";
        let signature = URL_SAFE_NO_PAD.encode(signing_key.sign(bytes).to_bytes());
        let keys = vec![PublicKeyInfo {
            key_id: "release-key".into(),
            alg: "Ed25519".into(),
            public_key: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes()),
        }];

        verify_public_signature(&keys, "release-key", &signature, bytes, "manifest")
            .expect("published signature");
        assert!(verify_public_signature(
            &keys,
            "release-key",
            &signature,
            b"tampered manifest",
            "manifest"
        )
        .is_err());
    }

    #[tokio::test]
    async fn heartbeat_accepts_the_server_payload_object() {
        let app = axum::Router::new().route(
            "/v1/licenses/leases/heartbeat",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "protocol": "pom-license/v1",
                    "request_id": "heartbeat-request",
                    "server_time": "2099-01-01T00:00:00Z",
                    "expires_at": "2099-01-08T00:00:00Z",
                    "payload": {
                        "license_id": "lic-1",
                        "activation_id": "act-1",
                        "lease_id": "lease-1",
                        "lease_token": "plt-renewed",
                        "entitlements": [
                            "enterprise.request_logs",
                            "enterprise.statistics"
                        ],
                        "lease_expires_at": "2099-01-08T00:00:00Z",
                        "heartbeat_due_at": "2099-01-02T00:00:00Z",
                        "grace_seconds": 172800,
                        "plugin_release_id": "ent-test",
                        "release_changed": false,
                        "available_release_id": null,
                        "grant": {
                            "alg": "Ed25519",
                            "key_id": "test",
                            "body": "grant-body",
                            "signature": "grant-signature"
                        }
                    }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind heartbeat stub");
        let address = listener.local_addr().expect("heartbeat stub address");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve heartbeat stub");
        });

        let service = HttpLicenseService::new(format!("http://{address}"));
        let request = RequestEnvelope::new(
            "heartbeat-request",
            "heartbeat-idempotency",
            client_info(),
            HeartbeatPayload {
                activation_id: "act-1".into(),
                lease_id: "lease-1".into(),
                fingerprint_scheme: "pom-fingerprint-v1".into(),
                fingerprint:
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                node_id: "node-a".into(),
                nonce: "heartbeat-nonce".into(),
                client_time: "2099-01-01T00:00:00Z".into(),
            },
        );

        let response = service.heartbeat(request, "plt-test").await;

        server.abort();
        let response = response
            .expect("server heartbeat payload should deserialize")
            .payload;
        assert_eq!(response.lease_token, "plt-renewed");
        assert_eq!(response.heartbeat_due_at, "2099-01-02T00:00:00Z");
        assert_eq!(response.entitlements.len(), 2);
        assert_eq!(response.plugin_release_id, "ent-test");
        assert!(!response.release_changed);
        assert_eq!(response.available_release_id, None);
    }

    #[tokio::test]
    async fn heartbeat_persists_authoritative_lease_data() {
        let service = Arc::new(MockLicenseService::valid(activation()));
        let state = EnterpriseState::new(service, fingerprint());
        state.activate("PM-test-key").await.expect("activate");
        state
            .mark_plugin_ready("ent-test")
            .await
            .expect("plugin ready");

        state.heartbeat("heartbeat-nonce").await.expect("heartbeat");

        let persisted = state.persisted().await;
        assert_eq!(persisted.lease_token, "plt-renewed");
        assert_eq!(persisted.lease_expires_at, "2099-01-08T00:00:00Z");
        assert_eq!(persisted.heartbeat_due_at, "2099-01-02T00:00:00Z");
        assert_eq!(persisted.grace_seconds, 86400);
        assert_eq!(persisted.plugin_release_id, "ent-heartbeat");
        assert_eq!(persisted.signed_grant.body, "grant-body");
        assert_eq!(
            persisted.features,
            EnterpriseFeatures {
                request_logs: true,
                statistics: true,
            }
        );

        let status = state.status().await;
        assert_eq!(status.state, LicenseState::Active);
        assert_eq!(
            status.lease_expires_at.as_deref(),
            Some("2099-01-08T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn free_state_never_checks_the_pom_core_manifest() {
        let service = Arc::new(MockLicenseService::valid(activation()));
        let state = EnterpriseState::new(service.clone(), fingerprint());

        state
            .detect_core_update()
            .await
            .expect("free mode should skip core updates");

        assert!(service.manifest_plugins().await.is_empty());
    }

    #[tokio::test]
    async fn licensed_detection_advertises_core_release_without_downloading() {
        let artifact = b"new-pom".to_vec();
        let service = Arc::new(MockLicenseService::valid(activation()));
        service
            .set_core_manifest(core_manifest(&artifact), artifact)
            .await;
        let state = EnterpriseState::new(service.clone(), fingerprint());
        state.activate("PM-test-key").await.expect("activate");
        state
            .mark_plugin_ready("ent-test")
            .await
            .expect("plugin ready");

        state
            .detect_core_update()
            .await
            .expect("detect core update");

        let update = state
            .status()
            .await
            .core_update
            .expect("new release should be advertised");
        assert_eq!(update.release_id, "core-new-commit");
        assert_eq!(update.version, "0.2.0");
        assert_eq!(service.download_calls().await, 0);
    }

    #[tokio::test]
    async fn applying_a_different_release_is_rejected_without_downloading() {
        let artifact = b"new-pom".to_vec();
        let service = Arc::new(MockLicenseService::valid(activation()));
        service
            .set_core_manifest(core_manifest(&artifact), artifact)
            .await;
        let state = EnterpriseState::new(service.clone(), fingerprint());
        state.activate("PM-test-key").await.expect("activate");
        state
            .mark_plugin_ready("ent-test")
            .await
            .expect("plugin ready");
        state
            .detect_core_update()
            .await
            .expect("detect core update");

        let error = state
            .apply_core_update("core-not-confirmed")
            .await
            .expect_err("must require the advertised release");

        assert_eq!(error.code(), LicenseErrorCode::UpdateUnavailable);
        assert_eq!(service.download_calls().await, 0);
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
