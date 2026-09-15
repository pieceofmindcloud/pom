use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod abi;

pub const LICENSE_PROTOCOL: &str = "pom-license/v1";
pub const IPC_PROTOCOL: &str = "pom-enterprise-ipc/v1";
pub const ENTERPRISE_MANAGE_URL: &str = "https://pieceofmind.cloud/enterprise";
pub const FINGERPRINT_SCHEME: &str = "pom-fingerprint-v1";
pub const PLUGIN_ABI_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseState {
    NeedsActivation,
    Active,
    Degraded,
    Expired,
    Revoked,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnterpriseFeatures {
    pub request_logs: bool,
    pub statistics: bool,
}

impl EnterpriseFeatures {
    pub const fn disabled() -> Self {
        Self {
            request_logs: false,
            statistics: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnterpriseStatus {
    pub active: bool,
    pub state: LicenseState,
    pub features: EnterpriseFeatures,
    pub manage_url: String,
    pub plugin_version: Option<String>,
    pub lease_expires_at: Option<String>,
}

impl EnterpriseStatus {
    pub fn inactive() -> Self {
        Self {
            active: false,
            state: LicenseState::NeedsActivation,
            features: EnterpriseFeatures::disabled(),
            manage_url: ENTERPRISE_MANAGE_URL.to_owned(),
            plugin_version: None,
            lease_expires_at: None,
        }
    }

    pub fn is_feature_active(&self, feature: EnterpriseFeature) -> bool {
        self.active
            && match feature {
                EnterpriseFeature::RequestLogs => self.features.request_logs,
                EnterpriseFeature::Statistics => self.features.statistics,
            }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnterpriseFeature {
    RequestLogs,
    Statistics,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub scheme: String,
    pub hash: String,
    pub installation_id_hash: String,
    pub platform: String,
    pub arch: String,
    pub node_id: String,
}

impl Fingerprint {
    pub fn from_components(
        platform: &str,
        arch: &str,
        node_id: &str,
        machine_id: &str,
        board_id: &str,
        installation_id: &str,
    ) -> Self {
        let hash = digest_parts(&[
            FINGERPRINT_SCHEME,
            platform,
            arch,
            node_id,
            machine_id,
            board_id,
            installation_id,
        ]);
        let installation_id_hash = digest_parts(&["pom-installation-v1", installation_id]);
        Self {
            scheme: FINGERPRINT_SCHEME.to_owned(),
            hash,
            installation_id_hash,
            platform: platform.to_owned(),
            arch: arch.to_owned(),
            node_id: node_id.to_owned(),
        }
    }
}

fn digest_parts(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub product: String,
    pub version: String,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope<T> {
    pub protocol: String,
    pub request_id: String,
    pub idempotency_key: String,
    pub client: ClientInfo,
    pub payload: T,
}

impl<T> RequestEnvelope<T> {
    pub fn new(
        request_id: impl Into<String>,
        idempotency_key: impl Into<String>,
        client: ClientInfo,
        payload: T,
    ) -> Self {
        Self {
            protocol: LICENSE_PROTOCOL.to_owned(),
            request_id: request_id.into(),
            idempotency_key: idempotency_key.into(),
            client,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope<T> {
    pub protocol: String,
    pub request_id: String,
    pub server_time: String,
    pub expires_at: Option<String>,
    pub payload: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LicenseErrorCode {
    InvalidKey,
    FeatureNotEntitled,
    TooManyActivations,
    Expired,
    Suspended,
    Revoked,
    FingerprintMismatch,
    LeaseUnknown,
    RateLimited,
    ServerError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseError {
    pub code: LicenseErrorCode,
    pub message_for_user: String,
    pub retryable: bool,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub schema: u32,
    pub release_id: String,
    pub product: String,
    pub feature_set: Vec<String>,
    pub os: String,
    pub arch: String,
    pub min_host_version: String,
    pub plugin_abi: u32,
    pub sha256: String,
    pub size: u64,
    pub download_expires_at: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub alg: String,
    pub key_id: String,
    pub body: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivationPayload {
    pub license_key: String,
    pub fingerprint: Fingerprint,
    pub node_id: String,
    pub requested_features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivationResponse {
    pub license_id: String,
    pub activation_id: String,
    pub lease_id: String,
    pub entitlements: Vec<String>,
    pub lease_expires_at: String,
    pub heartbeat_due_at: String,
    pub plugin_release_id: String,
    pub plugin_manifest: PluginManifest,
    pub grant: Grant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    pub activation_id: String,
    pub lease_id: String,
    pub fingerprint: Fingerprint,
    pub node_id: String,
    pub nonce: String,
}

pub fn decode_response<T: DeserializeOwned>(
    body: &[u8],
) -> Result<ResponseEnvelope<T>, serde_json::Error> {
    serde_json::from_slice(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_the_stable_frontend_contract() {
        let status = EnterpriseStatus::inactive();

        assert_eq!(status.active, false);
        assert_eq!(status.state, LicenseState::NeedsActivation);
        assert_eq!(status.manage_url, ENTERPRISE_MANAGE_URL);
        assert_eq!(
            serde_json::to_value(status).expect("status JSON"),
            serde_json::json!({
                "active": false,
                "state": "needs_activation",
                "features": {"request_logs": false, "statistics": false},
                "manage_url": "https://pieceofmind.cloud/enterprise",
                "plugin_version": null,
                "lease_expires_at": null,
            })
        );
    }

    #[test]
    fn fingerprint_is_versioned_and_does_not_expose_raw_components() {
        let fingerprint = Fingerprint::from_components(
            "linux",
            "x86_64",
            "node-a",
            "machine-secret",
            "board-secret",
            "install-secret",
        );

        assert_eq!(fingerprint.scheme, "pom-fingerprint-v1");
        assert!(fingerprint.hash.starts_with("sha256:"));
        assert!(!fingerprint.hash.contains("machine-secret"));
        assert!(!fingerprint.hash.contains("board-secret"));
        assert!(!fingerprint.hash.contains("install-secret"));
        assert_eq!(
            fingerprint,
            Fingerprint::from_components(
                "linux",
                "x86_64",
                "node-a",
                "machine-secret",
                "board-secret",
                "install-secret",
            )
        );
    }

    #[test]
    fn request_envelope_contains_the_protocol_and_client_metadata() {
        let envelope = RequestEnvelope::new(
            "request-1",
            "activate:key:fingerprint",
            ClientInfo {
                product: "pom".into(),
                version: "0.1.0".into(),
                os: "macos".into(),
                arch: "aarch64".into(),
            },
            serde_json::json!({"license_key": "PM-test"}),
        );

        assert_eq!(envelope.protocol, LICENSE_PROTOCOL);
        assert_eq!(envelope.idempotency_key, "activate:key:fingerprint");
        assert_eq!(envelope.client.product, "pom");
        assert_eq!(envelope.payload["license_key"], "PM-test");
    }
}
