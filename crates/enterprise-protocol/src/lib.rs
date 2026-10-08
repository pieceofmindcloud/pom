use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod abi;

pub const LICENSE_PROTOCOL: &str = "pom-license/v1";
pub const IPC_PROTOCOL: &str = "pom-enterprise-ipc/v1";
/// Maximum encoded JSON frame size for the local `pom` <-> `pom-plugin-host` IPC.
pub const MAX_IPC_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Where a person goes to buy or manage a plugin license. Not specific to any
/// one plugin_code - it is the storefront for the whole catalog.
pub const PLUGIN_MANAGE_URL: &str = "https://pieceofmind.cloud/plugins/";
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
    UpdateUnavailable,
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
    pub version: String,
    pub channel: String,
    pub feature_set: Vec<String>,
    pub os: String,
    pub arch: String,
    pub min_host_version: String,
    pub plugin_abi: u32,
    pub sha256: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changelog: Option<String>,
    /// Optional, typed plugin-local preference definitions. Values configured
    /// by the operator are stored by POM locally and are never license data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferences: Option<PluginPreferencesSchema>,
    pub key_id: String,
    pub signature: String,
}

/// Preference definitions shipped with a plugin manifest. This is a UI/config
/// schema only; it does not grant features or change license entitlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPreferencesSchema {
    #[serde(default)]
    pub features: Vec<PluginBooleanPreference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_directory: Option<PluginStorageDirectoryPreference>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBooleanPreference {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStorageDirectoryPreference {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// Locally persisted preference values, deliberately separate from license
/// request/response DTOs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPreferences {
    #[serde(default)]
    pub features: std::collections::BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_directory: Option<String>,
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
    /// Which catalog entry this activation is for. A single license may grant
    /// several plugin_codes; the client activates each one it knows about
    /// separately, one request per code.
    pub plugin_code: String,
    pub fingerprint_scheme: String,
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_id_hash: Option<String>,
    pub node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster_id_hash: Option<String>,
    pub requested_features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidatePayload {
    pub license_key: String,
    pub requested_features: Vec<String>,
    pub fingerprint_scheme: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidateResponse {
    pub license_id: String,
    pub status: String,
    /// Os `plugin_code` que esta licença concede hoje. O servidor de licenças
    /// é um catálogo de plugins: a licença concede uma LISTA de códigos, e as
    /// features de cada plugin vêm do manifesto do release instalado
    /// (`PluginManifest::feature_set`), não da licença.
    pub licensed_plugins: Vec<String>,
    pub activation_required: bool,
    pub license_expires_at: Option<String>,
    pub activation_count: Option<u32>,
    pub max_activations: Option<u32>,
    /// Per-plugin active activation counts for this license.
    #[serde(default)]
    pub plugin_usage: Vec<PluginUsage>,
    /// Products and eligible releases this license can activate on this node.
    #[serde(default)]
    pub activation_candidates: Vec<ActivationCandidate>,
    pub activation_id: Option<String>,
    pub eligible_release: Option<EligibleRelease>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EligibleRelease {
    pub release_id: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationCandidate {
    pub plugin_code: String,
    pub display_name: String,
    pub release_id: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUsage {
    pub plugin_code: String,
    pub activation_count: u32,
    pub max_activations: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadGrant {
    pub token: String,
    pub url: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivationResponse {
    pub license_id: String,
    pub plugin_code: String,
    pub activation_id: String,
    pub activation_token: String,
    pub lease_id: String,
    pub lease_token: String,
    /// Os `plugin_code` que a licença concede, como em [`ValidateResponse`].
    pub licensed_plugins: Vec<String>,
    pub license_expires_at: Option<String>,
    pub activation_count: Option<u32>,
    pub max_activations: Option<u32>,
    /// Per-plugin active activation counts for this license.
    #[serde(default)]
    pub plugin_usage: Vec<PluginUsage>,
    /// Products and eligible releases this license can activate on this node.
    #[serde(default)]
    pub activation_candidates: Vec<ActivationCandidate>,
    pub lease_expires_at: String,
    pub heartbeat_due_at: String,
    pub grace_seconds: u64,
    pub plugin_release_id: String,
    pub plugin_manifest: PluginManifest,
    pub grant: Grant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestResponse {
    pub manifest: PluginManifest,
    pub download: DownloadGrant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantBody {
    pub v: u32,
    pub iss: String,
    pub product: String,
    pub plugin_code: String,
    pub license_id: String,
    pub activation_id: String,
    pub lease_id: String,
    pub jti: String,
    pub node_id: String,
    pub fingerprint_scheme: String,
    pub fingerprint_hash: String,
    pub cluster_id_hash: Option<String>,
    pub release_id: String,
    pub iat: String,
    pub nbf: String,
    pub exp: String,
    pub heartbeat_due_at: String,
    pub grace_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseStatusResponse {
    pub state: String,
    pub license_id: String,
    pub activation_id: String,
    pub lease_id: String,
    /// Os `plugin_code` que a licença concede, como em [`ValidateResponse`].
    pub licensed_plugins: Vec<String>,
    pub license_expires_at: Option<String>,
    pub activation_count: Option<u32>,
    pub max_activations: Option<u32>,
    /// Per-plugin active activation counts for this license.
    #[serde(default)]
    pub plugin_usage: Vec<PluginUsage>,
    /// Products and eligible releases this license can activate on this node.
    #[serde(default)]
    pub activation_candidates: Vec<ActivationCandidate>,
    pub lease_expires_at: String,
    pub grace_ends_at: String,
    pub plugin_release_id: String,
    pub eligible_release_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatPayload {
    pub activation_id: String,
    pub lease_id: String,
    pub fingerprint_scheme: String,
    pub fingerprint: String,
    pub node_id: String,
    pub nonce: String,
    pub client_time: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeactivatePayload {
    pub fingerprint_scheme: String,
    pub fingerprint: String,
    pub reason: String,
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

    #[test]
    fn activation_payload_matches_the_published_server_wire_contract() {
        let payload = ActivationPayload {
            license_key: "PM-test".into(),
            plugin_code: "logs".into(),
            fingerprint_scheme: FINGERPRINT_SCHEME.into(),
            fingerprint: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            installation_id_hash: Some(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            ),
            node_id: "node-a".into(),
            cluster_id_hash: None,
            requested_features: vec!["request_logs".into()],
        };

        assert_eq!(
            serde_json::to_value(payload).expect("activation payload JSON"),
            serde_json::json!({
                "license_key": "PM-test",
                "plugin_code": "logs",
                "fingerprint_scheme": "pom-fingerprint-v1",
                "fingerprint": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "installation_id_hash": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "node_id": "node-a",
                "requested_features": ["request_logs"],
            })
        );
    }

    #[test]
    fn license_status_preserves_per_license_usage_and_activation_candidates() {
        let status: LicenseStatusResponse = serde_json::from_value(serde_json::json!({
            "state": "active",
            "license_id": "lic-123",
            "activation_id": "act-456",
            "lease_id": "lease-789",
            "licensed_plugins": ["analytics_plus", "logs"],
            "license_expires_at": "2099-11-30T12:00:00Z",
            "activation_count": 3,
            "max_activations": 10,
            "plugin_usage": [{
                "plugin_code": "analytics_plus",
                "activation_count": 2,
                "max_activations": 5
            }],
            "activation_candidates": [{
                "plugin_code": "new_plugin",
                "display_name": "New Plugin",
                "release_id": "new-1",
                "version": "1.0.0"
            }],
            "lease_expires_at": "2099-12-01T12:00:00Z",
            "grace_ends_at": "2099-12-03T12:00:00Z",
            "plugin_release_id": "analytics-plus-1",
            "eligible_release_id": null
        }))
        .expect("published license status payload");

        assert_eq!(status.licensed_plugins, ["analytics_plus", "logs"]);
        assert_eq!(
            status.license_expires_at.as_deref(),
            Some("2099-11-30T12:00:00Z")
        );
        assert_eq!(status.activation_count, Some(3));
        assert_eq!(status.max_activations, Some(10));
        assert_eq!(status.plugin_usage[0].plugin_code, "analytics_plus");
        assert_eq!(status.plugin_usage[0].activation_count, 2);
        assert_eq!(status.plugin_usage[0].max_activations, 5);
        assert_eq!(status.activation_candidates[0].plugin_code, "new_plugin");
        assert_eq!(status.activation_candidates[0].release_id, "new-1");
    }
}
