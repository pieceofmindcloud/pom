//! Generic manager of independent plugins.
//!
//! Until the captain's decision (2026-09-18), POM had a single "Enterprise
//! license" for a single optional product. That model became N independent
//! plugins, each with its own `plugin_code`, activation cycle, heartbeat and
//! process host. The license server supplies codes for licensed installations;
//! a GitHub repository's `manifest.json` can introduce a code for a local
//! installation without a license. [`PluginManager`] creates a [`PluginRuntime`]
//! per code when needed. Each runtime has its own activation state, lease, host
//! and version update check, independent of the others.
//!
//! Updating the `pom` binary itself (`crate::pom_core_update`) is deliberately
//! NOT part of this manager: it is another product boundary (it will become
//! public GitHub Releases, with no license at all) and only borrows, under the
//! hood, the lease of the `"logs"` plugin until that transition mechanism
//! changes. See `crate::pom_core_update`.
//!
//! **License server contract** (confirmed against the real server, and pinned by
//! `enterprise-protocol/tests/wire_contract.rs` with response bodies captured
//! from it): one key grants several `plugin_code`s at once, and every response
//! lists those codes in `licensed_plugins` - plain codes, with no features.
//! **A plugin's features come from the installed release's manifest**
//! (`PluginManifest::feature_set`, in the format `"<plugin_code>.<feature>"`),
//! because what a version delivers is defined by that version, not by the
//! license. Activation (`POST /v1/licenses/activations`) is a call PER
//! `plugin_code`, and the manifest/download endpoints were already generic per
//! `plugin_code` (`/v1/plugins/:plugin_code/manifest`).
//!
//! This client once assumed the opposite - that the license listed features in
//! `entitlements` - and plugin activation broke in production with
//! `missing field \`entitlements\`` when the server became a plugin catalog.
//! Do not go back to deriving features from the license.

use async_trait::async_trait;
use axum::response::IntoResponse;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use enterprise_protocol::{
    ActivationPayload, ActivationResponse, ClientInfo, DownloadGrant, Fingerprint, Grant,
    HeartbeatPayload, LicenseErrorCode, LicenseState, ManifestResponse, RequestEnvelope,
    ResponseEnvelope,
};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, RwLock as StdRwLock};
use tokio::sync::{Mutex, RwLock};

/// The Request Statistics/Logs plugin. It is named because `crate::reqlog`,
/// `crate::backend::gateway::generation` and `crate::pom_core_update` need a
/// concrete `plugin_code`, not the dynamic code of another instance.
pub(crate) const LOGS_PLUGIN_CODE: &str = "logs";

/// Pauses between the attempts to hand an audit event to its plugin (one more attempt follows the last).
const AUDIT_RETRY_PAUSES: [std::time::Duration; 3] = [
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(3),
    std::time::Duration::from_secs(8),
];

const PERSISTED_SETTING_PREFIX: &str = "plugin_license:";
const PUBLIC_INSTALL_SETTING_PREFIX: &str = "plugin_public_install:";
const DEVELOPMENT_INSTALL_SETTING_PREFIX: &str = "plugin_dev_install:";
const GITHUB_REPOSITORIES_SETTING: &str = "plugin_github_repositories";
const PREFERENCES_SETTING_PREFIX: &str = "plugin_preferences:";
const UI_MANIFEST_SETTING_PREFIX: &str = "plugin_ui_manifest:";
/// `plugin_ui_asset:<code>:<path>`: the last copy of an asset the shell needs
/// while the plugin's process is down (its i18n catalogs and menu icon), as the
/// host answered `ui.asset`. See `PluginRuntime::shell_asset`.
const UI_ASSET_SETTING_PREFIX: &str = "plugin_ui_asset:";

/// Plugin codes come from the license service, from GitHub releases or from
/// manifests of local sources. They also make up URL segments and directories;
/// we accept generic slugs, but never separators or path segments.
pub(crate) fn is_valid_plugin_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Reads `lifecycle: "on_demand"` from an already validated and saved UI manifest.
fn manifest_is_on_demand(raw: Option<&serde_json::Value>) -> bool {
    raw.and_then(|manifest| manifest.get("lifecycle"))
        .and_then(|lifecycle| {
            serde_json::from_value::<crate::plugin_ui::Lifecycle>(lifecycle.clone()).ok()
        })
        .is_some_and(crate::plugin_ui::Lifecycle::is_on_demand)
}

#[derive(Debug, Clone)]
pub(crate) struct LicenseClientError {
    code: LicenseErrorCode,
    message: String,
}

impl LicenseClientError {
    pub(crate) fn new(code: LicenseErrorCode, message: impl Into<String>) -> Self {
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
    pub licensed_plugins: Vec<String>,
    pub license_expires_at: Option<String>,
    pub activation_count: Option<u32>,
    pub max_activations: Option<u32>,
    #[serde(default)]
    pub plugin_usage: Vec<enterprise_protocol::PluginUsage>,
    #[serde(default)]
    pub activation_candidates: Vec<enterprise_protocol::ActivationCandidate>,
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
            "license validation not available",
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
        _persisted: &PersistedPluginState,
        _fingerprint: &Fingerprint,
    ) -> Result<ResponseEnvelope<enterprise_protocol::LicenseStatusResponse>, LicenseClientError>
    {
        Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "license status not available",
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
            "plugin manifest not available",
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
            "plugin download not available",
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
            "license deactivation not available",
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
        .expect("license base URL must be valid");
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
                LicenseClientError::new(LicenseErrorCode::ServerError, "invalid license grant")
            })?;
        if grant.product != "pom"
            || grant.iss != "license.pieceofmind.cloud"
            || grant.plugin_code != response.plugin_code
            || grant.activation_id != response.activation_id
            || grant.lease_id != response.lease_id
            || grant.node_id.is_empty()
            || grant.fingerprint_scheme != enterprise_protocol::FINGERPRINT_SCHEME
            || !grant.fingerprint_hash.starts_with("sha256:")
            || grant.release_id != response.plugin_release_id
        {
            return Err(LicenseClientError::new(
                LicenseErrorCode::ServerError,
                "license grant does not match the activation",
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
                    "plugin manifest without a signature",
                )
            })?
            .to_owned();
        let mut unsigned = value;
        unsigned
            .as_object_mut()
            .expect("manifest serializes as an object")
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
        persisted: &PersistedPluginState,
        fingerprint: &Fingerprint,
    ) -> Result<ResponseEnvelope<enterprise_protocol::LicenseStatusResponse>, LicenseClientError>
    {
        let mut url = reqwest::Url::parse(&format!("{}/v1/licenses/status", self.base_url))
            .expect("license base URL must be valid");
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
            format!("malformed license {kind}"),
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
                format!("unknown {kind} public key"),
            )
        })?;
    if key.alg != "Ed25519" {
        return Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("unsupported {kind} algorithm"),
        ));
    }
    let raw_key = URL_SAFE_NO_PAD.decode(&key.public_key).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("invalid {kind} public key"),
        )
    })?;
    let raw_key: [u8; 32] = raw_key.try_into().map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("invalid {kind} public key size"),
        )
    })?;
    let verifying_key = VerifyingKey::from_bytes(&raw_key).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("invalid {kind} public key"),
        )
    })?;
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("invalid {kind} signature"),
        )
    })?;
    let signature = Signature::from_slice(&signature).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("invalid {kind} signature size"),
        )
    })?;
    verifying_key.verify(bytes, &signature).map_err(|_| {
        LicenseClientError::new(
            LicenseErrorCode::ServerError,
            format!("{kind} signature does not match"),
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
        .unwrap_or("The license server is unavailable.");
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
        // The server only answers ACTIVATION_UNKNOWN on deactivate: it does not know this
        // activation.
        "LEASE_UNKNOWN" | "ACTIVATION_UNKNOWN" => LicenseErrorCode::LeaseUnknown,
        "RATE_LIMITED" => LicenseErrorCode::RateLimited,
        _ => return None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedPluginState {
    pub license_id: String,
    pub activation_id: String,
    pub activation_token: String,
    pub lease_id: String,
    pub lease_token: String,
    pub fingerprint: Fingerprint,
    pub signed_grant: Grant,
    #[serde(default)]
    pub licensed_plugins: Vec<String>,
    #[serde(default)]
    pub plugin_usage: Vec<enterprise_protocol::PluginUsage>,
    #[serde(default)]
    pub activation_candidates: Vec<enterprise_protocol::ActivationCandidate>,
    #[serde(default)]
    pub license_expires_at: Option<String>,
    #[serde(default)]
    pub activation_count: Option<u32>,
    #[serde(default)]
    pub max_activations: Option<u32>,
    pub lease_expires_at: String,
    pub heartbeat_due_at: String,
    pub grace_seconds: u64,
    pub last_success_at: Option<String>,
    /// Moment the installed release's host became ready. Heartbeats and update
    /// checks do not change this field.
    #[serde(default)]
    pub installed_at: Option<String>,
    /// Last manifest query that completed successfully, whether or not there is a
    /// new release. A query that fails preserves the last good one.
    #[serde(default)]
    pub update_checked_at: Option<String>,
    /// The release actually downloaded, verified and installed on this machine.
    /// Only a completed install by `activate_and_install`/the final step of
    /// `activate` (with no real download, used only in this module's tests) and
    /// `apply_update` can advance this field - a heartbeat alone never confirms
    /// a download. `#[serde(rename)]` preserves the `plugin_release_id` key
    /// already persisted in existing installations.
    ///
    /// Fixed defect (2026-09-21): the heartbeat wrote here the
    /// `plugin_release_id` it itself receives from the server, even when no
    /// download had happened. Since `detect_update` compares the manifest's
    /// `release_id` against exactly this field, clicking "Update" (which calls
    /// heartbeat and then `detect_update` in the same action) corrupted its own
    /// comparison baseline and always concluded "no update" - unrelated to the
    /// release's semver format (`+build.N` works normally in the string
    /// comparison; the problem was the write order, not the format). See
    /// `docs/subsystems/enterprise-licensing.md`.
    #[serde(rename = "plugin_release_id")]
    pub installed_release_id: String,
    pub plugin_version: String,
    pub features: Vec<String>,
    pub plugin_sha256: String,
    pub plugin_path: Option<String>,
    #[serde(default)]
    pub preferences_schema: Option<enterprise_protocol::PluginPreferencesSchema>,
    /// Last installation/handshake failure, without changing the license state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_error: Option<String>,
}

/// State for a public GitHub plugin installed without a license lease. Stored
/// separately from `plugin_license:<code>` so free distribution never becomes
/// a synthetic license or entitlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PublicPluginInstallState {
    fingerprint: Fingerprint,
    #[serde(default)]
    repository_id: Option<String>,
    #[serde(default)]
    repository_url: Option<String>,
    installed_release_id: String,
    plugin_version: String,
    features: Vec<String>,
    plugin_sha256: String,
    size: u64,
    plugin_path: String,
    preferences_schema: Option<enterprise_protocol::PluginPreferencesSchema>,
    installed_at: String,
}

/// Local development artifact installed from a mounted source. Kept separate
/// from license and GitHub state: it starts a host but grants no features.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DevelopmentPluginInstallState {
    fingerprint: Fingerprint,
    plugin_sha256: String,
    size: u64,
    plugin_path: String,
    installed_at: String,
}

/// A version update available for installation - of the plugin itself, never of
/// `pom_core` (see `crate::pom_core_update::CoreUpdate`, the deliberately
/// separate sibling type).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PluginUpdateInfo {
    pub release_id: String,
    pub version: String,
    pub changelog: Option<String>,
}

/// The stable contract the Plugins screen reads. One entry per catalog
/// `plugin_code`, even when the operator never activated that plugin - that is
/// how the screen shows "available for purchase" next to the already active
/// ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PluginStatus {
    pub code: String,
    /// `true` only when the plugin's host was installed and is ready.
    pub active: bool,
    pub state: LicenseState,
    pub features: Vec<String>,
    pub manage_url: String,
    pub plugin_version: Option<String>,
    pub lease_expires_at: Option<String>,
    /// Validity of the LICENSE, distinct from the technical `lease_expires_at`
    /// above (the lease renews on every heartbeat and usually expires in days;
    /// the purchased license usually expires in months or years). Read from
    /// `LicenseStatusResponse::license_expires_at` (`GET /v1/licenses/status`),
    /// never from the lease - showing the lease as if it were the license's
    /// validity is the confusion this field exists to avoid.
    pub license_expires_at: Option<String>,
    pub activation_count: Option<u32>,
    pub max_activations: Option<u32>,
    pub licensed_plugins: Vec<String>,
    pub plugin_usage: Vec<enterprise_protocol::PluginUsage>,
    pub activation_candidates: Vec<enterprise_protocol::ActivationCandidate>,
    pub installed_at: Option<String>,
    pub update_checked_at: Option<String>,
    pub available_update: Option<PluginUpdateInfo>,
    /// Installation failure shown separately from the license validity state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_error: Option<String>,
    #[serde(default)]
    pub public_install: bool,
    /// True when the running artifact was built from a local development source.
    #[serde(default)]
    pub development_install: bool,
    /// URL of the local GitHub source used by the public installation, if installed.
    #[serde(default)]
    pub github_repository: Option<String>,
    #[serde(default)]
    pub preferences_schema: Option<enterprise_protocol::PluginPreferencesSchema>,
    #[serde(default)]
    pub preferences: Option<enterprise_protocol::PluginPreferences>,
    /// On-demand plugin: the manifest declares `lifecycle: "on_demand"`.
    #[serde(default)]
    pub on_demand: bool,
    /// `true` when the host process is running. An on-demand plugin can
    /// be `active` (installed and licensed) with `running` false.
    #[serde(default)]
    pub running: bool,
}

impl PluginStatus {
    fn inactive(code: &str) -> Self {
        Self {
            code: code.to_owned(),
            active: false,
            state: LicenseState::NeedsActivation,
            features: Vec::new(),
            manage_url: enterprise_protocol::PLUGIN_MANAGE_URL.to_owned(),
            plugin_version: None,
            lease_expires_at: None,
            license_expires_at: None,
            activation_count: None,
            max_activations: None,
            licensed_plugins: Vec::new(),
            plugin_usage: Vec::new(),
            activation_candidates: Vec::new(),
            installed_at: None,
            update_checked_at: None,
            available_update: None,
            installation_error: None,
            public_install: false,
            development_install: false,
            github_repository: None,
            preferences_schema: None,
            preferences: None,
            on_demand: false,
            running: false,
        }
    }

    pub(crate) fn has_feature(&self, feature: &str) -> bool {
        self.active && self.features.iter().any(|value| value == feature)
    }
}

#[derive(Debug, Clone)]
struct PendingActivation {
    response: ActivationResponse,
    fingerprint: Fingerprint,
}

/// State and lifecycle of ONE plugin (`plugin_code`). Each instance is
/// self-contained: its own activation, lease, heartbeat, process host and
/// version update check, independent of any other plugin. `PluginManager` keeps
/// one per catalog entry.
pub(crate) struct PluginRuntime {
    code: String,
    service: Arc<dyn LicenseService>,
    fingerprint: Fingerprint,
    status: Arc<RwLock<PluginStatus>>,
    pending: Arc<Mutex<Option<PendingActivation>>>,
    persisted: Arc<Mutex<Option<PersistedPluginState>>>,
    database: Option<Arc<crate::db::PomDb>>,
    host: Arc<Mutex<Option<crate::plugin_host::PluginHost>>>,
    /// Explicitly models a ready host in tests that do not spawn a process.
    #[cfg(test)]
    mock_host_ready: std::sync::atomic::AtomicBool,
    update_lock: Arc<Mutex<()>>,
    public_install: Arc<Mutex<Option<PublicPluginInstallState>>>,
    development_install: Arc<Mutex<Option<DevelopmentPluginInstallState>>>,
    /// Context the node hands to every plugin host (`host.configure`), shared
    /// with the [`PluginManager`]. `None` until the gateway is ready.
    host_context: HostContext,
    /// Last `ui.manifest` the host returned, raw and already validated. Saved
    /// in the database so the menu of an on-demand plugin keeps existing
    /// while its process is stopped (the manifest can only be requested from a running host).
    ui_manifest_raw: Arc<Mutex<Option<serde_json::Value>>>,
    /// The shell assets of the current manifest were copied from the host
    /// already; reset when the manifest changes.
    shell_assets_kept: std::sync::atomic::AtomicBool,
    /// `lifecycle: "on_demand"` of the last known manifest. It comes from the cache, so
    /// it applies at boot, before any process starts.
    on_demand: std::sync::atomic::AtomicBool,
    /// This plugin's secret for the node's loopback API (`crate::plugin_host_api`),
    /// new at every start of the node and handed over in `host.configure`.
    host_api_token: String,
}

/// What the node hands to each plugin host through the `host.configure` operation.
type HostContext = Arc<StdRwLock<Option<serde_json::Value>>>;

impl PluginRuntime {
    fn new(
        code: &str,
        service: Arc<dyn LicenseService>,
        fingerprint: Fingerprint,
        database: Option<Arc<crate::db::PomDb>>,
        host_context: HostContext,
    ) -> Self {
        let public_install = database
            .as_ref()
            .and_then(|database| {
                database
                    .local_setting_json(&format!("{PUBLIC_INSTALL_SETTING_PREFIX}{code}"))
                    .ok()
                    .flatten()
            })
            .and_then(|value| serde_json::from_str(&value).ok());
        let development_install = database
            .as_ref()
            .and_then(|database| {
                database
                    .local_setting_json(&format!("{DEVELOPMENT_INSTALL_SETTING_PREFIX}{code}"))
                    .ok()
                    .flatten()
            })
            .and_then(|value| serde_json::from_str(&value).ok());
        let ui_manifest_raw = database
            .as_ref()
            .and_then(|database| {
                database
                    .local_setting_json(&format!("{UI_MANIFEST_SETTING_PREFIX}{code}"))
                    .ok()
                    .flatten()
            })
            .and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok());
        let on_demand = manifest_is_on_demand(ui_manifest_raw.as_ref());
        Self {
            code: code.to_owned(),
            service,
            fingerprint,
            status: Arc::new(RwLock::new(PluginStatus::inactive(code))),
            pending: Arc::new(Mutex::new(None)),
            persisted: Arc::new(Mutex::new(None)),
            database,
            host: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            mock_host_ready: std::sync::atomic::AtomicBool::new(false),
            update_lock: Arc::new(Mutex::new(())),
            public_install: Arc::new(Mutex::new(public_install)),
            development_install: Arc::new(Mutex::new(development_install)),
            host_context,
            ui_manifest_raw: Arc::new(Mutex::new(ui_manifest_raw)),
            shell_assets_kept: std::sync::atomic::AtomicBool::new(false),
            on_demand: std::sync::atomic::AtomicBool::new(on_demand),
            host_api_token: hex::encode(rand::random::<[u8; 32]>()),
        }
    }

    /// `true` when the plugin manifest declares `lifecycle: "on_demand"`.
    fn on_demand(&self) -> bool {
        self.on_demand.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Starts the plugin host and already hands it the node context, if any.
    fn plugin_database_path(&self) -> std::path::PathBuf {
        let database_path = self.database.as_ref().map(|database| database.path());
        plugin_database_path_for(&self.code, database_path)
    }

    async fn start_host(
        &self,
        path: &std::path::Path,
    ) -> anyhow::Result<crate::plugin_host::PluginHost> {
        let database_path = self.plugin_database_path();
        let host = crate::plugin_host::PluginHost::start(&self.code, path, &database_path).await?;
        self.configure_host(&host).await;
        Ok(host)
    }

    /// `host.configure`: the local OpenAI endpoint and the API key the plugin
    /// uses to call the node's models. A plugin that does not know the operation
    /// answers with an error, and that is not a failure - it simply does not need
    /// it.
    async fn configure_host(&self, host: &crate::plugin_host::PluginHost) {
        let context = self
            .host_context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(mut payload) = context else {
            return;
        };
        if let Some(object) = payload.as_object_mut() {
            object.insert("operation".into(), "host.configure".into());
            // The shared context names where the node's API is; the secret is this plugin's own.
            if let Some(base_url) = object.remove("host_api_base_url") {
                object.insert(
                    "host_api".into(),
                    serde_json::json!({"base_url": base_url, "token": self.host_api_token}),
                );
            }
            let licensed_schema = self
                .persisted
                .lock()
                .await
                .as_ref()
                .and_then(|state| state.preferences_schema.clone());
            let schema = match licensed_schema {
                Some(schema) => Some(schema),
                None => self
                    .public_install
                    .lock()
                    .await
                    .as_ref()
                    .and_then(|state| state.preferences_schema.clone()),
            };
            if let Some(schema) = schema {
                object.insert(
                    "preferences".into(),
                    serde_json::to_value(self.preferences_for(&schema)).unwrap_or_default(),
                );
            }
        }
        if let Err(error) = host.query(payload).await {
            tracing::debug!(plugin_code = %self.code, %error, "plugin did not accept host.configure");
        }
    }

    #[cfg(test)]
    async fn stop_host_for_test(&self) {
        if let Some(host) = self.host.lock().await.take() {
            let _ = host.stop().await;
        }
    }

    /// Hands the context to the running host again, after the node changed it.
    async fn reconfigure_running_host(&self) {
        let host = self.host.lock().await.clone();
        if let Some(host) = host {
            self.configure_host(&host).await;
        }
    }

    fn setting_key(&self) -> String {
        format!("{PERSISTED_SETTING_PREFIX}{}", self.code)
    }

    fn preferences_key(&self) -> String {
        format!("{PREFERENCES_SETTING_PREFIX}{}", self.code)
    }

    fn public_install_key(&self) -> String {
        format!("{PUBLIC_INSTALL_SETTING_PREFIX}{}", self.code)
    }

    fn development_install_key(&self) -> String {
        format!("{DEVELOPMENT_INSTALL_SETTING_PREFIX}{}", self.code)
    }

    fn preferences_for(
        &self,
        schema: &enterprise_protocol::PluginPreferencesSchema,
    ) -> enterprise_protocol::PluginPreferences {
        let mut preferences = default_preferences(schema);
        if let Some(database) = &self.database {
            if let Ok(Some(json)) = database.local_setting_json(&self.preferences_key()) {
                if let Ok(saved) =
                    serde_json::from_str::<enterprise_protocol::PluginPreferences>(&json)
                {
                    if validate_preferences(schema, &saved).is_ok() {
                        preferences = merge_preferences(schema, saved);
                    }
                }
            }
        }
        preferences
    }

    pub(crate) async fn status(&self) -> PluginStatus {
        let mut status = self.status.read().await.clone();
        status.on_demand = self.on_demand();
        status.running = self.host.lock().await.is_some();
        let public_install = self
            .public_install
            .lock()
            .await
            .clone()
            .filter(|installed| installed.fingerprint == self.fingerprint);
        let development_install = self
            .development_install
            .lock()
            .await
            .clone()
            .filter(|installed| installed.fingerprint == self.fingerprint);
        status.development_install = development_install.is_some();
        status.github_repository = public_install
            .as_ref()
            .and_then(|installed| installed.repository_url.clone());
        if let Some(installed_version) = status
            .plugin_version
            .as_deref()
            .and_then(|version| semver::Version::parse(version).ok())
        {
            if status
                .available_update
                .as_ref()
                .and_then(|update| semver::Version::parse(&update.version).ok())
                .is_some_and(|version| version <= installed_version)
            {
                status.available_update = None;
            }
        }
        if let Some(persisted) = self.persisted.lock().await.as_ref() {
            if let Some(schema) = &persisted.preferences_schema {
                status.preferences_schema = Some(schema.clone());
                status.preferences = Some(self.preferences_for(schema));
            }
        } else if let Some(installed) = public_install.as_ref() {
            status.public_install = true;
            status.plugin_version = Some(installed.plugin_version.clone());
            status.features = installed.features.clone();
            status.installed_at = Some(installed.installed_at.clone());
            if let Some(schema) = &installed.preferences_schema {
                status.preferences_schema = Some(schema.clone());
                status.preferences = Some(self.preferences_for(schema));
            }
        }
        status
    }

    pub(crate) async fn persisted_state(&self) -> Option<PersistedPluginState> {
        self.persisted.lock().await.clone()
    }

    #[cfg(test)]
    async fn persisted_with_semver(
        &self,
    ) -> Result<(PersistedPluginState, semver::Version), LicenseClientError> {
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Local activation missing.")
        })?;
        let version = semver::Version::parse(&persisted.plugin_version)
            .map_err(|_| update_unavailable("installed plugin version is not valid SemVer"))?;
        Ok((persisted, version))
    }

    async fn save_preferences(
        &self,
        values: enterprise_protocol::PluginPreferences,
    ) -> Result<(), LicenseClientError> {
        if !self.status.read().await.active {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "Activate the plugin before editing its preferences.",
            ));
        }
        let licensed_schema = self
            .persisted
            .lock()
            .await
            .as_ref()
            .and_then(|persisted| persisted.preferences_schema.clone());
        let schema = match licensed_schema {
            Some(schema) => schema,
            None => self
                .public_install
                .lock()
                .await
                .as_ref()
                .and_then(|installed| installed.preferences_schema.clone())
                .ok_or_else(|| update_unavailable("this release declares no preferences"))?,
        };
        prepare_storage_directory(&schema, &values).map_err(|message| {
            LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, message)
        })?;
        validate_preferences(&schema, &values).map_err(|message| {
            LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, message)
        })?;
        let values = merge_preferences(&schema, values);
        if let Some(database) = &self.database {
            database
                .save_local_setting(&self.preferences_key(), &values)
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        self.reconfigure_running_host().await;
        self.send_host_event(plugin_event_envelope(
            "preferences.changed",
            &self.code,
            serde_json::json!({ "preferences": values }),
        ))
        .await;
        Ok(())
    }

    /// Delivers a `pom-plugin-events/v1` event to the plugin's native backend
    /// (`host.event` operation). A plugin that does not know the operation
    /// answers with an error, which is ignored as in `host.configure`.
    pub(crate) async fn send_host_event(&self, envelope: serde_json::Value) {
        let host = self.host.lock().await.clone();
        let Some(host) = host else {
            return;
        };
        let request = serde_json::json!({ "operation": "host.event", "event": envelope });
        if let Err(error) = host.query(request).await {
            tracing::debug!(plugin_code = %self.code, %error, "plugin did not accept host.event");
        }
    }

    /// Restores the persisted state (if any) and, if the plugin already had an
    /// installed binary and the license is still valid (online or within the
    /// offline grace), brings the process host back up. Runs once when the
    /// runtime is constructed.
    async fn restore(self: &Arc<Self>) {
        let Some(persisted) = self
            .database
            .as_ref()
            .and_then(|database| {
                database
                    .local_setting_json(&self.setting_key())
                    .ok()
                    .flatten()
            })
            .and_then(|json| serde_json::from_str::<PersistedPluginState>(&json).ok())
            .filter(|state| state.fingerprint == self.fingerprint)
        else {
            self.restore_public_install().await;
            if !self.status.read().await.active {
                self.restore_development_install().await;
            }
            return;
        };
        *self.persisted.lock().await = Some(persisted.clone());
        let restore_error = persisted.installation_error.clone().unwrap_or_else(|| {
            "The plugin has no installed binary. Reactivate it to repeat the installation.".into()
        });
        let Some(path) = persisted.plugin_path.clone().map(std::path::PathBuf::from) else {
            let mut status = self.status.write().await;
            status.state = LicenseState::Unavailable;
            status.licensed_plugins = persisted.licensed_plugins.clone();
            status.license_expires_at = persisted.license_expires_at.clone();
            status.installation_error = Some(restore_error);
            return;
        };
        if !path.is_file() || self.database.is_none() {
            let mut status = self.status.write().await;
            status.state = LicenseState::Unavailable;
            status.licensed_plugins = persisted.licensed_plugins.clone();
            status.license_expires_at = persisted.license_expires_at.clone();
            status.installation_error = Some(if path.is_file() {
                "The local POM database is not available to start the plugin host.".into()
            } else {
                "The installed plugin binary is missing. Reactivate it to download it again.".into()
            });
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let remote_state = this.service.status(&persisted, &this.fingerprint).await;
            let mut restored = persisted.clone();
            let allowed = match remote_state {
                Ok(remote) => {
                    let status = remote.payload;
                    let usage = plugin_usage_for_code(&this.code, &status.plugin_usage)
                        .map(|entry| (entry.activation_count, entry.max_activations));
                    let allowed = status.activation_id == persisted.activation_id
                        && status.lease_id == persisted.lease_id
                        && status
                            .licensed_plugins
                            .iter()
                            .any(|code| code == &this.code)
                        && matches!(status.state.as_str(), "active" | "grace");
                    if allowed {
                        restored.licensed_plugins = status.licensed_plugins;
                        restored.plugin_usage = status.plugin_usage;
                        restored.activation_candidates = status.activation_candidates;
                        restored.license_expires_at = status.license_expires_at;
                        restored.activation_count = usage.map(|(count, _)| count);
                        restored.max_activations = usage.map(|(_, maximum)| maximum);
                        restored.lease_expires_at = status.lease_expires_at;
                        if let Some(database) = &this.database {
                            let _ = database.save_local_setting(&this.setting_key(), &restored);
                        }
                    }
                    allowed
                }
                Err(_) => lease_allows_offline(&persisted),
            };
            if !allowed {
                {
                    let mut status = this.status.write().await;
                    status.active = false;
                    status.state = LicenseState::Expired;
                }
                this.restore_development_install().await;
                return;
            }
            *this.persisted.lock().await = Some(restored.clone());
            this.ensure_host_running(&restored).await;
            this.restore_ready(&restored).await;
        });
    }

    async fn restore_public_install(&self) {
        let _guard = self.update_lock.lock().await;
        if self.status.read().await.active {
            return;
        }
        let Some(installed) = self.public_install.lock().await.clone() else {
            return;
        };
        if installed.fingerprint != self.fingerprint || self.database.is_none() {
            return;
        }
        let path = std::path::Path::new(&installed.plugin_path);
        let Ok(bytes) = std::fs::read(path) else {
            tracing::warn!(plugin_code = %self.code, "installed public plugin binary is missing");
            if let Some(database) = &self.database {
                let _ = database.delete_local_setting(&self.public_install_key());
            }
            *self.public_install.lock().await = None;
            return;
        };
        let expected = installed
            .plugin_sha256
            .strip_prefix("sha256:")
            .unwrap_or(&installed.plugin_sha256);
        if bytes.len() as u64 != installed.size
            || !hex::encode(sha2::Sha256::digest(&bytes)).eq_ignore_ascii_case(expected)
        {
            tracing::error!(plugin_code = %self.code, "installed public plugin integrity does not match");
            if let Some(database) = &self.database {
                let _ = database.delete_local_setting(&self.public_install_key());
            }
            *self.public_install.lock().await = None;
            let _ = std::fs::remove_file(path);
            return;
        }
        // An on-demand plugin stays installed and active without a process until opened.
        let host = if self.on_demand() {
            Ok(None)
        } else {
            self.start_host(path).await.map(Some)
        };
        match host {
            Ok(host) => {
                *self.host.lock().await = host;
                let mut status = self.status.write().await;
                status.active = true;
                status.state = LicenseState::Active;
                status.public_install = true;
                status.features = installed.features;
                status.plugin_version = Some(installed.plugin_version);
                status.installed_at = Some(installed.installed_at);
                status.licensed_plugins.clear();
                status.lease_expires_at = None;
                status.license_expires_at = None;
                status.activation_count = None;
                status.max_activations = None;
                status.plugin_usage.clear();
                status.activation_candidates.clear();
                status.available_update = None;
                status.installation_error = None;
            }
            Err(error) => {
                tracing::warn!(plugin_code = %self.code, %error, "installed public plugin could not be started");
                let mut status = self.status.write().await;
                status.active = false;
                status.state = LicenseState::Unavailable;
                status.installation_error = Some(error.to_string());
            }
        }
    }

    async fn restore_development_install(&self) {
        let _guard = self.update_lock.lock().await;
        if self.status.read().await.active {
            return;
        }
        let Some(installed) = self.development_install.lock().await.clone() else {
            return;
        };
        if installed.fingerprint != self.fingerprint || self.database.is_none() {
            return;
        }
        let path = std::path::Path::new(&installed.plugin_path);
        let Ok(bytes) = std::fs::read(path) else {
            tracing::warn!(plugin_code = %self.code, "local development binary is missing");
            if let Some(database) = &self.database {
                let _ = database.delete_local_setting(&self.development_install_key());
            }
            *self.development_install.lock().await = None;
            return;
        };
        if bytes.len() as u64 != installed.size
            || !hex::encode(sha2::Sha256::digest(&bytes))
                .eq_ignore_ascii_case(&installed.plugin_sha256)
        {
            tracing::error!(plugin_code = %self.code, "local development plugin integrity does not match");
            if let Some(database) = &self.database {
                let _ = database.delete_local_setting(&self.development_install_key());
            }
            *self.development_install.lock().await = None;
            let _ = std::fs::remove_file(path);
            return;
        }
        let host = if self.on_demand() {
            Ok(None)
        } else {
            self.start_host(path).await.map(Some)
        };
        match host {
            Ok(host) => {
                *self.host.lock().await = host;
                let mut status = self.status.write().await;
                status.active = true;
                status.state = LicenseState::NeedsActivation;
                status.features.clear();
                status.plugin_version = None;
                status.installed_at = Some(installed.installed_at);
                status.development_install = true;
                status.installation_error = None;
            }
            Err(error) => {
                tracing::warn!(plugin_code = %self.code, %error, "local development plugin could not be started");
                let mut status = self.status.write().await;
                status.state = LicenseState::NeedsActivation;
                status.development_install = true;
                status.installation_error = Some(error.to_string());
            }
        }
    }

    async fn restore_ready(&self, persisted: &PersistedPluginState) {
        // On demand, "ready" means installed and licensed: the process starts when opened.
        let host_ready = self.host.lock().await.is_some() || self.on_demand();
        let development_install = self.development_install.lock().await.is_some();
        let mut status = self.status.write().await;
        status.active = host_ready;
        status.development_install = development_install;
        status.state = LicenseState::Active;
        status.features = persisted.features.clone();
        status.plugin_version = Some(persisted.plugin_version.clone());
        status.lease_expires_at = Some(persisted.lease_expires_at.clone());
        status.license_expires_at = persisted.license_expires_at.clone();
        status.activation_count = persisted.activation_count;
        status.max_activations = persisted.max_activations;
        status.licensed_plugins = persisted.licensed_plugins.clone();
        status.plugin_usage = persisted.plugin_usage.clone();
        status.activation_candidates = persisted.activation_candidates.clone();
        status.installed_at = persisted.installed_at.clone();
        status.update_checked_at = persisted.update_checked_at.clone();
        status.available_update = None;
        status.installation_error = if host_ready {
            None
        } else {
            persisted.installation_error.clone().or_else(|| {
                Some("The plugin host did not become ready. Check the error and try again.".into())
            })
        };
    }

    /// Full flow: validates the key for this plugin_code, installs the binary
    /// and starts the host. Single entry point used both in production and in
    /// this module's tests.
    async fn activate_and_install(&self, license_key: &str) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        self.activate(license_key).await?;
        let result = self.install_pending_activation().await;
        if let Err(error) = &result {
            self.record_installation_failure(error.to_string()).await;
        }
        result
    }

    async fn install_pending_activation(&self) -> Result<(), LicenseClientError> {
        let pending = self.pending.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "No activation waiting for the download.",
            )
        })?;
        let manifest = self
            .service
            .manifest(
                &self.code,
                &pending.response.lease_token,
                &pending.response.lease_id,
                &self.fingerprint.platform,
                &self.fingerprint.arch,
            )
            .await?;
        validate_manifest(&self.code, &pending.response, &manifest.manifest)?;
        let bytes = self.service.download(&manifest.download).await?;
        validate_download(&manifest.manifest, &bytes)?;
        let path = plugin_path(
            &self.code,
            &manifest.manifest.release_id,
            &self.fingerprint.platform,
            &self.fingerprint.arch,
        )?;
        install_plugin_atomically(&path, &bytes)?;
        let previous_development = self.development_install.lock().await.clone();
        let old_host = self.host.lock().await.take();
        if let Some(host) = old_host {
            if let Err(error) = host.stop().await {
                *self.host.lock().await = Some(host);
                let _ = std::fs::remove_file(&path);
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    format!("previous local host could not be stopped: {error}"),
                ));
            }
        }
        let previous_path =
            match previous_development.as_ref() {
                Some(previous) => Some(std::path::PathBuf::from(&previous.plugin_path)),
                None => self.persisted.lock().await.as_ref().and_then(|previous| {
                    previous.plugin_path.as_ref().map(std::path::PathBuf::from)
                }),
            };
        let host = match self.start_host(&path).await {
            Ok(host) => host,
            Err(error) => {
                if let Some(previous_path) = previous_path.as_deref().filter(|path| path.is_file())
                {
                    if let Ok(host) = self.start_host(previous_path).await {
                        *self.host.lock().await = Some(host);
                    }
                }
                let _ = std::fs::remove_file(&path);
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    format!("{error:#}"),
                ));
            }
        };
        *self.host.lock().await = Some(host);
        let mut persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Local activation missing.")
        })?;
        persisted.installed_release_id = manifest.manifest.release_id;
        persisted.plugin_version = manifest.manifest.version.clone();
        persisted.plugin_sha256 = manifest.manifest.sha256.clone();
        persisted.plugin_path = Some(path.to_string_lossy().into_owned());
        persisted.preferences_schema = manifest.manifest.preferences.clone();
        persisted.installation_error = None;
        persisted.last_success_at = Some(chrono::Utc::now().to_rfc3339());
        if let Some(database) = &self.database {
            if let Err(error) = database.save_local_setting(&self.setting_key(), &persisted) {
                if let Some(host) = self.host.lock().await.take() {
                    let _ = host.stop().await;
                }
                let _ = std::fs::remove_file(&path);
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    error.to_string(),
                ));
            }
            database
                .delete_local_setting(&self.development_install_key())
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self.development_install.lock().await = None;
        *self.persisted.lock().await = Some(persisted);
        if let Some(previous) =
            previous_development.filter(|previous| previous.plugin_path != path.to_string_lossy())
        {
            let _ = std::fs::remove_file(previous.plugin_path);
        }
        self.mark_plugin_ready(&manifest.manifest.version).await
    }

    async fn record_installation_failure(&self, error: String) {
        if let Some(host) = self.host.lock().await.take() {
            let _ = host.stop().await;
        }
        let mut persisted = self.persisted.lock().await;
        if let Some(state) = persisted.as_mut() {
            state.installation_error = Some(error.clone());
            if let Some(database) = &self.database {
                if let Err(save_error) = database.save_local_setting(&self.setting_key(), state) {
                    tracing::warn!(plugin_code = %self.code, error = %save_error, "plugin installation failure could not be persisted");
                }
            }
        }
        drop(persisted);

        let mut status = self.status.write().await;
        status.active = false;
        status.state = LicenseState::Active;
        status.features.clear();
        status.plugin_version = None;
        status.installed_at = None;
        status.available_update = None;
        status.installation_error = Some(error);
    }

    /// Only the license step (validate + activate + lease persistence), with no
    /// manifest, download or host - deliberately `pub(crate)` so that this
    /// module's tests can set up a "ready" plugin with `mark_plugin_ready`
    /// without depending on the real `pom-plugin-host` binary, exactly as the
    /// integration with that process is tested separately
    /// (`cargo test -p pom-plugin-host`, `-p enterprise-plugin-fixture`).
    pub(crate) async fn activate(&self, license_key: &str) -> Result<(), LicenseClientError> {
        if self.public_install.lock().await.is_some() {
            return Err(update_unavailable(
                "uninstall the public installation before activating a license for this plugin",
            ));
        }
        if license_key.trim().is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::InvalidKey,
                "Enter a license key.",
            ));
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let idempotency_key = format!(
            "activate:{}:{}:{}",
            self.code,
            stable_digest(license_key),
            self.fingerprint.hash
        );
        let request = RequestEnvelope::new(
            request_id,
            idempotency_key,
            client_info(),
            ActivationPayload {
                license_key: license_key.to_owned(),
                plugin_code: self.code.clone(),
                fingerprint_scheme: self.fingerprint.scheme.clone(),
                fingerprint: self.fingerprint.hash.clone(),
                installation_id_hash: Some(self.fingerprint.installation_id_hash.clone()),
                node_id: self.fingerprint.node_id.clone(),
                cluster_id_hash: None,
                requested_features: Vec::new(),
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
                "The activation response does not contain a valid grant or ABI.",
            ));
        }

        let response = response.payload;
        let usage = plugin_usage_for_code(&self.code, &response.plugin_usage);
        let activation_count = usage.map(|entry| entry.activation_count);
        let max_activations = usage.map(|entry| entry.max_activations);
        let features = features_for_code(&self.code, &response.plugin_manifest.feature_set);
        if features.is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "The installed release declares no feature for this plugin.",
            ));
        }
        *self.pending.lock().await = Some(PendingActivation {
            response: response.clone(),
            fingerprint: self.fingerprint.clone(),
        });
        let persisted = PersistedPluginState {
            license_id: response.license_id,
            activation_id: response.activation_id,
            activation_token: response.activation_token,
            lease_id: response.lease_id,
            lease_token: response.lease_token,
            fingerprint: self.fingerprint.clone(),
            signed_grant: response.grant,
            licensed_plugins: response.licensed_plugins.clone(),
            plugin_usage: response.plugin_usage.clone(),
            activation_candidates: response.activation_candidates.clone(),
            license_expires_at: response.license_expires_at.clone(),
            activation_count,
            max_activations,
            lease_expires_at: response.lease_expires_at,
            heartbeat_due_at: response.heartbeat_due_at,
            grace_seconds: response.grace_seconds,
            last_success_at: None,
            installed_at: None,
            update_checked_at: None,
            installed_release_id: response.plugin_release_id,
            plugin_version: response.plugin_manifest.version.clone(),
            features,
            plugin_sha256: response.plugin_manifest.sha256,
            plugin_path: None,
            preferences_schema: response.plugin_manifest.preferences.clone(),
            installation_error: None,
        };
        if let Some(database) = &self.database {
            database
                .save_local_setting(&self.setting_key(), &persisted)
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self.persisted.lock().await = Some(persisted);
        let mut status = self.status.write().await;
        status.active = false;
        status.state = LicenseState::Active;
        status.features = Vec::new();
        status.plugin_version = None;
        status.lease_expires_at = None;
        status.license_expires_at = response.license_expires_at;
        status.activation_count = activation_count;
        status.max_activations = max_activations;
        status.licensed_plugins = response.licensed_plugins;
        status.plugin_usage = response.plugin_usage;
        status.activation_candidates = response.activation_candidates;
        status.installed_at = None;
        status.update_checked_at = None;
        status.available_update = None;
        status.installation_error = None;
        Ok(())
    }

    async fn deactivate(&self) -> Result<(), LicenseClientError> {
        let public_install = self.public_install.lock().await.clone();
        let development_install = self.development_install.lock().await.clone();
        let status_snapshot = self.status.read().await.clone();
        let licensed_install = status_snapshot
            .licensed_plugins
            .iter()
            .any(|code| code == &self.code)
            && matches!(
                status_snapshot.state,
                LicenseState::Active | LicenseState::Degraded
            );
        if development_install.is_some() && public_install.is_none() && !licensed_install {
            let _guard = self.update_lock.lock().await;
            let installed = self.development_install.lock().await.clone();
            if let Some(host) = self.host.lock().await.take() {
                if let Err(error) = host.stop().await {
                    *self.host.lock().await = Some(host);
                    return Err(LicenseClientError::new(
                        LicenseErrorCode::ServerError,
                        format!("local plugin could not be stopped: {error}"),
                    ));
                }
            }
            if let Some(database) = &self.database {
                database
                    .delete_local_setting(&self.development_install_key())
                    .map_err(|error| {
                        LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                    })?;
            }
            *self.development_install.lock().await = None;
            *self.status.write().await = PluginStatus::inactive(&self.code);
            if let Some(installed) = installed {
                let _ = std::fs::remove_file(installed.plugin_path);
            }
            return Ok(());
        }
        if public_install.is_some() {
            let _guard = self.update_lock.lock().await;
            let Some(installed) = self.public_install.lock().await.clone() else {
                return Ok(());
            };
            if let Some(host) = self.host.lock().await.take() {
                if let Err(error) = host.stop().await {
                    *self.host.lock().await = Some(host);
                    return Err(LicenseClientError::new(
                        LicenseErrorCode::ServerError,
                        format!("public plugin could not be stopped: {error}"),
                    ));
                }
            }
            if let Some(database) = &self.database {
                if let Err(error) = database
                    .delete_local_setting(&self.public_install_key())
                    .and_then(|_| database.delete_local_setting(&self.development_install_key()))
                {
                    if std::path::Path::new(&installed.plugin_path).is_file() {
                        match self
                            .start_host(std::path::Path::new(&installed.plugin_path))
                            .await
                        {
                            Ok(host) => *self.host.lock().await = Some(host),
                            Err(restart_error) => {
                                tracing::error!(plugin_code = %self.code, error = %restart_error, "public plugin host could not restart after uninstall state persistence failed")
                            }
                        }
                    }
                    return Err(LicenseClientError::new(
                        LicenseErrorCode::ServerError,
                        error.to_string(),
                    ));
                }
            }
            *self.public_install.lock().await = None;
            *self.development_install.lock().await = None;
            *self.status.write().await = PluginStatus::inactive(&self.code);
            if let Err(error) = std::fs::remove_file(&installed.plugin_path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(plugin_code = %self.code, %error, "public plugin binary could not be removed after uninstall");
                }
            }
            return Ok(());
        }
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "No active license.")
        })?;
        if let Err(error) = self
            .service
            .deactivate(
                &persisted.activation_id,
                &persisted.activation_token,
                &self.fingerprint,
            )
            .await
        {
            // A server that does not know the activation (replaced, or with its
            // database rebuilt) has no slot to free; refusing here would leave the
            // plugin stuck to a license no server recognizes.
            if error.code() != LicenseErrorCode::LeaseUnknown {
                return Err(error);
            }
            tracing::warn!(plugin_code = %self.code, activation_id = %persisted.activation_id, "license server does not know this activation; removing it locally");
        }
        if let Some(host) = self.host.lock().await.take() {
            let _ = host.stop().await;
        }
        if let Some(database) = &self.database {
            database
                .delete_local_setting(&self.setting_key())
                .and_then(|_| database.delete_local_setting(&self.development_install_key()))
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self.pending.lock().await = None;
        *self.persisted.lock().await = None;
        *self.development_install.lock().await = None;
        *self.status.write().await = PluginStatus::inactive(&self.code);
        if let Some(installed) = development_install {
            let _ = std::fs::remove_file(installed.plugin_path);
        }
        Ok(())
    }

    /// Restarts the plugin host when the license is active but the host is not
    /// running (for example, after a temporary revocation stopped it and a
    /// subsequent heartbeat restored the license). Only marks the plugin ready
    /// after the handshake; failures keep `active` false and publish
    /// `installation_error`.
    /// Used both in the boot restore (`restore`) and in the heartbeat's
    /// self-heal (`heartbeat`) - same logic, in a single place.
    async fn clear_installation_failure(&self) {
        let mut persisted = self.persisted.lock().await;
        if let Some(state) = persisted.as_mut() {
            state.installation_error = None;
            if let Some(database) = &self.database {
                if let Err(error) = database.save_local_setting(&self.setting_key(), state) {
                    tracing::warn!(plugin_code = %self.code, %error, "plugin installation-ready state could not be persisted");
                }
            }
        }
        drop(persisted);
        let mut status = self.status.write().await;
        status.active = true;
        status.installation_error = None;
    }

    async fn ensure_host_running(&self, persisted: &PersistedPluginState) {
        // On demand, neither boot nor heartbeat starts the process: only `open_on_demand` does.
        if self.on_demand() || self.host.lock().await.is_some() {
            return;
        }
        let Some(path) = persisted.plugin_path.clone().map(std::path::PathBuf::from) else {
            return;
        };
        if !path.is_file() {
            self.record_installation_failure(
                "The installed plugin binary is missing. Reactivate it to download it again."
                    .into(),
            )
            .await;
            return;
        }
        if self.database.is_none() {
            self.record_installation_failure(
                "The local POM database is not available to start the plugin host.".into(),
            )
            .await;
            return;
        }
        match self.start_host(&path).await {
            Ok(host) => {
                *self.host.lock().await = Some(host);
                self.clear_installation_failure().await;
                tracing::info!(plugin_code = %self.code, "plugin host restarted after the license became active again");
            }
            Err(error) => {
                tracing::warn!(plugin_code = %self.code, error = %error, "plugin host could not be restarted after the license became active again");
                self.record_installation_failure(error.to_string()).await;
            }
        }
    }

    pub(crate) async fn ingest_audit_event(&self, event: crate::reqlog::AuditEvent) -> bool {
        let Some(host) = self.host.lock().await.clone() else {
            tracing::warn!(
                plugin_code = %self.code,
                "audit event dropped: the plugin host is not running (license inactive or waiting for restore)"
            );
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
        // The plugin answers late when it is busy, and an answer that comes after the host's timeout is lost. Both
        // events are idempotent in the plugin (`begin` upserts the row, `complete` updates it), so sending again is
        // safe; a completion lost for good leaves the request `incomplete` forever.
        for (attempt, pause) in AUDIT_RETRY_PAUSES.iter().enumerate() {
            match host.ingest(event.clone()).await {
                Ok(()) => return true,
                Err(error) => {
                    tracing::warn!(
                        plugin_code = %self.code,
                        attempt = attempt + 1,
                        error = %error,
                        "audit event did not enter the plugin; trying again"
                    );
                    tokio::time::sleep(*pause).await;
                }
            }
        }
        match host.ingest(event).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(plugin_code = %self.code, error = %error, "audit event did not enter the plugin");
                false
            }
        }
    }

    /// Hands the plugin one event of its own kind (`{"type": ...}`). The
    /// caller decides how loudly to report a refusal: a stream of events
    /// would otherwise log one warning per event.
    pub(crate) async fn ingest_event(&self, event: serde_json::Value) -> Result<(), String> {
        if !self.status().await.active {
            return Err("plugin inactive on this node".into());
        }
        let Some(host) = self.host.lock().await.clone() else {
            return Err("plugin host is not running".into());
        };
        host.ingest(event).await.map_err(|error| error.to_string())
    }

    pub(crate) async fn query_audit(
        &self,
        operation: &str,
        query: serde_json::Value,
    ) -> Option<serde_json::Value> {
        if !self.status().await.active {
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
                tracing::warn!(plugin_code = %self.code, error = %error, "plugin query failed");
                None
            }
        }
    }

    /// Like `query_audit`, for an operation a plugin is free not to implement:
    /// a refusal is expected and stays out of the warning log.
    pub(crate) async fn query_optional(
        &self,
        operation: &str,
        query: serde_json::Value,
    ) -> Option<serde_json::Value> {
        if !self.status().await.active {
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
                tracing::debug!(plugin_code = %self.code, operation, %error, "plugin did not answer the optional operation");
                None
            }
        }
    }

    /// The plugin's UI manifest, requested from the host (`ui.manifest`) and
    /// validated. `None` when inactive, without a host, without UI or with an
    /// invalid manifest.
    pub(crate) async fn ui_manifest(&self) -> Option<crate::plugin_ui::PluginUiManifest> {
        // On-demand plugin with its process stopped: the menu uses the saved manifest.
        if self.on_demand() && self.host.lock().await.is_none() {
            let raw = self.ui_manifest_raw.lock().await.clone()?;
            return crate::plugin_ui::PluginUiManifest::parse(&self.code, raw).ok();
        }
        // A query that times out (the plugin is busy) must not hide an installed plugin's menu: the last valid
        // manifest, kept in memory and in the database, stands in until the next answer.
        let Some(value) = self
            .query_audit("ui.manifest", serde_json::json!({}))
            .await
        else {
            let raw = self.ui_manifest_raw.lock().await.clone()?;
            return crate::plugin_ui::PluginUiManifest::parse(&self.code, raw).ok();
        };
        match crate::plugin_ui::PluginUiManifest::parse(&self.code, value.clone()) {
            Ok(manifest) => {
                self.remember_ui_manifest(value, manifest.lifecycle.is_on_demand())
                    .await;
                Some(manifest)
            }
            Err(error) => {
                tracing::warn!(plugin_code = %self.code, %error, "plugin UI manifest refused");
                None
            }
        }
    }

    /// Stores the newly validated manifest (in memory and in the database) only when it
    /// changed, because the menu requests it on every query.
    async fn remember_ui_manifest(&self, raw: serde_json::Value, on_demand: bool) {
        self.on_demand
            .store(on_demand, std::sync::atomic::Ordering::Release);
        let mut cached = self.ui_manifest_raw.lock().await;
        if cached.as_ref() == Some(&raw) {
            return;
        }
        if let Some(database) = &self.database {
            let key = format!("{UI_MANIFEST_SETTING_PREFIX}{}", self.code);
            if let Err(error) = database.save_local_setting(&key, &raw) {
                tracing::warn!(plugin_code = %self.code, %error, "UI manifest could not be saved");
            }
        }
        *cached = Some(raw);
        // A new manifest may name other catalogs or another icon.
        self.shell_assets_kept
            .store(false, std::sync::atomic::Ordering::Release);
    }

    fn shell_asset_key(&self, path: &str) -> String {
        format!("{UI_ASSET_SETTING_PREFIX}{}:{path}", self.code)
    }

    /// The kept copy of a shell asset (`{content_type, base64}`, as `ui.asset`
    /// answers), for when the host cannot answer: an on-demand plugin whose
    /// process is stopped still has its menu, so its labels and icon must load.
    pub(crate) fn shell_asset(&self, path: &str) -> Option<serde_json::Value> {
        let text = self
            .database
            .as_ref()?
            .local_setting_json(&self.shell_asset_key(path))
            .ok()
            .flatten()?;
        serde_json::from_str(&text).ok()
    }

    /// Keeps the host's answer for a shell asset, when it differs.
    pub(crate) fn keep_shell_asset(&self, path: &str, reply: &serde_json::Value) {
        let Some(database) = &self.database else {
            return;
        };
        if self.shell_asset(path).as_ref() == Some(reply) {
            return;
        }
        if let Err(error) = database.save_local_setting(&self.shell_asset_key(path), reply) {
            tracing::warn!(plugin_code = %self.code, %path, %error, "plugin UI asset could not be kept");
        }
    }

    /// Copies every shell asset of `manifest` from the running host, once per
    /// manifest, so they are there the next time the process is stopped.
    async fn keep_shell_assets(&self, manifest: &crate::plugin_ui::PluginUiManifest) {
        if self.host.lock().await.is_none()
            || self
                .shell_assets_kept
                .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        for path in manifest.shell_assets() {
            let Some(reply) = self
                .query_audit("ui.asset", serde_json::json!({ "path": path }))
                .await
            else {
                // Asked again with the next manifest read.
                self.shell_assets_kept
                    .store(false, std::sync::atomic::Ordering::Release);
                continue;
            };
            if crate::plugin_ui::decode_asset(&reply).is_ok() {
                self.keep_shell_asset(&path, &reply);
            }
        }
    }

    /// Starts the process of an on-demand plugin that is already installed and active. Does not touch
    /// the license: only the host.
    pub(crate) async fn open_on_demand(&self) -> Result<(), String> {
        if !self.on_demand() {
            return Err("this plugin is not on demand".into());
        }
        if !self.status.read().await.active {
            return Err("the plugin is not active on this node".into());
        }
        let _guard = self.update_lock.lock().await;
        if self.host.lock().await.is_some() {
            return Ok(());
        }
        let path = self
            .installed_plugin_path()
            .await
            .ok_or("the plugin has no installed binary")?;
        let host = self
            .start_host(&path)
            .await
            .map_err(|error| format!("{error:#}"))?;
        *self.host.lock().await = Some(host);
        self.clear_installation_failure().await;
        Ok(())
    }

    /// Stops the process of an on-demand plugin. The plugin stays installed and
    /// active, and comes back with `open_on_demand`.
    pub(crate) async fn close_on_demand(&self) -> Result<(), String> {
        if !self.on_demand() {
            return Err("this plugin is not on demand".into());
        }
        let _guard = self.update_lock.lock().await;
        if let Some(host) = self.host.lock().await.take() {
            if let Err(error) = host.stop().await {
                *self.host.lock().await = Some(host);
                return Err(format!("the process could not be stopped: {error:#}"));
            }
        }
        Ok(())
    }

    /// Path of the binary of the current installation, in the order boot restore
    /// picks it: license, then public installation, then local.
    async fn installed_plugin_path(&self) -> Option<std::path::PathBuf> {
        if let Some(path) = self
            .persisted
            .lock()
            .await
            .as_ref()
            .and_then(|persisted| persisted.plugin_path.clone())
        {
            return Some(path.into());
        }
        if let Some(installed) = self.public_install.lock().await.as_ref() {
            return Some(installed.plugin_path.clone().into());
        }
        self.development_install
            .lock()
            .await
            .as_ref()
            .map(|installed| installed.plugin_path.clone().into())
    }

    pub(crate) async fn mark_plugin_ready(
        &self,
        plugin_version: &str,
    ) -> Result<(), LicenseClientError> {
        let pending = self.pending.lock().await.take().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "No activation waiting for the host.",
            )
        })?;
        if pending.fingerprint != self.fingerprint {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FingerprintMismatch,
                "The activation does not belong to this installation.",
            ));
        }
        let features = features_for_code(&self.code, &pending.response.plugin_manifest.feature_set);
        if features.is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "The installed release declares no feature for this plugin.",
            ));
        }
        let installed_at = chrono::Utc::now().to_rfc3339();
        let mut persisted_guard = self.persisted.lock().await;
        let mut persisted = persisted_guard.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Local activation missing.")
        })?;
        persisted.installed_at = Some(installed_at.clone());
        if let Some(database) = &self.database {
            database
                .save_local_setting(&self.setting_key(), &persisted)
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *persisted_guard = Some(persisted);
        drop(persisted_guard);
        let mut status = self.status.write().await;
        status.active = true;
        status.state = LicenseState::Active;
        status.features = features;
        status.plugin_version = Some(plugin_version.to_owned());
        status.lease_expires_at = Some(pending.response.lease_expires_at);
        status.installed_at = Some(installed_at);
        status.installation_error = None;
        status.available_update = None;
        Ok(())
    }

    #[cfg(test)]
    async fn mark_heartbeat_failure(&self, within_grace: bool) {
        let mut status = self.status.write().await;
        if !status.active {
            return;
        }
        status.state = if within_grace {
            LicenseState::Degraded
        } else {
            status.active = false;
            status.available_update = None;
            LicenseState::Expired
        };
    }

    pub(crate) async fn heartbeat(&self, nonce: &str) -> Result<(), LicenseClientError> {
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "No local lease.")
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
                    licensed_plugins,
                    license_expires_at,
                    plugin_usage,
                    activation_candidates,
                    lease_expires_at,
                    heartbeat_due_at,
                    grace_seconds,
                    plugin_release_id,
                    release_changed,
                    available_release_id,
                    grant,
                    ..
                } = response.payload;
                let usage = plugin_usage_for_code(&self.code, &plugin_usage);
                let activation_count = usage.map(|entry| entry.activation_count);
                let max_activations = usage.map(|entry| entry.max_activations);
                // The heartbeat brings no manifest, so the features stay those of
                // the installed release. What it brings is the list of plugins the
                // license still grants: if this one left the list, the plugin
                // loses its features right away.
                let features = if licensed_plugins.iter().any(|code| *code == self.code) {
                    persisted.features.clone()
                } else {
                    Vec::new()
                };
                let mut persisted = self.persisted.lock().await;
                let mut updated_persisted = None;
                if let Some(state) = persisted.as_mut() {
                    state.lease_token = lease_token;
                    state.lease_expires_at = lease_expires_at.clone();
                    state.license_expires_at = license_expires_at.clone();
                    state.activation_count = activation_count;
                    state.max_activations = max_activations;
                    state.licensed_plugins = licensed_plugins.clone();
                    state.plugin_usage = plugin_usage.clone();
                    state.activation_candidates = activation_candidates.clone();
                    state.heartbeat_due_at = heartbeat_due_at;
                    state.grace_seconds = grace_seconds;
                    // `installed_release_id` is NOT written here: the heartbeat neither
                    // downloads nor verifies anything, and `detect_update` compares the
                    // manifest against exactly this field - writing it here corrupted
                    // that very comparison (see the comment on the field in
                    // `PersistedPluginState`). The heartbeat's `plugin_release_id`/
                    // `release_changed`/`available_release_id` are still consumed, but
                    // only to feed `status.available_update` (below), never this
                    // baseline.
                    state.signed_grant = grant;
                    state.features = features.clone();
                    state.last_success_at = Some(chrono::Utc::now().to_rfc3339());
                    if let Some(database) = &self.database {
                        let _ = database.save_local_setting(&self.setting_key(), state);
                    }
                    updated_persisted = Some(state.clone());
                }
                drop(persisted);
                if release_changed {
                    tracing::info!(
                        plugin_code = %self.code,
                        plugin_release_id = %plugin_release_id,
                        available_release_id = ?available_release_id,
                        "heartbeat indicated a plugin release change"
                    );
                }
                let host_running = self.host.lock().await.is_some();
                #[cfg(test)]
                let host_running = host_running
                    || self
                        .mock_host_ready
                        .load(std::sync::atomic::Ordering::Relaxed);
                let mut status = self.status.write().await;
                let license_was_active = status.state == LicenseState::Active;
                // On demand, installed and licensed is enough; the process starts when opened.
                status.active = host_running || self.on_demand();
                status.state = LicenseState::Active;
                status.features = features;
                status.lease_expires_at = Some(lease_expires_at);
                status.license_expires_at = license_expires_at;
                status.activation_count = activation_count;
                status.max_activations = max_activations;
                status.licensed_plugins = licensed_plugins;
                status.plugin_usage = plugin_usage;
                status.activation_candidates = activation_candidates;
                drop(status);
                if !license_was_active {
                    tracing::info!(
                        plugin_code = %self.code,
                        "plugin license became active again after a successful heartbeat"
                    );
                }
                if let Some(persisted) = updated_persisted {
                    self.ensure_host_running(&persisted).await;
                }
                // The heartbeat signaled a new release; re-query the manifest (via
                // `detect_update`, now with the comparison baseline intact) to
                // publish `status.available_update` with the real version and
                // changelog, without waiting for the next manual click on "Update".
                // This also covers the periodic heartbeat supervisor
                // (`spawn_heartbeat_supervisor`), which never called
                // `detect_update` on its own.
                if release_changed {
                    if let Err(error) = self.detect_update().await {
                        tracing::warn!(
                            plugin_code = %self.code,
                            error = %error,
                            "update detection after the heartbeat failed"
                        );
                    }
                }
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
                    status.available_update = None;
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
        let runtime = self.clone();
        tokio::spawn(async move {
            loop {
                let delay = runtime.heartbeat_delay().await;
                tokio::time::sleep(delay).await;
                if runtime.persisted.lock().await.is_none() {
                    continue;
                }
                if let Err(error) = runtime.heartbeat(&uuid::Uuid::new_v4().to_string()).await {
                    tracing::warn!(plugin_code = %runtime.code, error = %error, "plugin heartbeat failed");
                } else if let Err(error) = runtime.detect_update().await {
                    tracing::warn!(plugin_code = %runtime.code, error = %error, "plugin update detection failed");
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

    /// Re-queries `/v1/licenses/status` to publish an updated
    /// `license_expires_at` - the validity of the LICENSE, distinct from the
    /// technical `lease_expires_at` the heartbeat already renews. Called only by
    /// the manual click on "Update" (`PluginManager::refresh`), not by the
    /// periodic supervisor: the heartbeat already covers the recurring health
    /// check, and this is an extra call to the same endpoint `restore()` uses at
    /// startup. A failure here never brings down the calling `refresh` - it is
    /// enrichment, not proof of validity.
    async fn refresh_license_status(&self) -> Result<(), LicenseClientError> {
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "No local lease.")
        })?;
        let response = self.service.status(&persisted, &self.fingerprint).await?;
        let mut status = self.status.write().await;
        status.license_expires_at = response.payload.license_expires_at;
        Ok(())
    }

    /// This plugin's OWN version update check - not shared with other plugins or
    /// with `pom_core`. It only publishes the notice in the status; download,
    /// swap and host restart are reserved for the operator's explicit
    /// confirmation in `apply_update`.
    pub(crate) async fn detect_update(&self) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        if !self.status().await.active {
            return Ok(());
        }
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "local lease missing to check the plugin update",
            )
        })?;
        let manifest = self
            .service
            .manifest(
                &self.code,
                &persisted.lease_token,
                &persisted.lease_id,
                &self.fingerprint.platform,
                &self.fingerprint.arch,
            )
            .await?;
        validate_manifest_for_runtime(&self.code, &manifest.manifest)?;
        let available =
            (manifest.manifest.release_id != persisted.installed_release_id).then(|| {
                PluginUpdateInfo {
                    release_id: manifest.manifest.release_id.clone(),
                    version: manifest.manifest.version.clone(),
                    changelog: manifest.manifest.changelog.clone(),
                }
            });
        let update_checked_at = chrono::Utc::now().to_rfc3339();
        let mut persisted_guard = self.persisted.lock().await;
        let has_persisted = if let Some(state) = persisted_guard.as_mut() {
            state.update_checked_at = Some(update_checked_at.clone());
            if let Some(database) = &self.database {
                database
                    .save_local_setting(&self.setting_key(), state)
                    .map_err(|error| {
                        LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                    })?;
            }
            true
        } else {
            false
        };
        drop(persisted_guard);
        if !has_persisted {
            return Err(LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "local lease missing while saving the update check",
            ));
        }
        let mut status = self.status.write().await;
        if status.active {
            if let Some(update) = &available {
                tracing::info!(
                    plugin_code = %self.code,
                    release_id = %update.release_id,
                    version = %update.version,
                    "new plugin release found"
                );
            }
            status.update_checked_at = Some(update_checked_at);
            status.available_update = available;
        }
        Ok(())
    }

    /// Downloads, installs and restarts the host only with the release the
    /// operator confirmed on the Plugins screen. Re-queries the manifest so as
    /// not to reuse metadata from an old detection.
    pub(crate) async fn apply_update(
        &self,
        requested_release_id: &str,
    ) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        let status = self.status().await;
        if !status.active {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "this plugin must be active to be updated",
            ));
        }
        let requested_release_id = requested_release_id.trim();
        let Some(available) = status.available_update.as_ref() else {
            return Err(update_unavailable("no update of this plugin is available"));
        };
        if available.release_id != requested_release_id {
            return Err(update_unavailable(
                "the confirmed update is no longer the available update",
            ));
        }
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "local lease missing to update the plugin",
            )
        })?;
        let manifest = self
            .service
            .manifest(
                &self.code,
                &persisted.lease_token,
                &persisted.lease_id,
                &self.fingerprint.platform,
                &self.fingerprint.arch,
            )
            .await?;
        if manifest.manifest.release_id != requested_release_id {
            return Err(update_unavailable(
                "the confirmed update is no longer a new available release",
            ));
        }
        validate_manifest_for_runtime(&self.code, &manifest.manifest)?;
        let bytes = self.service.download(&manifest.download).await?;
        validate_download(&manifest.manifest, &bytes)?;
        let path = plugin_path(
            &self.code,
            &manifest.manifest.release_id,
            &self.fingerprint.platform,
            &self.fingerprint.arch,
        )?;
        install_plugin_atomically(&path, &bytes)?;
        if let Some(host) = self.host.lock().await.take() {
            let _ = host.stop().await;
        }
        let host = self.start_host(&path).await.map_err(|error| {
            LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
        })?;
        *self.host.lock().await = Some(host);
        let mut persisted = persisted;
        persisted.installation_error = None;
        persisted.installed_release_id = manifest.manifest.release_id.clone();
        persisted.plugin_version = manifest.manifest.version.clone();
        persisted.plugin_sha256 = manifest.manifest.sha256.clone();
        persisted.plugin_path = Some(path.to_string_lossy().into_owned());
        persisted.preferences_schema = manifest.manifest.preferences.clone();
        persisted.installed_at = Some(chrono::Utc::now().to_rfc3339());
        persisted.last_success_at = Some(chrono::Utc::now().to_rfc3339());
        if let Some(database) = &self.database {
            database
                .save_local_setting(&self.setting_key(), &persisted)
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self.persisted.lock().await = Some(persisted.clone());
        let mut status = self.status.write().await;
        status.plugin_version = Some(manifest.manifest.version);
        status.installed_at = persisted.installed_at.clone();
        status.available_update = None;
        status.installation_error = None;
        drop(status);
        self.reconfigure_running_host().await;
        Ok(())
    }

    async fn install_public_github_release(
        &self,
        repository: &crate::plugin_github::GithubRepository,
        downloaded: crate::plugin_github::DownloadedRelease,
    ) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        if self.persisted.lock().await.is_some() {
            return Err(update_unavailable(
                "this plugin code is already installed through the license mechanism",
            ));
        }
        let manifest = downloaded.manifest;
        if manifest.plugin_code != self.code {
            return Err(update_unavailable("manifest belongs to another plugin"));
        }
        if let Some(preferences) = &manifest.preferences {
            crate::plugin_github::validate_preferences_schema(preferences)
                .map_err(|error| LicenseClientError::new(LicenseErrorCode::ServerError, error))?;
        }
        crate::plugin_github::validate_integrity(&manifest, &downloaded.bytes, None)
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, error))?;
        let digest = manifest
            .sha256
            .strip_prefix("sha256:")
            .unwrap_or(&manifest.sha256);
        let installed_id = format!("github-{}-{digest}", manifest.release_id);
        let path = plugin_path(
            &self.code,
            &installed_id,
            &self.fingerprint.platform,
            &self.fingerprint.arch,
        )?;
        let already_installed = std::fs::read(&path).ok().is_some_and(|bytes| {
            crate::plugin_github::validate_integrity(&manifest, &bytes, None).is_ok()
        });
        if !already_installed {
            if path.exists() {
                std::fs::remove_file(&path).map_err(io_error)?;
            }
            install_plugin_atomically(&path, &downloaded.bytes)?;
        }
        let previous = self.public_install.lock().await.clone();
        let previous_development = self.development_install.lock().await.clone();
        let old_path = previous
            .as_ref()
            .filter(|previous| previous.fingerprint == self.fingerprint)
            .map(|previous| std::path::PathBuf::from(&previous.plugin_path))
            .or_else(|| {
                previous_development
                    .as_ref()
                    .filter(|previous| previous.fingerprint == self.fingerprint)
                    .map(|previous| std::path::PathBuf::from(&previous.plugin_path))
            });
        let installed = PublicPluginInstallState {
            fingerprint: self.fingerprint.clone(),
            repository_id: Some(repository.id.clone()),
            repository_url: Some(repository.repository_url.clone()),
            installed_release_id: installed_id,
            plugin_version: manifest.version.clone(),
            features: features_for_code(&self.code, &manifest.feature_set),
            plugin_sha256: manifest.sha256.clone(),
            size: manifest.size,
            plugin_path: path.to_string_lossy().into_owned(),
            preferences_schema: manifest.preferences.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Some(old_host) = self.host.lock().await.take() {
            if let Err(error) = old_host.stop().await {
                *self.host.lock().await = Some(old_host);
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    format!("the previous plugin host could not be stopped: {error}"),
                ));
            }
        }
        let host = match self.start_host(&path).await {
            Ok(host) => host,
            Err(error) => {
                if let Some(old_path) = old_path.as_deref().filter(|path| path.is_file()) {
                    match self.start_host(old_path).await {
                        Ok(host) => *self.host.lock().await = Some(host),
                        Err(restart_error) => {
                            tracing::error!(plugin_code = %self.code, error = %restart_error, "previous public plugin host could not be restored after selected release failed to start")
                        }
                    }
                }
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    error.to_string(),
                ));
            }
        };
        if let Some(database) = &self.database {
            if let Err(error) = database.save_local_setting(&self.public_install_key(), &installed)
            {
                let _ = host.stop().await;
                if let Some(old_path) = old_path.as_deref().filter(|path| path.is_file()) {
                    match self.start_host(old_path).await {
                        Ok(host) => *self.host.lock().await = Some(host),
                        Err(restart_error) => {
                            tracing::error!(plugin_code = %self.code, error = %restart_error, "previous public plugin host could not be restored after selected release state failed to persist")
                        }
                    }
                }
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    error.to_string(),
                ));
            }
            database
                .delete_local_setting(&self.development_install_key())
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self.host.lock().await = Some(host);
        *self.development_install.lock().await = None;
        *self.public_install.lock().await = Some(installed.clone());
        let mut status = self.status.write().await;
        status.active = true;
        status.state = LicenseState::Active;
        status.public_install = true;
        status.development_install = false;
        status.features = installed.features.clone();
        status.plugin_version = Some(installed.plugin_version.clone());
        status.lease_expires_at = None;
        status.license_expires_at = None;
        status.activation_count = None;
        status.max_activations = None;
        status.licensed_plugins.clear();
        status.plugin_usage.clear();
        status.activation_candidates.clear();
        status.installed_at = Some(installed.installed_at);
        status.available_update = None;
        status.installation_error = None;
        if let Some(old_path) = old_path.filter(|path| {
            path.as_path() != std::path::Path::new(&installed.plugin_path)
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("dev-"))
        }) {
            let _ = std::fs::remove_file(old_path);
        }
        Ok(())
    }

    /// Installs a development artifact without changing license or GitHub
    /// entitlement. In a local-only install the host is active, but its
    /// feature list stays empty and the license state remains
    /// `NeedsActivation`.
    async fn install_development_artifact(
        &self,
        bytes: &[u8],
    ) -> Result<String, LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        if bytes.is_empty() || bytes.len() as u64 > 512 * 1024 * 1024 {
            return Err(update_unavailable("invalid development plugin"));
        }

        let mut licensed = self.persisted.lock().await.clone();
        let mut public = self.public_install.lock().await.clone();
        let previous_development = self.development_install.lock().await.clone();
        let digest = hex::encode(sha2::Sha256::digest(bytes));
        let release_id = format!(
            "dev-{}-{}",
            chrono::Utc::now().timestamp_millis(),
            &digest[..16]
        );
        let path = plugin_path(
            &self.code,
            &release_id,
            &self.fingerprint.platform,
            &self.fingerprint.arch,
        )?;
        install_plugin_atomically(&path, bytes)?;
        let installed_at = chrono::Utc::now().to_rfc3339();
        let old_path = licensed
            .as_ref()
            .and_then(|state| state.plugin_path.as_ref())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                public
                    .as_ref()
                    .map(|state| state.plugin_path.clone().into())
            })
            .or_else(|| {
                previous_development
                    .as_ref()
                    .map(|state| state.plugin_path.clone().into())
            });
        let old_host = self.host.lock().await.take();
        if let Some(host) = old_host {
            if let Err(error) = host.stop().await {
                *self.host.lock().await = Some(host);
                let _ = std::fs::remove_file(&path);
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    format!("the previous plugin host could not be stopped: {error}"),
                ));
            }
        }
        let host = match self.start_host(&path).await {
            Ok(host) => host,
            Err(error) => {
                if let Some(old_path) = old_path.as_deref().filter(|path| path.is_file()) {
                    match self.start_host(old_path).await {
                        Ok(host) => *self.host.lock().await = Some(host),
                        Err(restart_error) => {
                            tracing::error!(plugin_code = %self.code, error = %restart_error, "previous plugin host could not be restored after development rebuild failed")
                        }
                    }
                }
                let _ = std::fs::remove_file(&path);
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    error.to_string(),
                ));
            }
        };

        if let Some(state) = licensed.as_mut() {
            state.plugin_path = Some(path.to_string_lossy().into_owned());
            state.plugin_sha256 = format!("sha256:{digest}");
            state.installed_at = Some(installed_at.clone());
        }
        if let Some(state) = public.as_mut() {
            state.plugin_path = path.to_string_lossy().into_owned();
            state.plugin_sha256 = format!("sha256:{digest}");
            state.size = bytes.len() as u64;
            state.installed_at = installed_at.clone();
        }
        let development = DevelopmentPluginInstallState {
            fingerprint: self.fingerprint.clone(),
            plugin_sha256: digest,
            size: bytes.len() as u64,
            plugin_path: path.to_string_lossy().into_owned(),
            installed_at: installed_at.clone(),
        };

        let save_result = if let Some(database) = &self.database {
            let result = if let Some(state) = &licensed {
                database.save_local_setting(&self.setting_key(), state)
            } else {
                Ok(())
            };
            result
                .and_then(|()| {
                    if let Some(state) = &public {
                        database.save_local_setting(&self.public_install_key(), state)
                    } else {
                        Ok(())
                    }
                })
                .and_then(|()| {
                    database.save_local_setting(&self.development_install_key(), &development)
                })
        } else {
            Ok(())
        };
        if let Err(error) = save_result {
            let _ = host.stop().await;
            if let Some(old_path) = old_path.as_deref().filter(|path| path.is_file()) {
                match self.start_host(old_path).await {
                    Ok(host) => *self.host.lock().await = Some(host),
                    Err(restart_error) => {
                        tracing::error!(plugin_code = %self.code, error = %restart_error, "previous plugin host could not be restored after development state persistence failed")
                    }
                }
            }
            let _ = std::fs::remove_file(&path);
            return Err(LicenseClientError::new(
                LicenseErrorCode::ServerError,
                error.to_string(),
            ));
        }

        *self.host.lock().await = Some(host);
        *self.development_install.lock().await = Some(development);
        if let Some(state) = licensed {
            *self.persisted.lock().await = Some(state);
        }
        if let Some(state) = public {
            *self.public_install.lock().await = Some(state.clone());
            self.status.write().await.installed_at = Some(state.installed_at);
        } else {
            self.status.write().await.installed_at = Some(installed_at.clone());
        }
        let has_license_or_github_install =
            self.persisted.lock().await.is_some() || self.public_install.lock().await.is_some();
        {
            let mut status = self.status.write().await;
            status.active = true;
            status.development_install = true;
            if !has_license_or_github_install {
                status.state = LicenseState::NeedsActivation;
                status.features.clear();
                status.public_install = false;
                status.licensed_plugins.clear();
                status.lease_expires_at = None;
                status.activation_count = None;
                status.max_activations = None;
            } else if !matches!(status.state, LicenseState::Active | LicenseState::Degraded) {
                status.features.clear();
            }
            status.installation_error = None;
        }
        if let Some(old_path) = old_path.filter(|old_path| {
            old_path.as_path() != path.as_path()
                && old_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("dev-"))
        }) {
            let _ = std::fs::remove_file(old_path);
        }
        Ok(installed_at)
    }
}

/// Dynamic collection of [`PluginRuntime`], one per granted or restored
/// `plugin_code`. It is the only entry point of the rest of the backend into the
/// plugin licensing system - `crate::backend::gateway::GatewayState` keeps an
/// `Arc<PluginManager>`.
pub(crate) struct PluginManager {
    fingerprint: Fingerprint,
    service: Arc<dyn LicenseService>,
    database: Option<Arc<crate::db::PomDb>>,
    runtimes: StdRwLock<HashMap<String, Arc<PluginRuntime>>>,
    host_context: HostContext,
    github_client: crate::plugin_github::GithubReleaseClient,
    github_repositories: StdRwLock<HashMap<String, crate::plugin_github::GithubRepository>>,
    github_repositories_lock: Mutex<()>,
    events: PluginEventHub,
}

/// How many `pom-plugin-events/v1` events the node keeps for the interface to
/// read through `GET /api/ui/plugins/events`.
const PLUGIN_EVENT_BACKLOG: usize = 100;

/// Short queue of node events for the plugins. Each published event goes to the
/// native backend of every running host (`host.event`) and stays here, with a
/// sequence number, for the interface to relay to the screens.
#[derive(Default)]
struct PluginEventHub {
    next: std::sync::atomic::AtomicU64,
    recent: StdRwLock<std::collections::VecDeque<(u64, serde_json::Value)>>,
}

impl PluginEventHub {
    fn push(&self, envelope: serde_json::Value) -> u64 {
        let seq = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let mut recent = self
            .recent
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        recent.push_back((seq, envelope));
        while recent.len() > PLUGIN_EVENT_BACKLOG {
            recent.pop_front();
        }
        seq
    }

    fn after(&self, after: Option<u64>) -> (u64, Vec<(u64, serde_json::Value)>) {
        let latest = self.next.load(std::sync::atomic::Ordering::SeqCst);
        let Some(after) = after else {
            return (latest, Vec::new());
        };
        let recent = self
            .recent
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let events = recent
            .iter()
            .filter(|(seq, _)| *seq > after)
            .cloned()
            .collect();
        (latest, events)
    }
}

impl PluginManager {
    /// Publishes a node event to all plugins: stores it in the queue read by the
    /// interface and delivers `host.event` to each running host, without waiting
    /// for them.
    pub(crate) fn publish_event(&self, event_type: &str, payload: serde_json::Value) -> u64 {
        let envelope = plugin_event_envelope(event_type, "*", payload);
        let seq = self.events.push(envelope.clone());
        for runtime in self.runtime_values() {
            let envelope = envelope.clone();
            tokio::spawn(async move { runtime.send_host_event(envelope).await });
        }
        seq
    }

    /// Events with a sequence greater than `after`. Without `after`, only the
    /// current sequence: whoever opens the interface does not receive the past
    /// again.
    pub(crate) fn events_after(&self, after: Option<u64>) -> (u64, Vec<(u64, serde_json::Value)>) {
        self.events.after(after)
    }

    /// `restore_on_boot` turns on re-reading each plugin's persisted state (see
    /// [`PluginRuntime::restore`]). In production it is always `true`: it is what
    /// brings an already activated plugin back up after a `pom` restart. Tests
    /// pass `false` because the restore is a concurrent task that overwrites the
    /// in-memory state from the database at an instant the test does not control
    /// - leaving it on would make any scenario that builds state by hand
    /// non-deterministic, without covering anything more.
    fn from_codes(
        plugin_codes: impl IntoIterator<Item = String>,
        service: Arc<dyn LicenseService>,
        fingerprint: Fingerprint,
        database: Option<Arc<crate::db::PomDb>>,
        restore_on_boot: bool,
    ) -> Arc<Self> {
        Self::from_codes_with_github(
            plugin_codes,
            service,
            fingerprint,
            database,
            restore_on_boot,
            crate::plugin_github::GithubReleaseClient::new()
                .with_license_fallback(&license_base_url()),
        )
    }

    fn from_codes_with_github(
        plugin_codes: impl IntoIterator<Item = String>,
        service: Arc<dyn LicenseService>,
        fingerprint: Fingerprint,
        database: Option<Arc<crate::db::PomDb>>,
        restore_on_boot: bool,
        github_client: crate::plugin_github::GithubReleaseClient,
    ) -> Arc<Self> {
        let host_context = HostContext::default();
        let runtimes = plugin_codes
            .into_iter()
            .filter(|code| is_valid_plugin_code(code))
            .map(|code| {
                let runtime = Arc::new(PluginRuntime::new(
                    &code,
                    service.clone(),
                    fingerprint.clone(),
                    database.clone(),
                    host_context.clone(),
                ));
                (code, runtime)
            })
            .collect::<HashMap<_, _>>();
        let github_repositories = database
            .as_ref()
            .and_then(|database| {
                database
                    .local_setting_json(GITHUB_REPOSITORIES_SETTING)
                    .ok()
                    .flatten()
            })
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        let manager = Arc::new(Self {
            fingerprint,
            service,
            database,
            runtimes: StdRwLock::new(runtimes),
            host_context,
            github_client,
            github_repositories: StdRwLock::new(github_repositories),
            github_repositories_lock: Mutex::new(()),
            events: PluginEventHub::default(),
        });
        for runtime in manager.runtime_values() {
            if restore_on_boot {
                tokio::spawn({
                    let runtime = runtime.clone();
                    async move { runtime.restore().await }
                });
            }
            runtime.spawn_heartbeat_supervisor();
        }
        manager
    }

    pub(crate) fn runtime_with_database(
        node_id: &str,
        database: Arc<crate::db::PomDb>,
    ) -> Arc<Self> {
        let fingerprint = machine_fingerprint(node_id);
        let plugin_codes = match database.local_setting_keys_with_prefix(PERSISTED_SETTING_PREFIX) {
            Ok(mut keys) => {
                keys.extend(
                    database
                        .local_setting_keys_with_prefix(PUBLIC_INSTALL_SETTING_PREFIX)
                        .unwrap_or_default(),
                );
                keys.extend(
                    database
                        .local_setting_keys_with_prefix(DEVELOPMENT_INSTALL_SETTING_PREFIX)
                        .unwrap_or_default(),
                );
                keys.into_iter()
                    .filter_map(|key| {
                        key.strip_prefix(PERSISTED_SETTING_PREFIX)
                            .or_else(|| key.strip_prefix(PUBLIC_INSTALL_SETTING_PREFIX))
                            .or_else(|| key.strip_prefix(DEVELOPMENT_INSTALL_SETTING_PREFIX))
                            .map(str::to_owned)
                    })
                    .filter(|code| is_valid_plugin_code(code))
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            }
            Err(error) => {
                tracing::warn!(error = %error, "local plugin licenses could not be enumerated at startup");
                Vec::new()
            }
        };
        Self::from_codes(
            plugin_codes,
            Arc::new(HttpLicenseService::new(license_base_url())),
            fingerprint,
            Some(database),
            true,
        )
    }

    #[cfg(test)]
    pub(crate) fn runtime(node_id: &str) -> Arc<Self> {
        let fingerprint = machine_fingerprint(node_id);
        Self::from_codes(
            Vec::new(),
            Arc::new(HttpLicenseService::new(license_base_url())),
            fingerprint,
            None,
            true,
        )
    }

    #[cfg(test)]
    pub(crate) fn test_manager(
        plugin_codes: &[&str],
        service: Arc<dyn LicenseService>,
    ) -> Arc<Self> {
        Self::from_codes(
            plugin_codes.iter().map(|code| (*code).to_owned()),
            service,
            test_fingerprint(),
            None,
            false,
        )
    }

    /// Like `test_manager`, but with a real `PomDb` to persist each plugin's code
    /// and state.
    #[cfg(test)]
    pub(crate) fn test_manager_with_database(
        plugin_codes: &[&str],
        service: Arc<dyn LicenseService>,
        database: Arc<crate::db::PomDb>,
    ) -> Arc<Self> {
        Self::from_codes(
            plugin_codes.iter().map(|code| (*code).to_owned()),
            service,
            test_fingerprint(),
            Some(database),
            false,
        )
    }

    #[cfg(test)]
    pub(crate) fn test_manager_with_github_database(
        plugin_codes: &[&str],
        service: Arc<dyn LicenseService>,
        database: Arc<crate::db::PomDb>,
        api_base: &str,
    ) -> Arc<Self> {
        Self::test_manager_with_github_database_for_target(
            plugin_codes,
            service,
            database,
            api_base,
            "linux",
            "x86_64",
        )
    }

    #[cfg(test)]
    pub(crate) fn test_manager_with_github_database_for_target(
        plugin_codes: &[&str],
        service: Arc<dyn LicenseService>,
        database: Arc<crate::db::PomDb>,
        api_base: &str,
        os: &str,
        arch: &str,
    ) -> Arc<Self> {
        let github = crate::plugin_github::GithubReleaseClient::with_api_base(api_base)
            .expect("test GitHub API endpoint");
        Self::from_codes_with_github(
            plugin_codes.iter().map(|code| (*code).to_owned()),
            service,
            test_fingerprint_for(os, arch),
            Some(database),
            false,
            github,
        )
    }

    pub(crate) fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    pub(crate) fn service(&self) -> Arc<dyn LicenseService> {
        self.service.clone()
    }

    /// Replaces the context handed to the plugin hosts and hands it again to those
    /// already running. Hosts started later receive the new one on startup.
    pub(crate) async fn set_host_context(&self, context: serde_json::Value) {
        {
            let mut current = self
                .host_context
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if current.as_ref() == Some(&context) {
                return;
            }
            *current = Some(context);
        }
        for runtime in self.runtime_values() {
            runtime.reconfigure_running_host().await;
        }
    }

    /// The plugin a loopback API token belongs to (`crate::plugin_host_api`).
    pub(crate) fn code_for_host_api_token(&self, token: &str) -> Option<String> {
        use subtle::ConstantTimeEq;
        if token.is_empty() {
            return None;
        }
        self.runtime_values()
            .into_iter()
            .find(|runtime| bool::from(runtime.host_api_token.as_bytes().ct_eq(token.as_bytes())))
            .map(|runtime| runtime.code.clone())
    }

    #[cfg(test)]
    pub(crate) fn host_api_token_for_test(&self, code: &str) -> String {
        self.runtime_for(code)
            .map(|runtime| runtime.host_api_token.clone())
            .unwrap_or_default()
    }

    pub(crate) fn runtime_for(&self, code: &str) -> Option<Arc<PluginRuntime>> {
        self.runtimes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(code)
            .cloned()
    }

    fn get_or_create_runtime(&self, code: &str) -> Result<Arc<PluginRuntime>, LicenseClientError> {
        if !is_valid_plugin_code(code) {
            return Err(LicenseClientError::new(
                LicenseErrorCode::ServerError,
                "the server returned an invalid plugin_code",
            ));
        }
        let mut runtimes = self
            .runtimes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(runtime) = runtimes.get(code) {
            return Ok(runtime.clone());
        }
        let runtime = Arc::new(PluginRuntime::new(
            code,
            self.service.clone(),
            self.fingerprint.clone(),
            self.database.clone(),
            self.host_context.clone(),
        ));
        runtimes.insert(code.to_owned(), runtime.clone());
        drop(runtimes);
        runtime.spawn_heartbeat_supervisor();
        Ok(runtime)
    }

    fn runtime_values(&self) -> Vec<Arc<PluginRuntime>> {
        self.runtimes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    pub(crate) async fn status(&self, code: &str) -> Option<PluginStatus> {
        match self.runtime_for(code) {
            Some(runtime) => Some(runtime.status().await),
            None => None,
        }
    }

    /// Local status of the plugins already known to this instance, ordered by
    /// code. The commercial catalog and the GitHub repositories are separate
    /// lists.
    pub(crate) async fn all_statuses(&self) -> Vec<PluginStatus> {
        let runtimes = self.runtime_values();
        let mut statuses = Vec::with_capacity(runtimes.len());
        for runtime in runtimes {
            statuses.push(runtime.status().await);
        }
        statuses.sort_by(|a, b| a.code.cmp(&b.code));
        statuses
    }

    pub(crate) fn github_repositories(&self) -> Vec<crate::plugin_github::GithubRepository> {
        let mut repositories = self
            .github_repositories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        repositories.sort_by(|left, right| left.repository_url.cmp(&right.repository_url));
        repositories
    }

    pub(crate) async fn development_plugin_sources(
        &self,
    ) -> crate::plugin_dev::MountedPluginSources {
        let statuses = self
            .all_statuses()
            .await
            .into_iter()
            .map(|status| {
                (
                    status.code,
                    status.active,
                    status.public_install,
                    status.development_install,
                )
            })
            .collect::<Vec<_>>();
        crate::plugin_dev::list_mounted_sources(
            std::path::Path::new(crate::plugin_dev::DEV_PLUGIN_ROOT),
            &statuses,
        )
    }

    pub(crate) async fn rebuild_and_install_development_plugin(
        &self,
        repository: &str,
    ) -> Result<crate::plugin_dev::RebuildInstallResult, crate::plugin_dev::DevPluginError> {
        let build_root = crate::config::project_root().join(".pom-dev-plugin-builds");
        let built = crate::plugin_dev::build_source(
            std::path::Path::new(crate::plugin_dev::DEV_PLUGIN_ROOT),
            &build_root,
            repository,
            &crate::plugin_dev::CargoCommandRunner,
        )
        .await?;
        let runtime = self
            .get_or_create_runtime(&built.plugin_code)
            .map_err(|_| crate::plugin_dev::DevPluginError {
                code: crate::plugin_dev::DevPluginErrorCode::InvalidSource,
                stage: Some(crate::plugin_dev::BuildStage::Inspect),
                exit_code: None,
            })?;
        let installed_at = runtime
            .install_development_artifact(&built.bytes)
            .await
            .map_err(|_| crate::plugin_dev::DevPluginError {
                code: crate::plugin_dev::DevPluginErrorCode::InstallFailed,
                stage: Some(crate::plugin_dev::BuildStage::Install),
                exit_code: None,
            })?;
        Ok(crate::plugin_dev::RebuildInstallResult {
            repository: built.repository,
            plugin_code: built.plugin_code,
            build_ms: built.elapsed_ms,
            installed_at,
        })
    }

    fn persist_github_repositories(
        &self,
        repositories: &HashMap<String, crate::plugin_github::GithubRepository>,
    ) -> Result<(), LicenseClientError> {
        if let Some(database) = &self.database {
            database
                .save_local_setting(GITHUB_REPOSITORIES_SETTING, repositories)
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self
            .github_repositories
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = repositories.clone();
        Ok(())
    }

    pub(crate) async fn add_github_repository(
        &self,
        repository_url: &str,
    ) -> Result<crate::plugin_github::GithubRepository, LicenseClientError> {
        let repository = self
            .inspect_github_repository(repository_url, true)
            .await
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, error))?;
        let _guard = self.github_repositories_lock.lock().await;
        let current = self
            .github_repositories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if current.contains_key(&repository.id) {
            return Err(update_unavailable(
                "this GitHub repository was already registered",
            ));
        }
        let mut next = current;
        next.insert(repository.id.clone(), repository.clone());
        self.persist_github_repositories(&next)?;
        Ok(repository)
    }

    /// GitHub's inspection of a repository, or, with `fallback`, the license
    /// server's listing of the same plugin when GitHub refuses it.
    async fn inspect_github_repository(
        &self,
        repository_url: &str,
        fallback: bool,
    ) -> Result<crate::plugin_github::GithubRepository, String> {
        let (os, arch) = (&self.fingerprint.platform, &self.fingerprint.arch);
        match self
            .github_client
            .inspect_repository(repository_url, os, arch)
            .await
        {
            Ok(repository) => Ok(repository),
            Err(github_error) if fallback => self
                .github_client
                .inspect_from_license_server(repository_url, os, arch, &github_error)
                .await
                .map_err(|license_error| {
                    format!("{github_error}; license server fallback: {license_error}")
                }),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn refresh_github_repository(
        &self,
        id: &str,
    ) -> Result<crate::plugin_github::GithubRepository, LicenseClientError> {
        let current = self
            .github_repositories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let previous = current
            .get(id)
            .cloned()
            .ok_or_else(|| update_unavailable("unknown GitHub repository"))?;
        // A repository listed by the license server is refreshed from it when
        // GitHub still fails; one read from GitHub keeps its saved inspection.
        let refreshed = self
            .inspect_github_repository(
                &previous.repository_url,
                previous.manifest_path == "license-server",
            )
            .await;
        let _guard = self.github_repositories_lock.lock().await;
        let current = self
            .github_repositories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(previous) = current.get(id).cloned() else {
            return Err(update_unavailable("GitHub repository was removed"));
        };
        match refreshed {
            Ok(repository) => {
                let mut next = current;
                next.insert(id.to_owned(), repository.clone());
                self.persist_github_repositories(&next)?;
                Ok(repository)
            }
            Err(error) => {
                let mut retained = previous;
                retained.check_error = Some(error.clone());
                let mut next = current;
                next.insert(id.to_owned(), retained);
                self.persist_github_repositories(&next)?;
                Err(LicenseClientError::new(
                    LicenseErrorCode::UpdateUnavailable,
                    error,
                ))
            }
        }
    }

    pub(crate) async fn remove_github_repository(
        &self,
        id: &str,
    ) -> Result<(), LicenseClientError> {
        let _guard = self.github_repositories_lock.lock().await;
        let current = self
            .github_repositories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !current.contains_key(id) {
            return Err(update_unavailable("unknown GitHub repository"));
        }
        let mut next = current;
        next.remove(id);
        self.persist_github_repositories(&next)
    }

    pub(crate) async fn install_github_repository_release(
        &self,
        id: &str,
        release_id: &str,
    ) -> Result<(), LicenseClientError> {
        let current = self
            .github_repositories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // The repository as it was inspected when added or refreshed: the
        // release list the screen offered comes from it. Inspecting it again
        // here cost several more API calls per install, out of 60 per hour
        // per IP without a token; the release itself is still read fresh, and
        // a release no longer published fails there.
        let repository = current
            .get(id)
            .cloned()
            .ok_or_else(|| update_unavailable("unknown GitHub repository"))?;
        let downloaded = self
            .github_client
            .download_release(
                &repository,
                release_id,
                &self.fingerprint.platform,
                &self.fingerprint.arch,
            )
            .await
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, error))?;
        let plugin_code = &repository.manifest.plugin_code;
        if let Some(database) = &self.database {
            let license_state = database
                .local_setting_json(&format!("{PERSISTED_SETTING_PREFIX}{plugin_code}"))
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
            if license_state.is_some() {
                return Err(update_unavailable(
                    "this plugin code already has a local installation through the license mechanism",
                ));
            }
        }
        let runtime = self.get_or_create_runtime(plugin_code)?;
        if runtime.persisted.lock().await.is_some() {
            return Err(update_unavailable(
                "this plugin code is already installed through the license mechanism; deactivate it before installing another source",
            ));
        }
        if let Some(installed) = runtime.public_install.lock().await.as_ref() {
            if installed.repository_id.as_deref() != Some(repository.id.as_str()) {
                return Err(update_unavailable(
                    "this plugin code is already installed from another GitHub repository",
                ));
            }
        }
        let _guard = self.github_repositories_lock.lock().await;
        let current = self
            .github_repositories
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !current.contains_key(id) {
            return Err(update_unavailable(
                "GitHub repository was removed during the installation",
            ));
        }
        let mut next = current;
        next.insert(id.to_owned(), repository.clone());
        self.persist_github_repositories(&next)?;
        drop(_guard);
        runtime
            .install_public_github_release(&repository, downloaded)
            .await
    }

    pub(crate) async fn save_preferences(
        &self,
        code: &str,
        preferences: enterprise_protocol::PluginPreferences,
    ) -> Result<(), LicenseClientError> {
        let runtime = self
            .runtime_for(code)
            .ok_or_else(|| update_unavailable("unknown plugin"))?;
        runtime.save_preferences(preferences).await
    }

    pub(crate) async fn ingest_audit_event(
        &self,
        code: &str,
        event: crate::reqlog::AuditEvent,
    ) -> bool {
        match self.runtime_for(code) {
            Some(runtime) => runtime.ingest_audit_event(event).await,
            None => false,
        }
    }

    pub(crate) async fn ingest_event(
        &self,
        code: &str,
        event: serde_json::Value,
    ) -> Result<(), String> {
        match self.runtime_for(code) {
            Some(runtime) => runtime.ingest_event(event).await,
            None => Err("unknown plugin on this node".into()),
        }
    }

    pub(crate) async fn query_audit(
        &self,
        code: &str,
        operation: &str,
        query: serde_json::Value,
    ) -> Option<serde_json::Value> {
        self.runtime_for(code)?.query_audit(operation, query).await
    }

    /// Asks every running plugin an optional operation and returns the
    /// answers by `plugin_code`, in code order.
    pub(crate) async fn query_running(&self, operation: &str) -> Vec<(String, serde_json::Value)> {
        let mut answers = Vec::new();
        for runtime in self.runtime_values() {
            if let Some(answer) = runtime
                .query_optional(operation, serde_json::json!({}))
                .await
            {
                answers.push((runtime.code.clone(), answer));
            }
        }
        answers.sort_by(|left, right| left.0.cmp(&right.0));
        answers
    }

    pub(crate) async fn ui_manifest(
        &self,
        code: &str,
    ) -> Option<crate::plugin_ui::PluginUiManifest> {
        let runtime = self.runtime_for(code)?;
        let manifest = runtime.ui_manifest().await?;
        if !runtime
            .shell_assets_kept
            .load(std::sync::atomic::Ordering::Acquire)
        {
            let kept = manifest.clone();
            tokio::spawn(async move { runtime.keep_shell_assets(&kept).await });
        }
        Some(manifest)
    }

    /// See `PluginRuntime::shell_asset`.
    pub(crate) fn shell_asset(&self, code: &str, path: &str) -> Option<serde_json::Value> {
        self.runtime_for(code)?.shell_asset(path)
    }

    /// See `PluginRuntime::keep_shell_asset`.
    pub(crate) fn keep_shell_asset(&self, code: &str, path: &str, reply: &serde_json::Value) {
        if let Some(runtime) = self.runtime_for(code) {
            runtime.keep_shell_asset(path, reply);
        }
    }

    /// Starts the process of an on-demand plugin (menu click).
    pub(crate) async fn open_on_demand(&self, code: &str) -> Result<(), String> {
        match self.runtime_for(code) {
            Some(runtime) => runtime.open_on_demand().await,
            None => Err("unknown plugin on this node".into()),
        }
    }

    /// Stops the process of an on-demand plugin (the X badge).
    pub(crate) async fn close_on_demand(&self, code: &str) -> Result<(), String> {
        match self.runtime_for(code) {
            Some(runtime) => runtime.close_on_demand().await,
            None => Err("unknown plugin on this node".into()),
        }
    }

    /// Validates the key once to find out which `plugin_code`s it grants, creates
    /// the necessary runtimes and activates/installs each of them. Succeeds if at
    /// least one plugin activated; individual failures also stay in
    /// `PluginStatus.installation_error` so the interface can show the reason.
    pub(crate) async fn activate(
        &self,
        license_key: &str,
    ) -> Result<Vec<String>, LicenseClientError> {
        if license_key.trim().is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::InvalidKey,
                "Enter a license key.",
            ));
        }
        let validate_request = RequestEnvelope::new(
            uuid::Uuid::new_v4().to_string(),
            format!("validate:{}", stable_digest(license_key)),
            client_info(),
            enterprise_protocol::ValidatePayload {
                license_key: license_key.to_owned(),
                requested_features: Vec::new(),
                fingerprint_scheme: Some(self.fingerprint.scheme.clone()),
                fingerprint: Some(self.fingerprint.hash.clone()),
            },
        );
        let validation = self.service.validate(validate_request).await?;
        let mut seen = HashSet::new();
        let mut granted_plugins = Vec::new();
        for code in validation.payload.licensed_plugins {
            if !is_valid_plugin_code(&code) {
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    "the server returned an invalid plugin_code",
                ));
            }
            if seen.insert(code.clone()) {
                granted_plugins.push(code);
            }
        }
        if granted_plugins.is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "The key grants no plugin.",
            ));
        }
        let mut activated = Vec::new();
        let mut last_error = None;
        for code in granted_plugins {
            let runtime = self.get_or_create_runtime(&code)?;
            match runtime.activate_and_install(license_key).await {
                Ok(()) => activated.push(code),
                Err(error) => {
                    tracing::warn!(plugin_code = %code, error = %error, "plugin activation failed");
                    last_error = Some(error);
                }
            }
        }
        if activated.is_empty() {
            return Err(last_error.unwrap_or_else(|| {
                LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    "no granted plugin could be activated",
                )
            }));
        }
        Ok(activated)
    }

    pub(crate) async fn deactivate(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self.runtime_for(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "unknown plugin")
        })?;
        runtime.deactivate().await
    }

    pub(crate) async fn refresh(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self.runtime_for(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "unknown plugin")
        })?;
        if runtime.public_install.lock().await.is_some() {
            return Ok(());
        }
        runtime.heartbeat(&uuid::Uuid::new_v4().to_string()).await?;
        if let Err(error) = runtime.detect_update().await {
            tracing::warn!(plugin_code = %code, error = %error, "plugin update detection failed");
        }
        if let Err(error) = runtime.refresh_license_status().await {
            tracing::warn!(plugin_code = %code, error = %error, "license validity check failed");
        }
        Ok(())
    }

    pub(crate) async fn check_update(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self.runtime_for(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "unknown plugin")
        })?;
        if runtime.public_install.lock().await.is_some() {
            return Err(update_unavailable(
                "see the releases on the card of the registered GitHub repository",
            ));
        }
        runtime.detect_update().await
    }

    pub(crate) async fn apply_update(
        &self,
        code: &str,
        release_id: &str,
    ) -> Result<(), LicenseClientError> {
        let runtime = self.runtime_for(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "unknown plugin")
        })?;
        if runtime.public_install.lock().await.is_some() {
            return Err(update_unavailable(
                "plugins installed from GitHub are updated from the repository card",
            ));
        }
        runtime.apply_update(release_id).await
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
}

fn machine_fingerprint(node_id: &str) -> Fingerprint {
    Fingerprint::from_components(
        std::env::consts::OS,
        std::env::consts::ARCH,
        node_id,
        &machine_component(),
        &board_component(),
        &installation_component(),
    )
}

/// Keeps the `host.configure` context up to date: this node's OpenAI endpoint on
/// loopback (loopback plaintext is always accepted, even with TLS) and the node's
/// chat key. Re-evaluated periodically to follow a key that was revoked and
/// recreated; a node that does not yet reach the principal tries again.
fn host_context_payload(
    gateway_port: u16,
    api_key: &str,
    workspace_root: &str,
    model_dirs: &[String],
) -> serde_json::Value {
    serde_json::json!({
        // The whole-model folders (not the slice folder), so a plugin can
        // store and find model files where POM's catalog reads them.
        "model_dirs": model_dirs,
        "gateway": {
            "openai_base_url": format!("http://127.0.0.1:{gateway_port}/v1"),
            "api_key": api_key,
        },
        "workspace_root": workspace_root,
        "host_api_base_url": format!(
            "http://127.0.0.1:{gateway_port}{}",
            crate::plugin_host_api::HOST_API_PREFIX
        ),
    })
}

/// The model folders a plugin may store files in: the whole-model folders the
/// image catalog reads, so never an Ollama store (a file written there would
/// never show up).
fn plugin_model_dirs(dirs: &[std::path::PathBuf]) -> Vec<String> {
    crate::catalog::image_roots(dirs)
        .into_iter()
        .map(|(dir, _)| dir.to_string_lossy().into_owned())
        .collect()
}

pub(crate) async fn refresh_host_context(
    state: &crate::gateway::GatewayState,
) -> anyhow::Result<()> {
    let (key, _) = crate::admin::node_chat_key(state).await?;
    let workspace_root = state.auth.read().await.workspace_root_path();
    let workspace_root = workspace_root.to_string_lossy();
    let model_dirs = plugin_model_dirs(&state.model_dirs().await);
    state
        .plugins
        .set_host_context(host_context_payload(
            state.gateway_port,
            &key.secret,
            &workspace_root,
            &model_dirs,
        ))
        .await;
    Ok(())
}

/// Models the network is serving now: the same ones `/v1/models` lists, with
/// each one's deployments and nodes.
pub(crate) fn serving_models(
    peers: &[crate::network::PeerInfo],
) -> std::collections::BTreeMap<String, serde_json::Value> {
    crate::openai::model_ids_from_peers(peers)
        .into_iter()
        .filter(|id| !id.contains('@'))
        .map(|model| {
            let serving = peers
                .iter()
                .filter(|peer| peer.is_alive() && peer.model == model);
            let mut deploy_ids = std::collections::BTreeSet::new();
            let mut node_ids = std::collections::BTreeSet::new();
            for peer in serving {
                if !peer.deploy_id.is_empty() {
                    deploy_ids.insert(peer.deploy_id.clone());
                }
                node_ids.insert(peer.node_id.clone());
            }
            let payload = serde_json::json!({
                "model": model,
                "deploy_ids": deploy_ids,
                "node_ids": node_ids,
            });
            (model, payload)
        })
        .collect()
}

/// Compares two snapshots of `serving_models` and returns the
/// `model.serving`/`model.stopped` events in the order they must go out.
pub(crate) fn serving_transitions(
    before: &std::collections::BTreeMap<String, serde_json::Value>,
    after: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Vec<(&'static str, serde_json::Value)> {
    let mut events = Vec::new();
    for (model, payload) in after {
        if !before.contains_key(model) {
            events.push(("model.serving", payload.clone()));
        }
    }
    for model in before.keys() {
        if !after.contains_key(model) {
            events.push(("model.stopped", serde_json::json!({ "model": model })));
        }
    }
    events
}

/// Watches the network and notifies the plugins when a model starts or stops
/// being served. The first snapshot is only the baseline: a node that has just
/// started does not announce as news what was already up.
pub(crate) fn spawn_serving_watcher(state: crate::gateway::GatewayState) {
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    tokio::spawn(async move {
        let mut known: Option<std::collections::BTreeMap<String, serde_json::Value>> = None;
        loop {
            let current = serving_models(&state.all_peers().await);
            if let Some(before) = &known {
                for (event_type, payload) in serving_transitions(before, &current) {
                    state.plugins.publish_event(event_type, payload);
                }
            }
            known = Some(current);
            tokio::time::sleep(INTERVAL).await;
        }
    });
}

#[derive(Debug, Deserialize)]
pub(crate) struct PluginEventsQuery {
    #[serde(default)]
    pub after: Option<u64>,
}

/// `GET /api/ui/plugins/events?after=<seq>`: node events that the interface
/// relays to the plugin screens through the `pom-plugin-events/v1` protocol.
pub(crate) async fn list_plugin_events(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Query(query): axum::extract::Query<PluginEventsQuery>,
) -> axum::response::Response {
    let (latest, events) = state.plugins.events_after(query.after);
    axum::Json(serde_json::json!({
        "protocol": PLUGIN_EVENTS_PROTOCOL,
        "latest": latest,
        "events": events
            .into_iter()
            .map(|(seq, event)| serde_json::json!({ "seq": seq, "event": event }))
            .collect::<Vec<_>>(),
    }))
    .into_response()
}

pub(crate) fn spawn_host_context(state: crate::gateway::GatewayState) {
    const REFRESH: std::time::Duration = std::time::Duration::from_secs(600);
    const RETRY: std::time::Duration = std::time::Duration::from_secs(30);
    tokio::spawn(async move {
        loop {
            let wait = match refresh_host_context(&state).await {
                Ok(()) => {
                    // The image components this node serves, again: after a
                    // restart, or a plugin closed and reopened since.
                    let call = crate::components::live_plugins(std::sync::Arc::clone(&state.plugins));
                    crate::components::restore(&state.database, &call).await;
                    REFRESH
                }
                Err(error) => {
                    tracing::warn!(%error, "plugin context without an API key; retrying in 30 s");
                    RETRY
                }
            };
            tokio::time::sleep(wait).await;
        }
    });
}

/// The features ONE `plugin_code` delivers, read from the release manifest's
/// `feature_set` in the format `"<plugin_code>.<feature>"`, without the prefix.
/// A release only declares its own features, but the prefix is filtered anyway:
/// it is what guarantees that a swapped manifest does not lend a feature of
/// another plugin.
fn features_for_code(code: &str, feature_set: &[String]) -> Vec<String> {
    feature_set
        .iter()
        .filter_map(|feature| {
            feature
                .strip_prefix(code)
                .and_then(|rest| rest.strip_prefix('.'))
        })
        .map(str::to_owned)
        .collect()
}

fn default_preferences(
    schema: &enterprise_protocol::PluginPreferencesSchema,
) -> enterprise_protocol::PluginPreferences {
    enterprise_protocol::PluginPreferences {
        features: schema
            .features
            .iter()
            .map(|preference| (preference.key.clone(), preference.default))
            .collect(),
        storage_directory: schema
            .storage_directory
            .as_ref()
            .and_then(|preference| preference.default.clone()),
    }
}

fn merge_preferences(
    schema: &enterprise_protocol::PluginPreferencesSchema,
    values: enterprise_protocol::PluginPreferences,
) -> enterprise_protocol::PluginPreferences {
    let mut merged = default_preferences(schema);
    for (key, value) in values.features {
        if merged.features.contains_key(&key) {
            merged.features.insert(key, value);
        }
    }
    if schema.storage_directory.is_some() {
        // Defaults apply only when there is no locally saved preference set.
        // An explicit `null` from the editor clears a non-null manifest default.
        merged.storage_directory = values.storage_directory;
    }
    merged
}

/// Single event protocol POM sends to plugins, through the browser SDK and the
/// native `host.event` operation. See `docs/subsystems/plugins-ui.md`, section
/// "Events and notifications".
pub(crate) const PLUGIN_EVENTS_PROTOCOL: &str = "pom-plugin-events/v1";

pub(crate) fn plugin_event_envelope(
    event_type: &str,
    plugin_code: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "protocol": PLUGIN_EVENTS_PROTOCOL,
        "id": uuid::Uuid::new_v4().simple().to_string(),
        "type": event_type,
        "target": plugin_code,
        "source": "pom",
        "at": chrono::Utc::now().to_rfc3339(),
        "payload": payload,
    })
}

/// The editor accepts an absolute directory that does not exist yet: the node
/// creates it before validating, instead of refusing with a generic message.
fn prepare_storage_directory(
    schema: &enterprise_protocol::PluginPreferencesSchema,
    values: &enterprise_protocol::PluginPreferences,
) -> Result<(), String> {
    let (Some(_), Some(directory)) = (&schema.storage_directory, &values.storage_directory) else {
        return Ok(());
    };
    let path = std::path::Path::new(directory);
    if directory.trim().is_empty() || !path.is_absolute() || directory.contains('\0') {
        return Err("the storage directory must be an absolute path".into());
    }
    if !path.exists() {
        std::fs::create_dir_all(path).map_err(|error| {
            format!("could not create the storage directory {directory}: {error}")
        })?;
    }
    Ok(())
}

fn validate_preferences(
    schema: &enterprise_protocol::PluginPreferencesSchema,
    values: &enterprise_protocol::PluginPreferences,
) -> Result<(), String> {
    crate::plugin_github::validate_preferences_schema(schema)?;
    if values.features.keys().any(|key| {
        !schema
            .features
            .iter()
            .any(|preference| preference.key == *key)
    }) {
        return Err("unknown boolean preference for this plugin".into());
    }
    match (&schema.storage_directory, &values.storage_directory) {
        (None, Some(_)) => return Err("this plugin does not accept a storage directory".into()),
        (Some(_), Some(directory)) => {
            if directory.trim().is_empty() || directory.len() > 4096 || directory.contains('\0') {
                return Err("invalid storage directory".into());
            }
            let path = std::path::Path::new(directory);
            if !path.is_absolute() || !path.is_dir() {
                return Err("the storage directory must exist and be absolute".into());
            }
        }
        _ => {}
    }
    Ok(())
}

fn plugin_usage_for_code<'a>(
    code: &str,
    usage: &'a [enterprise_protocol::PluginUsage],
) -> Option<&'a enterprise_protocol::PluginUsage> {
    usage.iter().find(|entry| entry.plugin_code == code)
}

fn update_unavailable(message: impl Into<String>) -> LicenseClientError {
    LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, message)
}

// ---------------------------------------------------------------------------
// Rotas HTTP
// ---------------------------------------------------------------------------

pub(crate) async fn list_plugins(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    axum::Json(state.plugins.all_statuses().await).into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct LicenseActivationRequest {
    pub license_key: String,
}

pub(crate) async fn activate_plugin(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::Json(request): axum::Json<LicenseActivationRequest>,
) -> axum::response::Response {
    match state.plugins.activate(&request.license_key).await {
        Ok(activated) => axum::Json(serde_json::json!({
            "activated": activated,
            "plugins": state.plugins.all_statuses().await,
        }))
        .into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "plugin activation failed");
            license_error_response(error)
        }
    }
}

pub(crate) async fn refresh_plugin(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
) -> axum::response::Response {
    match state.plugins.refresh(&code).await {
        Ok(()) => axum::Json(state.plugins.all_statuses().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn deactivate_plugin(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
) -> axum::response::Response {
    match state.plugins.deactivate(&code).await {
        Ok(()) => axum::Json(state.plugins.all_statuses().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn check_plugin_update(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
) -> axum::response::Response {
    match state.plugins.check_update(&code).await {
        Ok(()) => axum::Json(state.plugins.all_statuses().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PluginUpdateApplyRequest {
    pub release_id: String,
}

pub(crate) async fn apply_plugin_update(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
    axum::Json(request): axum::Json<PluginUpdateApplyRequest>,
) -> axum::response::Response {
    match state.plugins.apply_update(&code, &request.release_id).await {
        Ok(()) => axum::Json(state.plugins.all_statuses().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct GithubRepositoryRequest {
    pub repository_url: String,
}

pub(crate) async fn list_github_repositories(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::Json<Vec<crate::plugin_github::GithubRepository>> {
    axum::Json(state.plugins.github_repositories())
}

pub(crate) async fn list_development_plugin_sources(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    if !crate::pom_dev_mode_enabled() {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "code": "development_disabled" })),
        )
            .into_response();
    }
    axum::Json(state.plugins.development_plugin_sources().await).into_response()
}

pub(crate) async fn rebuild_install_development_plugin(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(repository): axum::extract::Path<String>,
) -> axum::response::Response {
    if !crate::pom_dev_mode_enabled() {
        return (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "code": "development_disabled" })),
        )
            .into_response();
    }
    match state
        .plugins
        .rebuild_and_install_development_plugin(&repository)
        .await
    {
        Ok(result) => axum::Json(result).into_response(),
        Err(error) => {
            let status = match error.code {
                crate::plugin_dev::DevPluginErrorCode::UnknownRepository => {
                    axum::http::StatusCode::NOT_FOUND
                }
                crate::plugin_dev::DevPluginErrorCode::MountMissing => {
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                }
                crate::plugin_dev::DevPluginErrorCode::InvalidSource
                | crate::plugin_dev::DevPluginErrorCode::BuildFailed
                | crate::plugin_dev::DevPluginErrorCode::ArtifactMissing => {
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY
                }
                crate::plugin_dev::DevPluginErrorCode::InstallFailed => {
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                }
            };
            (status, axum::Json(serde_json::json!({ "error": error }))).into_response()
        }
    }
}

pub(crate) async fn add_github_repository(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::Json(request): axum::Json<GithubRepositoryRequest>,
) -> axum::response::Response {
    match state
        .plugins
        .add_github_repository(&request.repository_url)
        .await
    {
        Ok(repository) => axum::Json(repository).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn refresh_github_repository(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    match state.plugins.refresh_github_repository(&id).await {
        Ok(repository) => axum::Json(repository).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn remove_github_repository(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    match state.plugins.remove_github_repository(&id).await {
        Ok(()) => axum::http::StatusCode::NO_CONTENT.into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn install_github_repository_release(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(request): axum::Json<PluginUpdateApplyRequest>,
) -> axum::response::Response {
    match state
        .plugins
        .install_github_repository_release(&id, &request.release_id)
        .await
    {
        Ok(()) => axum::Json(state.plugins.all_statuses().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PluginPreferencesRequest {
    pub preferences: enterprise_protocol::PluginPreferences,
}

pub(crate) async fn save_plugin_preferences(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
    axum::Json(request): axum::Json<PluginPreferencesRequest>,
) -> axum::response::Response {
    match state
        .plugins
        .save_preferences(&code, request.preferences)
        .await
    {
        Ok(()) => axum::Json(state.plugins.all_statuses().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn plugin_peer_fingerprint(
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
            axum::Json(serde_json::json!({"error": "invalid fingerprint challenge"})),
        )
            .into_response();
    }
    let identity = crate::identity::Identity::load_or_create(&crate::identity::key_path());
    if identity.node_id.0 != state.plugins.fingerprint().node_id {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": "peer identity mismatch"})),
        )
            .into_response();
    }
    axum::Json(state.plugins.peer_proof(&challenge, &identity.signing_key)).into_response()
}

pub(crate) fn license_error_response(error: LicenseClientError) -> axum::response::Response {
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
            "manage_url": enterprise_protocol::PLUGIN_MANAGE_URL,
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
    // Tests run with the crate as the project root: no id is written there.
    if cfg!(test) {
        return "test-machine".into();
    }
    stored_machine_id(&crate::config::project_root())
}

/// The machine component where the OS gives none: a random id kept beside the
/// node key. A container has no `/etc/machine-id` and its `HOSTNAME` is the
/// container id, so the fingerprint changed on every recreate and the node
/// dropped its activated plugins at startup.
fn stored_machine_id(root: &std::path::Path) -> String {
    let path = root.join(format!(".{}-machine-id", crate::brand::slug()));
    if let Ok(value) = std::fs::read_to_string(&path) {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_owned();
        }
    }
    let value = hex::encode(rand::random::<[u8; 16]>());
    if let Err(error) = std::fs::write(&path, &value) {
        tracing::warn!(%error, path = %path.display(), "the machine id could not be stored; plugin activations will not survive a restart");
    }
    value
}

fn board_component() -> String {
    std::env::var("POM_BOARD_ID").unwrap_or_else(|_| "unknown-board".into())
}

fn installation_component() -> String {
    std::env::var("POM_INSTALLATION_ID").unwrap_or_else(|_| "unknown-installation".into())
}

/// Request from the storefront screen to buy a plugin (`POST /api/ui/plugins/checkout`).
#[derive(Debug, Deserialize)]
pub(crate) struct PluginCheckoutRequest {
    pub plugin_code: String,
    #[serde(default)]
    pub interval: Option<String>,
    #[serde(default)]
    pub locale: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckoutPlans {
    #[serde(default)]
    plans: Vec<CheckoutPlan>,
}

#[derive(Debug, Deserialize)]
struct CheckoutPlan {
    code: String,
    #[serde(default)]
    plugin_codes: Vec<String>,
    #[serde(default)]
    checkout_available: bool,
}

#[derive(Debug, Deserialize)]
struct CheckoutSession {
    url: String,
}

/// Only a known payment page may receive the browser: Stripe's checkout or, in
/// the server's simulated mode, the license-server itself.
pub(crate) fn checkout_url_allowed(url: &str, license_base: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let license_host = reqwest::Url::parse(license_base)
        .ok()
        .and_then(|base| base.host_str().map(str::to_owned));
    let host = parsed.host_str().unwrap_or_default();
    let https = parsed.scheme() == "https";
    (https && (host == "checkout.stripe.com" || host.ends_with(".stripe.com")))
        || (license_host.as_deref() == Some(host) && (https || cfg!(test)))
}

/// Creates the checkout session on the license-server on behalf of the browser.
/// The public endpoint only allows CORS for the site, and the POM interface runs
/// on each node's origin; the `price_id` is still chosen by the server from the
/// plugin and the interval, never supplied by the client.
pub(crate) async fn create_plugin_checkout(
    client: &reqwest::Client,
    license_base: &str,
    request: &PluginCheckoutRequest,
) -> Result<String, (axum::http::StatusCode, &'static str)> {
    use axum::http::StatusCode;
    let valid_code = {
        let bytes = request.plugin_code.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 64
            && bytes[0].is_ascii_lowercase()
            && bytes
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
    };
    let interval = request.interval.as_deref().unwrap_or("month");
    let locale = request.locale.as_deref().unwrap_or("pt-BR");
    if !valid_code || !matches!(interval, "month" | "year") || !matches!(locale, "pt-BR" | "en") {
        return Err((StatusCode::BAD_REQUEST, "checkout_request_invalid"));
    }
    let base = license_base.trim_end_matches('/');
    let plans: CheckoutPlans = client
        .get(format!("{base}/v1/checkout/plans"))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| (StatusCode::BAD_GATEWAY, "checkout_unavailable"))?
        .json()
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "checkout_unavailable"))?;
    let plan = plans
        .plans
        .iter()
        .find(|plan| {
            plan.checkout_available
                && plan
                    .plugin_codes
                    .iter()
                    .any(|code| code == &request.plugin_code)
        })
        .ok_or((StatusCode::CONFLICT, "checkout_not_offered"))?;
    let session: CheckoutSession = client
        .post(format!("{base}/v1/checkout/sessions"))
        .json(&serde_json::json!({
            "plan_code": plan.code,
            "plugin_code": request.plugin_code,
            "interval": interval,
            "locale": locale,
        }))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| (StatusCode::BAD_GATEWAY, "checkout_unavailable"))?
        .json()
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "checkout_unavailable"))?;
    if !checkout_url_allowed(&session.url, license_base) {
        return Err((StatusCode::BAD_GATEWAY, "checkout_url_rejected"));
    }
    Ok(session.url)
}

pub(crate) async fn start_plugin_checkout(
    _auth: crate::auth::ConnectionAuth,
    axum::Json(request): axum::Json<PluginCheckoutRequest>,
) -> axum::response::Response {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    match create_plugin_checkout(&client, &license_base_url(), &request).await {
        Ok(url) => axum::Json(serde_json::json!({ "url": url })).into_response(),
        Err((status, code)) => (
            status,
            axum::Json(serde_json::json!({ "error": code, "code": code })),
        )
            .into_response(),
    }
}

/// Query of the public plugin store, as the Plugins screen asks it.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct PluginCatalogQuery {
    q: Option<String>,
    page: Option<u32>,
    per_page: Option<u32>,
}

/// Largest store answer passed on to the browser.
const PLUGIN_CATALOG_MAX_BYTES: usize = 4 * 1024 * 1024;

/// The store URL for a listing, or `None` when the query is out of bounds.
/// Only these three parameters are passed on.
fn plugin_catalog_url(base: &str, query: &PluginCatalogQuery) -> Option<String> {
    let mut url = reqwest::Url::parse(&format!("{}/v1/plugins", base.trim_end_matches('/'))).ok()?;
    {
        let mut pairs = url.query_pairs_mut();
        if let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            if q.len() > 200 {
                return None;
            }
            pairs.append_pair("q", q);
        }
        if let Some(page) = query.page {
            if !(1..=10_000).contains(&page) {
                return None;
            }
            pairs.append_pair("page", &page.to_string());
        }
        if let Some(per_page) = query.per_page {
            if !(1..=100).contains(&per_page) {
                return None;
            }
            pairs.append_pair("per_page", &per_page.to_string());
        }
    }
    let url = url.to_string();
    Some(url.strip_suffix('?').map(str::to_owned).unwrap_or(url))
}

/// The store URL of one plugin, or `None` for a code the store cannot hold.
fn plugin_catalog_entry_url(base: &str, code: &str) -> Option<String> {
    let valid = !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    valid.then(|| format!("{}/v1/plugins/{code}", base.trim_end_matches('/')))
}

/// Reads the store for the browser, so the page never leaves its own origin:
/// a page served over plain HTTP from another machine is an origin the store's
/// CORS policy does not know. No credential goes to the store, and its answer
/// comes back as it is, JSON only; the screen validates every field.
async fn relay_plugin_catalog(url: Option<String>) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    let Some(url) = url else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "catalog_query_invalid" })),
        )
            .into_response();
    };
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    });
    let unavailable = || {
        (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({ "error": "catalog_unavailable" })),
        )
            .into_response()
    };
    let Ok(response) = client.get(&url).send().await else {
        return unavailable();
    };
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "error": "plugin_not_listed" })),
        )
            .into_response();
    }
    if !status.is_success()
        || response
            .content_length()
            .is_some_and(|length| length > PLUGIN_CATALOG_MAX_BYTES as u64)
    {
        return unavailable();
    }
    let Ok(body) = response.bytes().await else {
        return unavailable();
    };
    if body.len() > PLUGIN_CATALOG_MAX_BYTES
        || serde_json::from_slice::<serde_json::Value>(&body).is_err()
    {
        return unavailable();
    }
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// `GET /api/ui/plugin-catalog` - the public store's listing, read by the node.
pub(crate) async fn plugin_catalog(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::Query(query): axum::extract::Query<PluginCatalogQuery>,
) -> axum::response::Response {
    relay_plugin_catalog(plugin_catalog_url(&license_base_url(), &query)).await
}

/// `GET /api/ui/plugin-catalog/:code` - one plugin's public store entry.
pub(crate) async fn plugin_catalog_entry(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::Path(code): axum::extract::Path<String>,
) -> axum::response::Response {
    relay_plugin_catalog(plugin_catalog_entry_url(&license_base_url(), &code)).await
}

pub(crate) fn license_base_url() -> String {
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

fn plugin_database_path_for(
    code: &str,
    pom_database_path: Option<&std::path::Path>,
) -> std::path::PathBuf {
    let filename = format!("{code}.db");
    pom_database_path
        .and_then(std::path::Path::parent)
        .map(|directory| directory.join(&filename))
        .unwrap_or_else(|| crate::config::database_dir().join(filename))
}

fn plugin_path(
    code: &str,
    release_id: &str,
    os: &str,
    arch: &str,
) -> Result<std::path::PathBuf, LicenseClientError> {
    // '+' appears in the SemVer build metadata (e.g. "ent-0.1.0+build.1-linux-x86_64"),
    // that the license server's releases carry. It is safe in file
    // names on the three platforms (Linux/macOS/Windows) and does not enable path
    // traversal, since it is not '.'/'/'/'\\'.
    if release_id.is_empty()
        || !release_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
    {
        return Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "invalid plugin release_id",
        ));
    }
    let extension = match os {
        "windows" => "dll",
        "macos" => "dylib",
        _ => "so",
    };
    Ok(crate::config::project_root()
        .join("plugins")
        .join(code)
        .join(format!("{release_id}-{arch}.{extension}")))
}

fn validate_manifest(
    code: &str,
    activation: &ActivationResponse,
    manifest: &enterprise_protocol::PluginManifest,
) -> Result<(), LicenseClientError> {
    if manifest.release_id != activation.plugin_release_id || activation.plugin_code != code {
        return Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "plugin manifest incompatible with the activation",
        ));
    }
    validate_manifest_for_runtime(code, manifest)
}

fn validate_manifest_for_runtime(
    code: &str,
    manifest: &enterprise_protocol::PluginManifest,
) -> Result<(), LicenseClientError> {
    if manifest.product != "pom"
        || manifest.plugin_abi != enterprise_protocol::PLUGIN_ABI_VERSION
        || manifest.os != std::env::consts::OS
        || manifest.arch != std::env::consts::ARCH
        || manifest.signature.trim().is_empty()
        || !is_valid_plugin_code(code)
        || plugin_path(code, &manifest.release_id, &manifest.os, &manifest.arch).is_err()
    {
        return Err(LicenseClientError::new(
            LicenseErrorCode::ServerError,
            "plugin manifest incompatible with this installation",
        ));
    }
    if let Some(preferences) = &manifest.preferences {
        crate::plugin_github::validate_preferences_schema(preferences)
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::ServerError, error))?;
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
            "plugin size mismatch",
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
            "plugin hash mismatch",
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
        .ok_or_else(|| io_error(std::io::Error::other("plugin directory missing")))?;
    std::fs::create_dir_all(parent).map_err(io_error)?;
    let temporary = parent.join(format!(
        ".{}.part-{}",
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("plugin"),
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

fn lease_allows_offline(persisted: &PersistedPluginState) -> bool {
    let Ok(expires_at) = chrono::DateTime::parse_from_rfc3339(&persisted.lease_expires_at) else {
        return false;
    };
    let grace_end = expires_at.with_timezone(&chrono::Utc)
        + chrono::Duration::seconds(persisted.grace_seconds as i64);
    chrono::Utc::now() <= grace_end
}

#[cfg(test)]
fn test_fingerprint_for(platform: &str, arch: &str) -> Fingerprint {
    Fingerprint::from_components(platform, arch, "node-a", "machine", "board", "installation")
}

#[cfg(test)]
fn test_fingerprint() -> Fingerprint {
    // Runtime OS/arch, not a hardcoded pair: license manifest tests compare
    // against the actual build target. GitHub stub tests select their own target.
    test_fingerprint_for(std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
pub(crate) mod tests_support {
    //! Test double of [`LicenseService`] shared by this crate's tests. Unlike the
    //! old single-plugin `MockLicenseService`, this one keeps a response PER
    //! `plugin_code`, so that tests can prove that two plugins activate and fail
    //! independently.
    use super::*;

    #[derive(Clone)]
    pub(crate) struct MockLicenseService {
        responses: Arc<Mutex<HashMap<String, MockResponse>>>,
        heartbeat_error: Arc<Mutex<Option<LicenseErrorCode>>>,
        /// `Some(release_id)` makes the next heartbeat answer
        /// `release_changed: true` and `available_release_id: Some(release_id)`,
        /// as the license server does when it detects a new release.
        heartbeat_release_signal: Arc<Mutex<Option<String>>>,
        artifacts: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        manifest_calls: Arc<Mutex<Vec<String>>>,
        manifest_errors: Arc<Mutex<HashMap<String, LicenseErrorCode>>>,
        download_calls: Arc<Mutex<u32>>,
        deactivate_calls: Arc<Mutex<u32>>,
        deactivate_error: Arc<Mutex<Option<LicenseErrorCode>>>,
        license_expires_at: Arc<Mutex<Option<String>>>,
    }

    #[derive(Clone)]
    enum MockResponse {
        Valid(ActivationResponse),
        Error(LicenseErrorCode),
    }

    impl MockLicenseService {
        pub(crate) fn new() -> Self {
            Self {
                responses: Arc::new(Mutex::new(HashMap::new())),
                heartbeat_error: Arc::new(Mutex::new(None)),
                heartbeat_release_signal: Arc::new(Mutex::new(None)),
                artifacts: Arc::new(Mutex::new(HashMap::new())),
                manifest_calls: Arc::new(Mutex::new(Vec::new())),
                manifest_errors: Arc::new(Mutex::new(HashMap::new())),
                download_calls: Arc::new(Mutex::new(0)),
                deactivate_calls: Arc::new(Mutex::new(0)),
                deactivate_error: Arc::new(Mutex::new(None)),
                license_expires_at: Arc::new(Mutex::new(None)),
            }
        }

        pub(crate) fn single(code: &str, response: ActivationResponse) -> Self {
            let service = Self::new();
            service.grant_sync(code, response, b"plugin-binary".to_vec());
            service
        }

        fn grant_sync(&self, code: &str, response: ActivationResponse, artifact: Vec<u8>) {
            self.responses
                .try_lock()
                .expect("synchronous use in test setup")
                .insert(code.to_owned(), MockResponse::Valid(response));
            self.artifacts
                .try_lock()
                .expect("synchronous use in test setup")
                .insert(code.to_owned(), artifact);
        }

        pub(crate) async fn grant(
            &self,
            code: &str,
            response: ActivationResponse,
            artifact: Vec<u8>,
        ) {
            self.responses
                .lock()
                .await
                .insert(code.to_owned(), MockResponse::Valid(response));
            self.artifacts
                .lock()
                .await
                .insert(code.to_owned(), artifact);
        }

        pub(crate) async fn deny(&self, code: &str, error: LicenseErrorCode) {
            self.responses
                .lock()
                .await
                .insert(code.to_owned(), MockResponse::Error(error));
        }

        pub(crate) async fn set_heartbeat_error(&self, code: LicenseErrorCode) {
            *self.heartbeat_error.lock().await = Some(code);
        }

        pub(crate) async fn signal_heartbeat_release_change(&self, available_release_id: &str) {
            *self.heartbeat_release_signal.lock().await = Some(available_release_id.to_owned());
        }

        pub(crate) async fn clear_heartbeat_error(&self) {
            *self.heartbeat_error.lock().await = None;
        }

        pub(crate) async fn manifest_calls(&self) -> Vec<String> {
            self.manifest_calls.lock().await.clone()
        }

        pub(crate) async fn fail_manifest_for(&self, code: &str, error: LicenseErrorCode) {
            self.manifest_errors
                .lock()
                .await
                .insert(code.to_owned(), error);
        }

        pub(crate) async fn download_calls(&self) -> u32 {
            *self.download_calls.lock().await
        }

        pub(crate) async fn fail_deactivate(&self, code: LicenseErrorCode) {
            *self.deactivate_error.lock().await = Some(code);
        }

        pub(crate) async fn deactivate_calls(&self) -> u32 {
            *self.deactivate_calls.lock().await
        }

        pub(crate) async fn set_license_expires_at(&self, value: Option<&str>) {
            *self.license_expires_at.lock().await = value.map(str::to_owned);
        }
    }

    #[async_trait]
    impl LicenseService for MockLicenseService {
        async fn validate(
            &self,
            request: RequestEnvelope<enterprise_protocol::ValidatePayload>,
        ) -> Result<ResponseEnvelope<enterprise_protocol::ValidateResponse>, LicenseClientError>
        {
            let responses = self.responses.lock().await;
            let licensed_plugins = responses
                .values()
                .filter_map(|response| match response {
                    MockResponse::Valid(response) => Some(response.plugin_code.clone()),
                    MockResponse::Error(_) => None,
                })
                .collect::<Vec<_>>();
            if licensed_plugins.is_empty() {
                if let Some(MockResponse::Error(code)) = responses.values().next() {
                    return Err(LicenseClientError::new(*code, "mock license error"));
                }
            }
            let activation_candidates = responses
                .values()
                .filter_map(|response| match response {
                    MockResponse::Valid(response) => Some(response.activation_candidates.clone()),
                    MockResponse::Error(_) => None,
                })
                .flatten()
                .collect::<Vec<_>>();
            let plugin_usage = responses
                .values()
                .filter_map(|response| match response {
                    MockResponse::Valid(response) => Some(response.plugin_usage.clone()),
                    MockResponse::Error(_) => None,
                })
                .flatten()
                .collect::<Vec<_>>();
            Ok(ResponseEnvelope {
                protocol: enterprise_protocol::LICENSE_PROTOCOL.into(),
                request_id: request.request_id,
                server_time: "2099-01-01T00:00:00Z".into(),
                expires_at: Some("2099-01-08T00:00:00Z".into()),
                payload: enterprise_protocol::ValidateResponse {
                    license_id: "lic-mock".into(),
                    status: "active".into(),
                    licensed_plugins,
                    activation_required: true,
                    license_expires_at: Some("2099-12-31T00:00:00Z".into()),
                    activation_count: Some(1),
                    max_activations: Some(5),
                    plugin_usage,
                    activation_candidates,
                    activation_id: None,
                    eligible_release: None,
                },
            })
        }

        async fn activate(
            &self,
            request: RequestEnvelope<ActivationPayload>,
        ) -> Result<ResponseEnvelope<ActivationResponse>, LicenseClientError> {
            let code = request.payload.plugin_code.clone();
            match self.responses.lock().await.get(&code).cloned() {
                Some(MockResponse::Valid(payload)) => Ok(ResponseEnvelope {
                    protocol: enterprise_protocol::LICENSE_PROTOCOL.into(),
                    request_id: request.request_id,
                    server_time: "2099-01-01T00:00:00Z".into(),
                    expires_at: Some(payload.lease_expires_at.clone()),
                    payload,
                }),
                Some(MockResponse::Error(code)) => {
                    Err(LicenseClientError::new(code, "mock license error"))
                }
                None => Err(LicenseClientError::new(
                    LicenseErrorCode::FeatureNotEntitled,
                    "mock: plugin_code without a registered response",
                )),
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
            let signal = self.heartbeat_release_signal.lock().await.clone();
            let responses = self.responses.lock().await;
            let Some(MockResponse::Valid(response)) = responses
                .values()
                .find(|response| matches!(response, MockResponse::Valid(r) if r.lease_id == request.payload.lease_id))
            else {
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
                    license_id: response.license_id.clone(),
                    activation_id: response.activation_id.clone(),
                    lease_id: response.lease_id.clone(),
                    lease_token: "plt-renewed".into(),
                    licensed_plugins: vec![response.plugin_code.clone()],
                    license_expires_at: Some("2099-12-31T00:00:00Z".into()),
                    activation_count: Some(1),
                    max_activations: Some(5),
                    plugin_usage: response.plugin_usage.clone(),
                    activation_candidates: response.activation_candidates.clone(),
                    lease_expires_at: "2099-01-08T00:00:00Z".into(),
                    heartbeat_due_at: "2099-01-02T00:00:00Z".into(),
                    grace_seconds: 86400,
                    plugin_release_id: signal
                        .clone()
                        .unwrap_or_else(|| response.plugin_release_id.clone()),
                    release_changed: signal.is_some(),
                    available_release_id: signal,
                    grant: response.grant.clone(),
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
            self.manifest_calls
                .lock()
                .await
                .push(plugin_code.to_owned());
            if let Some(error) = self.manifest_errors.lock().await.get(plugin_code) {
                return Err(LicenseClientError::new(*error, "mock manifest error"));
            }
            let responses = self.responses.lock().await;
            match responses.get(plugin_code) {
                Some(MockResponse::Valid(response)) => Ok(ManifestResponse {
                    manifest: response.plugin_manifest.clone(),
                    download: DownloadGrant {
                        token: "mock-download".into(),
                        url: format!("/mock/download/{plugin_code}"),
                        expires_at: "2099-01-01T00:00:00Z".into(),
                    },
                }),
                Some(MockResponse::Error(code)) => {
                    Err(LicenseClientError::new(*code, "mock license error"))
                }
                None => Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    "mock: plugin_code without a registered manifest",
                )),
            }
        }

        async fn download(&self, grant: &DownloadGrant) -> Result<Vec<u8>, LicenseClientError> {
            *self.download_calls.lock().await += 1;
            let code = grant.url.rsplit('/').next().unwrap_or_default().to_owned();
            self.artifacts
                .lock()
                .await
                .get(&code)
                .cloned()
                .ok_or_else(|| {
                    LicenseClientError::new(
                        LicenseErrorCode::ServerError,
                        "mock artifact not configured",
                    )
                })
        }

        async fn deactivate(
            &self,
            _activation_id: &str,
            _activation_token: &str,
            _fingerprint: &Fingerprint,
        ) -> Result<(), LicenseClientError> {
            *self.deactivate_calls.lock().await += 1;
            match *self.deactivate_error.lock().await {
                Some(code) => Err(LicenseClientError::new(code, "deactivation refused")),
                None => Ok(()),
            }
        }

        async fn status(
            &self,
            persisted: &PersistedPluginState,
            _fingerprint: &Fingerprint,
        ) -> Result<ResponseEnvelope<enterprise_protocol::LicenseStatusResponse>, LicenseClientError>
        {
            Ok(ResponseEnvelope {
                protocol: enterprise_protocol::LICENSE_PROTOCOL.into(),
                request_id: "mock-status".into(),
                server_time: "2099-01-01T00:00:00Z".into(),
                expires_at: None,
                payload: enterprise_protocol::LicenseStatusResponse {
                    state: "active".into(),
                    license_id: persisted.license_id.clone(),
                    activation_id: persisted.activation_id.clone(),
                    lease_id: persisted.lease_id.clone(),
                    licensed_plugins: persisted.licensed_plugins.clone(),
                    license_expires_at: self.license_expires_at.lock().await.clone(),
                    activation_count: persisted.activation_count,
                    max_activations: persisted.max_activations,
                    plugin_usage: persisted.plugin_usage.clone(),
                    activation_candidates: persisted.activation_candidates.clone(),
                    lease_expires_at: persisted.lease_expires_at.clone(),
                    grace_ends_at: "2099-01-08T00:00:00Z".into(),
                    plugin_release_id: persisted.installed_release_id.clone(),
                    eligible_release_id: None,
                },
            })
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_stored_machine_id_is_the_same_across_restarts() {
        let root = tempfile::tempdir().unwrap();
        let first = super::stored_machine_id(root.path());
        assert_eq!(first.len(), 32);
        assert_eq!(super::stored_machine_id(root.path()), first);
    }

    #[test]
    fn the_catalog_relay_passes_on_only_a_bounded_query() {
        let base = "https://license.example/";
        let url = |q: Option<&str>, page, per_page| {
            plugin_catalog_url(
                base,
                &PluginCatalogQuery {
                    q: q.map(str::to_owned),
                    page,
                    per_page,
                },
            )
        };
        assert_eq!(
            url(None, None, None).as_deref(),
            Some("https://license.example/v1/plugins")
        );
        assert_eq!(
            url(Some(" rag & logs "), Some(2), Some(10)).as_deref(),
            Some("https://license.example/v1/plugins?q=rag+%26+logs&page=2&per_page=10")
        );
        assert!(url(Some(&"x".repeat(201)), None, None).is_none());
        assert!(url(None, Some(0), None).is_none());
        assert!(url(None, None, Some(101)).is_none());
        assert_eq!(
            plugin_catalog_entry_url(base, "pom-plugin_base2").as_deref(),
            Some("https://license.example/v1/plugins/pom-plugin_base2")
        );
        assert!(plugin_catalog_entry_url(base, "../admin").is_none());
        assert!(plugin_catalog_entry_url(base, "Logs").is_none());
        assert!(plugin_catalog_entry_url(base, "").is_none());
    }

    use super::tests_support::MockLicenseService;
    use super::*;
    use enterprise_protocol::{ActivationResponse, Grant, LicenseErrorCode, PluginManifest};
    use serde_json::Value;

    fn activation(code: &str, release_id: &str) -> ActivationResponse {
        ActivationResponse {
            license_id: "lic-1".into(),
            plugin_code: code.into(),
            activation_id: format!("act-{code}"),
            activation_token: "pat-test".into(),
            lease_id: format!("lease-{code}"),
            lease_token: "plt-test".into(),
            licensed_plugins: vec![code.into()],
            license_expires_at: Some("2099-12-31T00:00:00Z".into()),
            activation_count: Some(1),
            max_activations: Some(5),
            plugin_usage: vec![enterprise_protocol::PluginUsage {
                plugin_code: code.into(),
                activation_count: 1,
                max_activations: 5,
            }],
            activation_candidates: vec![enterprise_protocol::ActivationCandidate {
                plugin_code: "next_plugin".into(),
                display_name: "Next Plugin".into(),
                release_id: "next-1".into(),
                version: "1.0.0".into(),
            }],
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
                feature_set: vec![format!("{code}.request_logs"), format!("{code}.statistics")],
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

    #[test]
    fn manifest_url_uses_the_requested_plugin_code() {
        let service = HttpLicenseService::new("https://license.example");

        let url = service.manifest_url("pom_core", "lease-1", "macos", "aarch64");

        assert_eq!(
            url,
            "https://license.example/v1/plugins/pom_core/manifest?lease_id=lease-1&os=macos&arch=aarch64&channel=stable&host_version=0.1.0&plugin_abi=1"
        );
    }

    #[test]
    fn logs_plugin_database_uses_persistent_pom_database_directory() {
        let database = std::path::Path::new("/data/pom.db");
        assert_eq!(
            plugin_database_path_for("logs", Some(database)),
            std::path::Path::new("/data/logs.db")
        );
        assert_eq!(
            plugin_database_path_for("analytics", Some(database)),
            std::path::Path::new("/data/analytics.db")
        );
    }

    #[test]
    fn plugin_path_accepts_semver_build_metadata_in_release_id() {
        let path = plugin_path("logs", "ent-0.1.0+build.1-linux-x86_64", "linux", "x86_64")
            .expect("a release_id with a build metadata '+' must be accepted");

        assert!(path
            .to_string_lossy()
            .ends_with("plugins/logs/ent-0.1.0+build.1-linux-x86_64-x86_64.so"));
    }

    #[test]
    fn plugin_code_validation_accepts_generic_slugs_and_rejects_path_segments() {
        assert!(is_valid_plugin_code("future.plugin-v2"));
        assert!(is_valid_plugin_code("analytics_plus"));
        assert!(!is_valid_plugin_code(""));
        assert!(!is_valid_plugin_code("../escape"));
        assert!(!is_valid_plugin_code("plugin/name"));
        assert!(!is_valid_plugin_code("plugin%2Fname"));
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

    /// A plugin's features come from the release manifest's `feature_set`, and
    /// each release only declares its own. The `"<plugin_code>."` prefix is
    /// filtered anyway: it is what prevents another plugin's manifest from
    /// granting a feature to this one.
    #[test]
    fn features_for_code_takes_only_the_features_of_that_plugin() {
        let feature_set = vec![
            "logs.request_logs".to_string(),
            "logs.statistics".to_string(),
            "sample.other_feature".to_string(),
        ];

        assert_eq!(
            features_for_code("logs", &feature_set),
            vec!["request_logs".to_string(), "statistics".to_string()]
        );
        assert_eq!(
            features_for_code("sample", &feature_set),
            vec!["other_feature".to_string()]
        );
        assert!(features_for_code("outro", &feature_set).is_empty());
    }

    /// Sets up a "ready" plugin without a real manifest/download/host - the same
    /// two-step split the original suite used, because starting the real host is
    /// integration tested separately.
    async fn activate_ready(runtime: &Arc<PluginRuntime>, license_key: &str, version: &str) {
        runtime.activate(license_key).await.expect("activate");
        runtime
            .mark_plugin_ready(version)
            .await
            .expect("mark ready");
    }

    #[tokio::test]
    async fn a_stopped_plugin_keeps_its_catalogs_and_icon_for_the_menu() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database =
            Arc::new(crate::db::PomDb::open(directory.path().join("pom.db")).expect("database"));
        let runtime = || {
            PluginRuntime::new(
                "jev",
                Arc::new(MockLicenseService::new()),
                test_fingerprint(),
                Some(database.clone()),
                HostContext::default(),
            )
        };
        let catalog = serde_json::json!({ "content_type": "application/json", "base64": "e30=" });
        let first = runtime();
        assert_eq!(first.shell_asset("i18n/en.json"), None);
        first.keep_shell_asset("i18n/en.json", &catalog);
        // A later start, with the process still down, reads what was kept.
        let again = runtime();
        assert_eq!(again.shell_asset("i18n/en.json"), Some(catalog));
        assert_eq!(again.shell_asset("ui/icon.png"), None);
    }

    #[tokio::test]
    async fn plugin_preferences_use_manifest_defaults_and_persist_locally_by_code() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database =
            Arc::new(crate::db::PomDb::open(directory.path().join("pom.db")).expect("database"));
        let service = Arc::new(MockLicenseService::new());
        let mut response = activation("logs", "ent-1");
        response.plugin_manifest.preferences = Some(enterprise_protocol::PluginPreferencesSchema {
            features: vec![enterprise_protocol::PluginBooleanPreference {
                key: "include_headers".into(),
                label: "Include request headers".into(),
                default: false,
                description: Some("Example local toggle".into()),
            }],
            storage_directory: Some(enterprise_protocol::PluginStorageDirectoryPreference {
                label: "Plugin data directory".into(),
                default: None,
            }),
        });
        service
            .grant("logs", response, b"plugin-binary".to_vec())
            .await;
        let manager =
            PluginManager::test_manager_with_database(&["logs"], service.clone(), database.clone());
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-logs", "0.1.0").await;
        let schema = runtime
            .persisted_state()
            .await
            .unwrap()
            .preferences_schema
            .unwrap();
        assert_eq!(
            runtime.preferences_for(&schema),
            enterprise_protocol::PluginPreferences {
                features: [("include_headers".into(), false)].into(),
                storage_directory: None,
            }
        );
        let default_storage = directory.path().join("schema-default");
        std::fs::create_dir(&default_storage).expect("default storage directory");
        let mut schema_with_default = schema.clone();
        schema_with_default.storage_directory =
            Some(enterprise_protocol::PluginStorageDirectoryPreference {
                label: "Default data directory".into(),
                default: Some(default_storage.to_string_lossy().into_owned()),
            });
        assert_eq!(
            default_preferences(&schema_with_default).storage_directory,
            Some(default_storage.to_string_lossy().into_owned())
        );
        assert_eq!(
            merge_preferences(
                &schema_with_default,
                enterprise_protocol::PluginPreferences {
                    features: [("include_headers".into(), false)].into(),
                    storage_directory: None,
                },
            )
            .storage_directory,
            None,
            "an explicit null clears a storage-directory default"
        );

        let storage = directory.path().join("plugin-data");
        std::fs::create_dir(&storage).expect("storage directory");
        let preferences = enterprise_protocol::PluginPreferences {
            features: [("include_headers".into(), true)].into(),
            storage_directory: Some(storage.to_string_lossy().into_owned()),
        };
        let invalid_key = enterprise_protocol::PluginPreferences {
            features: [("unknown_toggle".into(), true)].into(),
            storage_directory: None,
        };
        assert!(manager.save_preferences("logs", invalid_key).await.is_err());
        let invalid_directory = enterprise_protocol::PluginPreferences {
            features: [("include_headers".into(), true)].into(),
            storage_directory: Some("relative/path".into()),
        };
        assert!(manager
            .save_preferences("logs", invalid_directory)
            .await
            .is_err());
        let missing = directory.path().join("created-on-save");
        manager
            .save_preferences(
                "logs",
                enterprise_protocol::PluginPreferences {
                    features: [("include_headers".into(), true)].into(),
                    storage_directory: Some(missing.to_string_lossy().into_owned()),
                },
            )
            .await
            .expect("an absent absolute directory is created on save");
        assert!(missing.is_dir());
        manager
            .save_preferences("logs", preferences.clone())
            .await
            .expect("save local preferences");
        assert_eq!(
            runtime.preferences_for(&schema),
            preferences,
            "local values must override schema defaults"
        );
        let persisted = database
            .local_setting_json("plugin_license:logs")
            .unwrap()
            .expect("license record");
        assert!(!persisted.contains(&storage.to_string_lossy().to_string()));

        let restored = PluginManager::test_manager_with_database(&["logs"], service, database);
        let restored_runtime = restored.runtime_for("logs").expect("restored runtime");
        restored_runtime.restore().await;
        assert_eq!(
            restored_runtime.preferences_for(&schema),
            preferences,
            "local preferences survive a runtime restart"
        );
    }

    #[tokio::test]
    async fn persisted_semver_is_owned_after_the_database_lock_is_released() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant(
                "logs",
                activation("logs", "ent-1"),
                b"plugin-binary".to_vec(),
            )
            .await;
        let manager = PluginManager::test_manager(&["logs"], service);
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-logs", "0.1.0").await;

        let (persisted, version) = runtime
            .persisted_with_semver()
            .await
            .expect("owned installed version");
        assert_eq!(version, semver::Version::new(0, 1, 0));
        assert_eq!(version.to_string(), persisted.plugin_version);
    }

    #[tokio::test]
    async fn public_install_state_is_persisted_separately_from_license_entitlement() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database =
            Arc::new(crate::db::PomDb::open(directory.path().join("pom.db")).expect("database"));
        let service = Arc::new(MockLicenseService::new());
        let installed = PublicPluginInstallState {
            fingerprint: test_fingerprint(),
            repository_id: None,
            repository_url: None,
            installed_release_id: "github-v1.0.0-deadbeef".into(),
            plugin_version: "1.0.0".into(),
            features: vec!["free_feature".into()],
            plugin_sha256: "sha256:deadbeef".into(),
            size: 4,
            plugin_path: directory
                .path()
                .join("free-plugin.so")
                .to_string_lossy()
                .into_owned(),
            preferences_schema: None,
            installed_at: "2026-09-24T00:00:00Z".into(),
        };
        database
            .save_local_setting("plugin_public_install:free_plugin", &installed)
            .expect("persist public install");

        let manager =
            PluginManager::test_manager_with_database(&["free_plugin"], service, database.clone());
        let status = manager.status("free_plugin").await.expect("status");
        assert!(status.public_install);
        assert!(
            !status.active,
            "restoration does not claim the host is running yet"
        );
        assert!(
            status.licensed_plugins.is_empty(),
            "a free install grants no license codes"
        );
        assert_eq!(status.plugin_version.as_deref(), Some("1.0.0"));
        assert!(database
            .local_setting_json("plugin_license:free_plugin")
            .expect("license lookup")
            .is_none());
    }

    #[tokio::test]
    async fn github_repository_install_uses_the_selected_release_without_license_server_checks() {
        let _serial = REAL_HOST.lock().await;
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("tempdir");
        let database =
            Arc::new(crate::db::PomDb::open(directory.path().join("pom.db")).expect("database"));
        let (_client, api_base, server) =
            crate::plugin_github::tests::github_repository_stub().await;
        let host_log = directory.path().join("fake-host.log");
        let _restore_environment = RestoreEnvironment::capture(&[
            "POM_HOME",
            "POM_PLUGIN_HOST",
            "POM_FAKE_HOST_LOG",
            "POM_FAKE_HOST_UPSTREAM",
        ]);
        std::env::set_var("POM_HOME", directory.path());
        std::env::set_var("POM_PLUGIN_HOST", fake_host_binary());
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);
        std::env::set_var(
            "POM_FAKE_HOST_UPSTREAM",
            serde_json::json!({"status": "ready", "port": 43127, "token": "t".repeat(32)})
                .to_string(),
        );

        let service = Arc::new(MockLicenseService::new());
        let manager = PluginManager::test_manager_with_github_database(
            &[],
            service.clone(),
            database.clone(),
            &api_base,
        );
        let repository = manager
            .add_github_repository("https://github.com/acme/new-plugin")
            .await
            .expect("add repository from its root manifest");
        assert_eq!(repository.manifest.plugin_code, "acme_plugin");
        assert_eq!(manager.github_repositories().len(), 1);
        assert!(database
            .local_setting_json(GITHUB_REPOSITORIES_SETTING)
            .expect("local GitHub source setting")
            .is_some());

        manager
            .install_github_repository_release(&repository.id, "11")
            .await
            .expect("install selected historical release");
        let runtime = manager.runtime_for("acme_plugin").expect("plugin runtime");
        let status = manager.status("acme_plugin").await.expect("plugin status");
        assert!(status.active);
        assert!(status.public_install);
        assert!(status.licensed_plugins.is_empty());
        assert_eq!(status.plugin_version.as_deref(), Some("v0.9.0"));
        assert_eq!(
            status.github_repository.as_deref(),
            Some("https://github.com/acme/new-plugin")
        );
        assert!(runtime.persisted_state().await.is_none());
        let installed = runtime
            .public_install
            .lock()
            .await
            .clone()
            .expect("local public install record");
        assert_eq!(
            installed.repository_id.as_deref(),
            Some(repository.id.as_str())
        );
        assert_eq!(
            std::fs::read(&installed.plugin_path).expect("installed library"),
            b"older plugin binary"
        );
        assert!(database
            .local_setting_json("plugin_license:acme_plugin")
            .expect("license record")
            .is_none());
        assert!(database
            .local_setting_json("plugin_public_install:acme_plugin")
            .expect("public install record")
            .is_some());
        assert!(service.manifest_calls().await.is_empty());
        assert_eq!(service.deactivate_calls().await, 0);

        manager
            .deactivate("acme_plugin")
            .await
            .expect("uninstall public plugin locally");
        assert!(!manager.status("acme_plugin").await.unwrap().active);
        assert!(database
            .local_setting_json("plugin_public_install:acme_plugin")
            .expect("removed public install state")
            .is_none());
        assert!(service.manifest_calls().await.is_empty());
        assert_eq!(service.deactivate_calls().await, 0);
        server.abort();
    }

    #[tokio::test]
    async fn github_ui_manifest_repository_installs_its_platform_release_without_license_calls() {
        let _serial = REAL_HOST.lock().await;
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("tempdir");
        let database =
            Arc::new(crate::db::PomDb::open(directory.path().join("pom.db")).expect("database"));
        let (_client, api_base, server) =
            crate::plugin_github::tests::github_ui_manifest_repository_stub().await;
        let host_log = directory.path().join("fake-host.log");
        let _restore_environment = RestoreEnvironment::capture(&[
            "POM_HOME",
            "POM_PLUGIN_HOST",
            "POM_FAKE_HOST_LOG",
            "POM_FAKE_HOST_UPSTREAM",
        ]);
        std::env::set_var("POM_HOME", directory.path());
        std::env::set_var("POM_PLUGIN_HOST", fake_host_binary());
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);
        std::env::set_var(
            "POM_FAKE_HOST_UPSTREAM",
            serde_json::json!({"status": "ready", "port": 43127, "token": "t".repeat(32)})
                .to_string(),
        );

        let service = Arc::new(MockLicenseService::new());
        let manager = PluginManager::test_manager_with_github_database_for_target(
            &[],
            service.clone(),
            database.clone(),
            &api_base,
            "macos",
            "aarch64",
        );
        let repository = manager
            .add_github_repository("https://github.com/pereirajair/pom-plugin-base")
            .await
            .expect("find ui/manifest.json and read the platform release manifest");
        assert_eq!(repository.manifest_path, "ui/manifest.json");
        assert_eq!(repository.manifest.plugin_code, "base");
        manager
            .install_github_repository_release(&repository.id, "505")
            .await
            .expect("install the selected platform release");

        let runtime = manager.runtime_for("base").expect("plugin runtime");
        let status = manager.status("base").await.expect("plugin status");
        assert!(status.active);
        assert!(status.public_install);
        assert!(status.licensed_plugins.is_empty());
        assert_eq!(status.plugin_version.as_deref(), Some("0.0.5"));
        let installed = runtime
            .public_install
            .lock()
            .await
            .clone()
            .expect("local public install record");
        assert_eq!(installed.features, ["core"]);
        assert_eq!(
            installed.preferences_schema.as_ref().unwrap().features[0].key,
            "example_feature_enabled"
        );
        assert_eq!(
            std::fs::read(&installed.plugin_path).expect("installed library"),
            b"base plugin binary"
        );
        assert!(database
            .local_setting_json("plugin_license:base")
            .expect("license record")
            .is_none());
        assert!(service.manifest_calls().await.is_empty());
        assert_eq!(service.deactivate_calls().await, 0);
        server.abort();
    }

    #[tokio::test]
    async fn deactivate_clears_an_activation_the_server_does_not_know() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant(
                "logs",
                activation("logs", "ent-1"),
                b"plugin-binary".to_vec(),
            )
            .await;
        let manager = PluginManager::test_manager(&["logs"], service.clone());
        let logs = manager.runtime_for("logs").expect("runtime");
        activate_ready(&logs, "PM-logs", "0.1.0").await;

        service.fail_deactivate(LicenseErrorCode::ServerError).await;
        assert!(manager.deactivate("logs").await.is_err());
        assert!(
            manager.status("logs").await.expect("status").active,
            "a regular server failure keeps the license"
        );

        let unknown = parse_license_error(
            StatusCode::NOT_FOUND,
            br#"{"code":"ACTIVATION_UNKNOWN","message_for_user":"This activation does not exist."}"#,
            "deactivate",
        );
        service.fail_deactivate(unknown.code()).await;
        manager
            .deactivate("logs")
            .await
            .expect("an activation unknown to the server is removed locally");
        assert!(!manager.status("logs").await.expect("status").active);
    }

    #[tokio::test]
    async fn two_plugins_activate_and_deactivate_independently() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant(
                "logs",
                activation("logs", "ent-1"),
                b"plugin-binary".to_vec(),
            )
            .await;
        service
            .grant(
                "sample",
                activation("sample", "sample-1"),
                b"plugin-binary".to_vec(),
            )
            .await;
        let manager = PluginManager::test_manager(&["logs", "sample"], service.clone());
        let logs = manager.runtime_for("logs").expect("runtime");
        let sample = manager.runtime_for("sample").expect("runtime");
        activate_ready(&logs, "PM-logs", "0.1.0").await;
        activate_ready(&sample, "PM-sample", "0.1.0").await;

        assert!(manager.status("logs").await.expect("status").active);
        assert!(manager.status("sample").await.expect("status").active);

        manager.deactivate("logs").await.expect("deactivate logs");

        assert!(!manager.status("logs").await.expect("status").active);
        assert!(
            manager.status("sample").await.expect("status").active,
            "revoking one plugin cannot deactivate the other"
        );
    }

    #[tokio::test]
    async fn activation_without_a_granted_plugin_returns_feature_not_entitled() {
        let service = Arc::new(MockLicenseService::new());
        let manager = PluginManager::test_manager(&[], service);

        let error = manager
            .activate("PM-without-plugin-entitlement")
            .await
            .expect_err("must reject a key granting no plugins");

        assert_eq!(error.code(), LicenseErrorCode::FeatureNotEntitled);
    }

    #[tokio::test]
    async fn plugin_manager_starts_without_a_compiled_in_plugin_catalog() {
        let manager = PluginManager::test_manager(&[], Arc::new(MockLicenseService::new()));

        assert!(manager.all_statuses().await.is_empty());
    }

    #[tokio::test]
    async fn production_manager_restores_any_persisted_plugin_code() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = Arc::new(
            crate::db::PomDb::open(directory.path().join("pom.db"))
                .expect("open the test database"),
        );
        database
            .save_local_setting("plugin_license:future.plugin-v2", &serde_json::json!({}))
            .expect("save persisted plugin code");

        let manager = PluginManager::runtime_with_database("node-dynamic", database);

        assert!(manager.runtime_for("future.plugin-v2").is_some());
        assert!(manager.runtime_for(LOGS_PLUGIN_CODE).is_none());
    }

    #[tokio::test]
    async fn activation_recognizes_a_license_granted_only_to_base() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant(
                "base",
                activation("base", "base-1"),
                b"plugin-binary".to_vec(),
            )
            .await;
        service
            .fail_manifest_for("base", LicenseErrorCode::ServerError)
            .await;
        let manager = PluginManager::test_manager(&[], service.clone());

        let error = manager
            .activate("PM-base-only")
            .await
            .expect_err("the test fails at manifest lookup after selecting base");

        assert_eq!(error.code(), LicenseErrorCode::ServerError);
        assert_eq!(service.manifest_calls().await, ["base"]);
    }

    #[tokio::test]
    async fn activation_batches_logs_and_base_without_a_compiled_catalog() {
        let service = Arc::new(MockLicenseService::new());
        for (code, activation_id) in [("logs", "ent-1"), ("base", "base-1")] {
            service
                .grant(
                    code,
                    activation(code, activation_id),
                    b"plugin-binary".to_vec(),
                )
                .await;
            service
                .fail_manifest_for(code, LicenseErrorCode::ServerError)
                .await;
        }
        let manager = PluginManager::test_manager(&[], service.clone());

        let error = manager
            .activate("PM-logs-and-base")
            .await
            .expect_err("both mock manifests fail after their codes are selected");

        assert_eq!(error.code(), LicenseErrorCode::ServerError);
        let mut calls = service.manifest_calls().await;
        calls.sort();
        assert_eq!(calls, ["base", "logs"]);
        assert!(manager.runtime_for("base").is_some());
        assert!(manager.runtime_for("logs").is_some());
    }

    #[tokio::test]
    async fn activation_uses_the_same_per_plugin_path_for_an_arbitrary_plugin_code() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant(
                "future_plugin_x",
                activation("future_plugin_x", "future-1"),
                b"plugin-binary".to_vec(),
            )
            .await;
        service
            .fail_manifest_for("future_plugin_x", LicenseErrorCode::ServerError)
            .await;
        let manager = PluginManager::test_manager(&[], service.clone());

        let error = manager
            .activate("PM-future-plugin-only")
            .await
            .expect_err("the test fails at manifest lookup after selecting the arbitrary code");

        assert_eq!(error.code(), LicenseErrorCode::ServerError);
        assert_eq!(service.manifest_calls().await, ["future_plugin_x"]);
    }

    #[tokio::test]
    async fn plugin_status_exposes_authenticated_license_usage_and_candidates() {
        let service = Arc::new(MockLicenseService::single(
            "analytics_plus",
            activation("analytics_plus", "ap-1"),
        ));
        let manager = PluginManager::test_manager(&["analytics_plus"], service);
        let runtime = manager.runtime_for("analytics_plus").expect("runtime");

        runtime.activate("PM-test-key").await.expect("activate");

        let status = manager.status("analytics_plus").await.expect("status");
        assert_eq!(
            status.license_expires_at.as_deref(),
            Some("2099-12-31T00:00:00Z")
        );
        assert_eq!(status.activation_count, Some(1));
        assert_eq!(status.max_activations, Some(5));
        assert_eq!(status.licensed_plugins, ["analytics_plus"]);
        assert_eq!(status.activation_candidates[0].plugin_code, "next_plugin");
        let status_json = serde_json::to_value(status).expect("plugin status JSON");
        assert_eq!(
            status_json["activation_candidates"],
            serde_json::json!([{
                "plugin_code": "next_plugin",
                "display_name": "Next Plugin",
                "release_id": "next-1",
                "version": "1.0.0"
            }])
        );
        assert!(!status_json.to_string().contains("token"));
    }

    #[tokio::test]
    async fn invalid_license_keeps_the_plugin_inactive_and_returns_a_stable_error() {
        let service = Arc::new(MockLicenseService::new());
        service.deny("logs", LicenseErrorCode::InvalidKey).await;
        let manager = PluginManager::test_manager(&["logs"], service);

        let error = manager
            .activate("PM-invalid")
            .await
            .expect_err("must reject");

        assert_eq!(error.code(), LicenseErrorCode::InvalidKey);
        assert!(!manager.status("logs").await.expect("status").active);
    }

    #[tokio::test]
    async fn heartbeat_persists_authoritative_lease_data() {
        let service = Arc::new(MockLicenseService::single(
            "logs",
            activation("logs", "ent-1"),
        ));
        let manager = PluginManager::test_manager(&["logs"], service);
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;

        runtime
            .heartbeat("heartbeat-nonce")
            .await
            .expect("heartbeat");

        let persisted = runtime.persisted_state().await.expect("persisted state");
        assert_eq!(persisted.lease_token, "plt-renewed");
        assert_eq!(persisted.lease_expires_at, "2099-01-08T00:00:00Z");
        assert_eq!(persisted.grace_seconds, 86400);
        assert_eq!(
            persisted.license_expires_at.as_deref(),
            Some("2099-12-31T00:00:00Z")
        );
        assert_eq!(persisted.activation_count, Some(1));
        assert_eq!(persisted.max_activations, Some(5));
        assert_eq!(persisted.licensed_plugins, ["logs"]);
        assert_eq!(
            persisted.activation_candidates[0].plugin_code,
            "next_plugin"
        );

        let status = manager.status("logs").await.expect("status");
        assert_eq!(status.state, LicenseState::Active);
        assert_eq!(
            status.lease_expires_at.as_deref(),
            Some("2099-01-08T00:00:00Z")
        );
        assert_eq!(
            status.license_expires_at.as_deref(),
            Some("2099-12-31T00:00:00Z")
        );
        assert_eq!(status.activation_count, Some(1));
        assert_eq!(status.max_activations, Some(5));
    }

    /// The manual click on "Update" (`PluginManager::refresh`) publishes
    /// `license_expires_at` from `/v1/licenses/status` - the validity of the
    /// LICENSE, which the heartbeat alone does not bring (it only renews the
    /// technical `lease_expires_at`). The two dates are deliberately different
    /// here to prove that the screen never shows the lease in place of the
    /// license.
    #[tokio::test]
    async fn refresh_publishes_the_license_expiry_from_the_status_endpoint() {
        let service = Arc::new(MockLicenseService::single(
            "logs",
            activation("logs", "ent-1"),
        ));
        service
            .set_license_expires_at(Some("2027-09-15T00:00:00.000Z"))
            .await;
        let manager = PluginManager::test_manager(&["logs"], service);
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;
        assert_eq!(
            runtime.status().await.license_expires_at.as_deref(),
            Some("2099-12-31T00:00:00Z")
        );

        manager.refresh("logs").await.expect("refresh");

        let status = manager.status("logs").await.expect("status");
        assert_eq!(
            status.license_expires_at.as_deref(),
            Some("2027-09-15T00:00:00.000Z")
        );
        assert_ne!(status.license_expires_at, status.lease_expires_at);
    }

    #[tokio::test]
    async fn transient_heartbeat_failure_enters_degraded_without_deactivating() {
        let service = Arc::new(MockLicenseService::single(
            "logs",
            activation("logs", "ent-1"),
        ));
        let manager = PluginManager::test_manager(&["logs"], service);
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;

        runtime.mark_heartbeat_failure(true).await;

        let status = runtime.status().await;
        assert_eq!(status.state, LicenseState::Degraded);
        assert!(
            status.active,
            "a transient failure does not deactivate the plugin"
        );
    }

    #[tokio::test]
    async fn revoked_heartbeat_disables_only_that_plugin() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant(
                "logs",
                activation("logs", "ent-1"),
                b"plugin-binary".to_vec(),
            )
            .await;
        service
            .grant(
                "sample",
                activation("sample", "sample-1"),
                b"plugin-binary".to_vec(),
            )
            .await;
        let manager = PluginManager::test_manager(&["logs", "sample"], service.clone());
        let logs = manager.runtime_for("logs").expect("runtime");
        let sample = manager.runtime_for("sample").expect("runtime");
        activate_ready(&logs, "PM-logs", "0.1.0").await;
        activate_ready(&sample, "PM-sample", "0.1.0").await;
        service.set_heartbeat_error(LicenseErrorCode::Revoked).await;

        let runtime = manager.runtime_for("logs").expect("runtime");
        let error = runtime.heartbeat("nonce-1").await.expect_err("revoked");

        assert_eq!(error.code(), LicenseErrorCode::Revoked);
        assert!(!manager.status("logs").await.expect("status").active);
        assert!(
            manager.status("sample").await.expect("status").active,
            "the heartbeat of one plugin must not affect the other"
        );
    }

    #[tokio::test]
    async fn persisted_state_never_contains_the_license_key() {
        let service = Arc::new(MockLicenseService::single(
            "logs",
            activation("logs", "ent-1"),
        ));
        let manager = PluginManager::test_manager(&["logs"], service);
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-secret-license", "0.1.0").await;

        let persisted = serde_json::to_vec(&runtime.persisted_state().await.expect("persisted"))
            .expect("state JSON");

        assert!(!String::from_utf8_lossy(&persisted).contains("PM-secret-license"));
        assert!(String::from_utf8_lossy(&persisted).contains("lease-logs"));
    }

    /// `apply_update` really downloading and restarting the host is integration
    /// tested separately (it needs the real `pom-plugin-host` binary); here only
    /// `detect_update` runs end to end, and `apply_update`'s rejection of an
    /// unconfirmed release is the other branch that touches neither download nor
    /// host.
    #[tokio::test]
    async fn detects_a_plugins_own_update_without_touching_pom_core() {
        let mut updated = activation("logs", "ent-1");
        let service = Arc::new(MockLicenseService::single("logs", updated.clone()));
        let manager = PluginManager::test_manager(&["logs"], service.clone());
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;
        assert!(runtime.status().await.available_update.is_none());
        let installed_at = "2026-09-20T10:15:00.000Z".to_owned();
        runtime
            .persisted
            .lock()
            .await
            .as_mut()
            .expect("persisted")
            .installed_at = Some(installed_at.clone());
        runtime.status.write().await.installed_at = Some(installed_at.clone());

        updated.plugin_release_id = "ent-2".into();
        updated.plugin_manifest.release_id = "ent-2".into();
        updated.plugin_manifest.version = "0.2.0".into();
        service
            .grant("logs", updated, b"plugin-binary".to_vec())
            .await;

        runtime.detect_update().await.expect("detect update");
        let available = runtime
            .status()
            .await
            .available_update
            .expect("update should be advertised");
        assert_eq!(available.release_id, "ent-2");
        assert_eq!(available.version, "0.2.0");
        let status = runtime.status().await;
        let checked_at = status
            .update_checked_at
            .expect("successful manifest check timestamp");
        assert!(chrono::DateTime::parse_from_rfc3339(&checked_at).is_ok());
        assert_eq!(status.installed_at.as_deref(), Some(installed_at.as_str()));
        assert_eq!(
            runtime
                .persisted_state()
                .await
                .expect("persisted")
                .installed_at
                .as_deref(),
            Some(installed_at.as_str())
        );
    }

    /// Reproduces the bug reported by the captain: a successful heartbeat must not
    /// erase a real update, even when the server's release_id only changes in the
    /// SemVer build metadata (`+build.N`) - a format the string equality
    /// comparison already handled correctly, but which the heartbeat corrupted by
    /// writing its own `plugin_release_id` as if it were the installed release,
    /// before the following `detect_update` (same click on "Update",
    /// `PluginManager::refresh`) compared against that already tampered baseline.
    /// Without revoking the previous release at any moment.
    #[tokio::test]
    async fn refresh_button_does_not_lose_an_update_signalled_by_heartbeat() {
        let mut updated = activation("logs", "ent-0.1.0+build.1-linux-x86_64");
        let service = Arc::new(MockLicenseService::single("logs", updated.clone()));
        let manager = PluginManager::test_manager(&["logs"], service.clone());
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;
        runtime
            .mock_host_ready
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(runtime.status().await.available_update.is_none());

        // The server publishes the next release, with only the build metadata
        // advanced - never revoked, never deactivated.
        updated.plugin_manifest.release_id = "ent-0.1.0+build.2-linux-x86_64".into();
        updated.plugin_manifest.version = "0.1.0+build.2".into();
        service
            .grant("logs", updated, b"plugin-binary".to_vec())
            .await;
        // The server's heartbeat already signals the change, and (as in the
        // reported incident) its own `plugin_release_id` field also advances to
        // the new release - without anything having been downloaded yet.
        service
            .signal_heartbeat_release_change("ent-0.1.0+build.2-linux-x86_64")
            .await;

        manager.refresh("logs").await.expect("refresh");

        let available = runtime
            .status()
            .await
            .available_update
            .expect("the heartbeat cannot erase the real update");
        assert_eq!(available.release_id, "ent-0.1.0+build.2-linux-x86_64");
        assert_eq!(available.version, "0.1.0+build.2");
        // `installed_release_id` only advances with a confirmed download - the
        // heartbeat alone can never claim to have done one.
        assert_eq!(
            runtime
                .persisted_state()
                .await
                .expect("persisted")
                .installed_release_id,
            "ent-0.1.0+build.1-linux-x86_64",
        );
    }

    /// The periodic heartbeat supervisor (`spawn_heartbeat_supervisor`) never
    /// called `detect_update` on its own; a release signaled in the background,
    /// without any click from the operator, must also show up.
    #[tokio::test]
    async fn a_background_heartbeat_alone_also_surfaces_the_update() {
        let mut updated = activation("logs", "ent-1");
        let service = Arc::new(MockLicenseService::single("logs", updated.clone()));
        let manager = PluginManager::test_manager(&["logs"], service.clone());
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;
        runtime
            .mock_host_ready
            .store(true, std::sync::atomic::Ordering::Relaxed);

        updated.plugin_manifest.release_id = "ent-2".into();
        updated.plugin_manifest.version = "0.2.0".into();
        service
            .grant("logs", updated, b"plugin-binary".to_vec())
            .await;
        service.signal_heartbeat_release_change("ent-2").await;

        runtime.heartbeat("nonce-1").await.expect("heartbeat");

        let available = runtime
            .status()
            .await
            .available_update
            .expect("the release flagged in the background must show up");
        assert_eq!(available.release_id, "ent-2");
        assert_eq!(
            runtime
                .persisted_state()
                .await
                .expect("persisted")
                .installed_release_id,
            "ent-1",
        );
    }

    #[tokio::test]
    async fn applying_a_different_release_than_advertised_is_rejected_without_downloading() {
        let mut updated = activation("logs", "ent-1");
        let service = Arc::new(MockLicenseService::single("logs", updated.clone()));
        let manager = PluginManager::test_manager(&["logs"], service.clone());
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;

        updated.plugin_release_id = "ent-2".into();
        updated.plugin_manifest.release_id = "ent-2".into();
        service
            .grant("logs", updated, b"plugin-binary".to_vec())
            .await;
        runtime.detect_update().await.expect("detect update");

        let error = runtime
            .apply_update("ent-not-confirmed")
            .await
            .expect_err("must require the advertised release");

        assert_eq!(error.code(), LicenseErrorCode::UpdateUnavailable);
        assert_eq!(service.download_calls().await, 0);
    }
    /// Reproduces the bug reported by the captain: a revocation stops the host of
    /// ONE plugin; the next successful heartbeat (self-heal, without the whole
    /// `pom` process restarting) must bring it back up, otherwise Request
    /// Logs/Statistics stay mute indefinitely until the next full restart.
    /// Tests that start a real host change `POM_PLUGIN_HOST` and the fake host's
    /// log, process-wide variables: they run one at a time.
    static REAL_HOST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct RestoreEnvironment(Vec<(String, Option<std::ffi::OsString>)>);

    impl RestoreEnvironment {
        fn capture(keys: &[&str]) -> Self {
            Self(
                keys.iter()
                    .map(|key| ((*key).to_owned(), std::env::var_os(key)))
                    .collect(),
            )
        }
    }

    impl Drop for RestoreEnvironment {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    fn fake_host_binary() -> std::path::PathBuf {
        let fake_host = std::env::current_exe()
            .expect("locate the test binary")
            .parent()
            .and_then(|deps| deps.parent())
            .expect("locate the target/<profile> directory")
            .join(if cfg!(windows) {
                "pom-fake-enterprise-plugin-host.exe"
            } else {
                "pom-fake-enterprise-plugin-host"
            });
        assert!(
            fake_host.is_file(),
            "helper binary {} was not built; run `cargo test -p node` (not `--lib`) to produce all the [[bin]] targets",
            fake_host.display()
        );
        fake_host
    }

    #[tokio::test]
    async fn a_local_development_install_runs_without_granting_or_deactivating_a_license() {
        let _serial = REAL_HOST.lock().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let _environment = RestoreEnvironment::capture(&[
            "POM_HOME",
            "POM_PLUGIN_HOST",
            "POM_FAKE_HOST_LOG",
            "POM_FAKE_HOST_UPSTREAM",
            "POM_FAKE_HOST_READY_ERROR",
        ]);
        let host_log = directory.path().join("host.log");
        let data_dir = directory.path().join("data");
        std::fs::create_dir(&data_dir).expect("create persistent directory");
        std::env::set_var("POM_HOME", directory.path());
        std::env::set_var("POM_PLUGIN_HOST", fake_host_binary());
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);
        std::env::remove_var("POM_FAKE_HOST_UPSTREAM");
        std::env::remove_var("POM_FAKE_HOST_READY_ERROR");

        let database =
            Arc::new(crate::db::PomDb::open(data_dir.join("pom.db")).expect("open POM database"));
        let service = Arc::new(MockLicenseService::new());
        let manager = PluginManager::test_manager_with_database(
            &["local_plugin"],
            service.clone(),
            database.clone(),
        );
        let runtime = manager.runtime_for("local_plugin").expect("runtime");

        runtime
            .install_development_artifact(b"local development artifact")
            .await
            .expect("install local development build");

        let status = runtime.status().await;
        assert!(status.active, "the local plugin host should be ready");
        assert!(status.development_install);
        assert_eq!(status.state, LicenseState::NeedsActivation);
        assert!(status.features.is_empty());
        assert!(status.licensed_plugins.is_empty());
        assert!(database
            .local_setting_json("plugin_dev_install:local_plugin")
            .expect("development install lookup")
            .is_some());
        assert!(database
            .local_setting_json("plugin_license:local_plugin")
            .expect("license lookup")
            .is_none());

        runtime.deactivate().await.expect("uninstall local build");
        assert_eq!(service.deactivate_calls().await, 0);
        assert!(database
            .local_setting_json("plugin_dev_install:local_plugin")
            .expect("development install lookup")
            .is_none());
        assert!(!runtime.status().await.active);
    }

    #[tokio::test]
    async fn logs_activation_passes_logs_db_in_the_pom_database_directory() {
        let _serial = REAL_HOST.lock().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let _environment = RestoreEnvironment::capture(&[
            "POM_HOME",
            "POM_PLUGIN_HOST",
            "POM_FAKE_HOST_LOG",
            "POM_FAKE_HOST_UPSTREAM",
            "POM_FAKE_HOST_READY_ERROR",
        ]);
        let host_log = directory.path().join("host.log");
        let data_dir = directory.path().join("data");
        std::fs::create_dir(&data_dir).expect("create the persistent directory");
        std::env::set_var("POM_HOME", directory.path());
        std::env::set_var("POM_PLUGIN_HOST", fake_host_binary());
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);
        std::env::remove_var("POM_FAKE_HOST_UPSTREAM");
        std::env::remove_var("POM_FAKE_HOST_READY_ERROR");

        let database = Arc::new(
            crate::db::PomDb::open(data_dir.join("pom.db")).expect("open the POM database"),
        );
        let service = Arc::new(MockLicenseService::single(
            "logs",
            activation("logs", "ent-1"),
        ));
        let manager = PluginManager::test_manager_with_database(&["logs"], service, database);

        assert_eq!(
            manager
                .activate("PM-test-key")
                .await
                .expect("activate plugin"),
            vec!["logs"]
        );
        let expected_path = data_dir.join("logs.db");
        let host_log = std::fs::read_to_string(&host_log).expect("ler log do host");
        assert!(host_log.contains(&format!("POM_PLUGIN_DB={}", expected_path.display())));
        assert!(!host_log.contains("enterprise.db"));
        manager
            .runtime_for("logs")
            .expect("runtime")
            .stop_host_for_test()
            .await;
    }

    #[tokio::test]
    async fn failed_plugin_host_handshake_keeps_license_valid_but_not_ready() {
        let _serial = REAL_HOST.lock().await;
        let directory = tempfile::tempdir().expect("tempdir");
        let _environment = RestoreEnvironment::capture(&[
            "POM_HOME",
            "POM_PLUGIN_HOST",
            "POM_FAKE_HOST_LOG",
            "POM_FAKE_HOST_UPSTREAM",
            "POM_FAKE_HOST_READY_ERROR",
        ]);
        let host_log = directory.path().join("host.log");
        std::env::set_var("POM_HOME", directory.path());
        std::env::set_var("POM_PLUGIN_HOST", fake_host_binary());
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);
        std::env::remove_var("POM_FAKE_HOST_UPSTREAM");
        std::env::set_var(
            "POM_FAKE_HOST_READY_ERROR",
            "plugin refused the configuration",
        );

        let database = Arc::new(
            crate::db::PomDb::open(directory.path().join("pom.db")).expect("open the POM database"),
        );
        let service = Arc::new(MockLicenseService::single(
            "logs",
            activation("logs", "ent-1"),
        ));
        let manager =
            PluginManager::test_manager_with_database(&["logs"], service, database.clone());

        let error = manager
            .activate("PM-test-key")
            .await
            .expect_err("a refused handshake must fail the installation");
        let status = manager.status("logs").await.expect("status");
        assert_eq!(
            error.code(),
            LicenseErrorCode::ServerError,
            "an installation failure must come back as an error: {error}"
        );
        assert_eq!(status.state, LicenseState::Active, "license stays valid");
        assert!(
            !status.active,
            "a refused handshake does not leave the plugin ready"
        );
        assert!(status.installed_at.is_none());
        assert!(!status.has_feature("request_logs"));
        assert!(
            status
                .installation_error
                .as_deref()
                .is_some_and(|message| message
                    .contains("plugin host refused ready: plugin refused the configuration")),
            "installation_error={:?}; host_log={}",
            status.installation_error,
            std::fs::read_to_string(&host_log).unwrap_or_default()
        );
        let persisted = database
            .local_setting_json("plugin_license:logs")
            .expect("read persisted state")
            .expect("persisted license state");
        assert!(persisted.contains("plugin refused the configuration"));
    }

    /// The node context (`host.configure`) reaches the host on startup, on
    /// restart and when it changes; and the UI proxy forwards through the real
    /// router to the server the plugin announces in `ui.upstream`.
    #[tokio::test]
    async fn host_context_and_ui_proxy_reach_a_real_plugin_host() {
        let _serial = REAL_HOST.lock().await;
        // "Plugin" web server: returns what it received.
        let upstream = axum::Router::new().fallback(|request: axum::extract::Request| async move {
            let token = request
                .headers()
                .get(crate::plugin_proxy::PROXY_TOKEN_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            axum::Json(serde_json::json!({
                "uri": request.uri().to_string(),
                "token": token,
                "cookie": request.headers().contains_key("cookie"),
            }))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, upstream).await });

        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_path = dir.path().join("logs-plugin.bin");
        std::fs::write(&plugin_path, b"fake-plugin").expect("write fake plugin");
        let host_log = dir.path().join("host.log");
        let token = "t".repeat(32);
        // SAFETY: serialized by `REAL_HOST`.
        std::env::set_var("POM_PLUGIN_HOST", fake_host_binary());
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);
        std::env::set_var(
            "POM_FAKE_HOST_UPSTREAM",
            serde_json::json!({"status": "ready", "port": upstream_port, "token": token})
                .to_string(),
        );

        let db = Arc::new(
            crate::db::PomDb::open(dir.path().join("pom.db")).expect("open the test database"),
        );
        let service = Arc::new(MockLicenseService::single(
            "logs",
            activation("logs", "ent-1"),
        ));
        let manager = PluginManager::test_manager_with_database(&["logs"], service.clone(), db);
        let context = |key: &str| {
            serde_json::json!({
                "gateway": {
                    "openai_base_url": "http://127.0.0.1:8080/v1",
                    "api_key": key
                },
                "workspace_root": "/data/workspace"
            })
        };
        manager.set_host_context(context("sk-first")).await;

        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;
        {
            let mut persisted = runtime.persisted.lock().await;
            persisted.as_mut().unwrap().plugin_path =
                Some(plugin_path.to_string_lossy().into_owned());
        }
        let persisted = runtime.persisted_state().await.expect("persisted");
        runtime.ensure_host_running(&persisted).await;
        let configures = |key: &str| {
            std::fs::read_to_string(&host_log)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.contains("\"host.configure\"") && line.contains(key))
                .count()
        };
        assert_eq!(
            configures("sk-first"),
            1,
            "a freshly started host receives the context"
        );
        assert!(std::fs::read_to_string(&host_log)
            .unwrap_or_default()
            .contains("/data/workspace"));

        manager.set_host_context(context("sk-first")).await;
        assert_eq!(configures("sk-first"), 1, "an equal context is not resent");
        manager.set_host_context(context("sk-second")).await;
        assert_eq!(
            configures("sk-second"),
            1,
            "a running host receives the new context"
        );

        let mut state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.plugins = manager.clone();
        let app = axum::Router::new()
            .merge(crate::webui::routes())
            .with_state(state);
        let request = |uri: &str, origin: Option<&str>| {
            let mut builder = axum::http::Request::get(uri)
                .header("host", "salem:8080")
                .header("cookie", "pom_admin_session=secret");
            if let Some(origin) = origin {
                builder = builder.header("origin", origin);
            }
            builder.body(axum::body::Body::empty()).unwrap()
        };
        use tower::ServiceExt;

        let response = app
            .clone()
            .oneshot(request("/api/ui/plugins/logs/proxy/hello?x=1", None))
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["uri"], "/hello?x=1");
        assert_eq!(body["token"], token.as_str());
        assert_eq!(
            body["cookie"], false,
            "the POM session does not reach the plugin"
        );

        let root = app
            .clone()
            .oneshot(request(
                "/api/ui/plugins/logs/proxy/?folder=/workspace",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(root.status(), axum::http::StatusCode::OK);
        let root_body = crate::gateway::testkit::body_of(root).await;
        assert_eq!(root_body["uri"], "/?folder=/workspace");

        let forged = app
            .clone()
            .oneshot(request(
                "/api/ui/plugins/logs/proxy/hello",
                Some("http://evil.example"),
            ))
            .await
            .unwrap();
        assert_eq!(forged.status(), axum::http::StatusCode::FORBIDDEN);

        let unknown = app
            .clone()
            .oneshot(request("/api/ui/plugins/nope/proxy/hello", None))
            .await
            .unwrap();
        assert_eq!(unknown.status(), axum::http::StatusCode::NOT_FOUND);

        // Plugin RPC: the body reaches `rpc.<op>` with the node profile.
        let rpc = |uri: &str, body: &str, origin: Option<&str>| {
            let mut builder = axum::http::Request::post(uri)
                .header("host", "salem:8080")
                .header("content-type", "application/json");
            if let Some(origin) = origin {
                builder = builder.header("origin", origin);
            }
            builder
                .body(axum::body::Body::from(body.to_owned()))
                .unwrap()
        };
        let reply = app
            .clone()
            .oneshot(rpc(
                "/api/ui/plugins/logs/rpc/directory.set_node",
                r#"{"node_id":"n1","person":"Ana"}"#,
                Some("http://salem:8080"),
            ))
            .await
            .unwrap();
        assert_eq!(reply.status(), axum::http::StatusCode::OK);
        let reply = crate::gateway::testkit::body_of(reply).await;
        assert_eq!(reply["operation"], "rpc.directory.set_node");
        assert_eq!(reply["params"]["person"], "Ana");
        assert!(reply["profile"].is_string());
        assert!(reply["node_id"].is_string());
        for (uri, body, origin, status) in [
            (
                "/api/ui/plugins/logs/rpc/insights",
                "{}",
                Some("http://evil.example"),
                axum::http::StatusCode::FORBIDDEN,
            ),
            (
                "/api/ui/plugins/logs/rpc/Bad.Op",
                "{}",
                None,
                axum::http::StatusCode::BAD_REQUEST,
            ),
            (
                "/api/ui/plugins/logs/rpc/insights",
                "not json",
                None,
                axum::http::StatusCode::BAD_REQUEST,
            ),
            (
                "/api/ui/plugins/nope/rpc/insights",
                "{}",
                None,
                axum::http::StatusCode::BAD_GATEWAY,
            ),
        ] {
            let response = app.clone().oneshot(rpc(uri, body, origin)).await.unwrap();
            assert_eq!(response.status(), status, "{uri} {body}");
        }

        runtime.stop_host_for_test().await;
        runtime.ensure_host_running(&persisted).await;
        assert_eq!(
            configures("sk-second"),
            2,
            "a restarted host receives the current context"
        );
        std::env::remove_var("POM_FAKE_HOST_UPSTREAM");
        runtime.stop_host_for_test().await;
    }

    #[test]
    fn host_context_payload_includes_the_shared_workspace_root() {
        let context = host_context_payload(8080, "sk-test", "/data/workspace", &[]);
        assert_eq!(
            context.get("workspace_root").and_then(Value::as_str),
            Some("/data/workspace")
        );
        assert_eq!(
            context.get("host_api_base_url").and_then(Value::as_str),
            Some("http://127.0.0.1:8080/api/plugin-host")
        );
    }

    #[test]
    fn host_context_carries_the_model_dirs() {
        let dirs = vec!["/models/a".to_owned(), "/models/b".to_owned()];
        let context = host_context_payload(8080, "sk-test", "/data/workspace", &dirs);
        assert_eq!(context["model_dirs"], serde_json::json!(["/models/a", "/models/b"]));
    }

    #[test]
    fn plugins_never_get_an_ollama_store_as_a_model_folder() {
        let root = tempfile::tempdir().unwrap();
        let ollama = root.path().join("ollama");
        std::fs::create_dir_all(ollama.join("manifests/registry")).unwrap();
        std::fs::create_dir_all(ollama.join("blobs")).unwrap();
        std::fs::write(ollama.join("blobs/sha256-0"), b"x").unwrap();
        let full = root.path().join("full");
        std::fs::create_dir_all(&full).unwrap();
        assert_eq!(
            plugin_model_dirs(&[ollama, full.clone()]),
            [full.to_string_lossy().into_owned()]
        );
    }

    #[tokio::test]
    async fn heartbeat_success_restarts_the_stopped_plugin_host() {
        let _serial = REAL_HOST.lock().await;
        // `CARGO_BIN_EXE_*` is only injected in integration tests
        // (`tests/*.rs`); this is a unit test inside the `pom` binary, so we
        // locate the helper binary the same way `plugin_host::host_executable`
        // locates the real `pom-plugin-host` in production: beside cargo's output
        // directory (`target/<profile>/deps/pom-<hash>` -> `target/<profile>/`).
        let fake_host = std::env::current_exe()
            .expect("locate the test binary")
            .parent()
            .and_then(|deps| deps.parent())
            .expect("locate the target/<profile> directory")
            .join(if cfg!(windows) {
                "pom-fake-enterprise-plugin-host.exe"
            } else {
                "pom-fake-enterprise-plugin-host"
            });
        assert!(
            fake_host.is_file(),
            "helper binary {} was not built; run `cargo test -p node` (not `--lib`) to produce all the [[bin]] targets",
            fake_host.display()
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_path = dir.path().join("logs-plugin.bin");
        std::fs::write(&plugin_path, b"fake-plugin").expect("write fake plugin");
        let host_log = dir.path().join("ingest.log");

        // SAFETY: no other test in this suite reads or writes
        // POM_PLUGIN_HOST/POM_FAKE_HOST_LOG; only this one exercises a real
        // PluginHost::start().
        std::env::set_var("POM_PLUGIN_HOST", fake_host);
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);

        let db = Arc::new(
            crate::db::PomDb::open(dir.path().join("pom.db")).expect("open the test database"),
        );
        let service = Arc::new(MockLicenseService::single(
            "logs",
            activation("logs", "ent-1"),
        ));
        // `test_manager_with_database` does not trigger the boot `restore()` (see
        // `from_codes`): here the state is built by hand just below, and a
        // concurrent restore would overwrite it from the database at an instant
        // outside the test's control.
        let manager = PluginManager::test_manager_with_database(&["logs"], service.clone(), db);
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;
        {
            let mut persisted = runtime.persisted.lock().await;
            persisted.as_mut().unwrap().plugin_path =
                Some(plugin_path.to_string_lossy().into_owned());
        }

        let persisted = runtime.persisted_state().await.expect("persisted");
        runtime.ensure_host_running(&persisted).await;
        assert!(
            runtime.host.lock().await.is_some(),
            "the host should start on the first activation"
        );

        service.set_heartbeat_error(LicenseErrorCode::Revoked).await;
        runtime
            .heartbeat("nonce-revoke")
            .await
            .expect_err("the heartbeat must reflect the revocation");
        assert!(
            runtime.host.lock().await.is_none(),
            "the host should stop when the license is revoked"
        );
        assert!(!runtime.status().await.active);

        service.clear_heartbeat_error().await;
        runtime
            .heartbeat("nonce-self-heal")
            .await
            .expect("the self-heal heartbeat must succeed");

        assert!(
            runtime.host.lock().await.is_some(),
            "a successful heartbeat should restart the host without restarting the pom process"
        );
        assert!(runtime.status().await.active);

        assert!(
            runtime
                .ingest_audit_event(crate::reqlog::AuditEvent::Begin(
                    crate::reqlog::AuditRequest {
                        id: "req-1".into(),
                        received_at: "2099-01-01T00:00:00Z".into(),
                        route: "/api/generate".into(),
                        model: "test-model".into(),
                        caller: crate::reqlog::Caller::anonymous(),
                        entry_node_id: "node-a".into(),
                        prompt: "olá".into(),
                        request_details: serde_json::json!({}),
                    }
                ))
                .await,
            "ingest should work with the restored host"
        );
        let events = std::fs::read_to_string(&host_log).expect("ler log do fake host");
        assert!(
            events.contains("req-1"),
            "audit event should have reached the restarted host"
        );
    }

    /// An on-demand plugin (manifest with `lifecycle: on_demand`) does not have its
    /// process restarted by boot or heartbeat: only `open_on_demand` starts the
    /// host, `close_on_demand` stops it, and the installation and license stay.
    #[tokio::test]
    async fn an_on_demand_plugin_runs_only_while_opened() {
        let _serial = REAL_HOST.lock().await;
        let fake_host = std::env::current_exe()
            .expect("locate the test binary")
            .parent()
            .and_then(|deps| deps.parent())
            .expect("locate the target/<profile> directory")
            .join(if cfg!(windows) {
                "pom-fake-enterprise-plugin-host.exe"
            } else {
                "pom-fake-enterprise-plugin-host"
            });
        assert!(
            fake_host.is_file(),
            "helper binary {} was not built; run `cargo test -p node` (not `--lib`)",
            fake_host.display()
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_path = dir.path().join("logs-plugin.bin");
        std::fs::write(&plugin_path, b"fake-plugin").expect("escrever plugin fake");
        let host_log = dir.path().join("ingest.log");

        // SAFETY: same criterion as the restart test above; `REAL_HOST` serializes.
        std::env::set_var("POM_PLUGIN_HOST", fake_host);
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);

        let db = Arc::new(
            crate::db::PomDb::open(dir.path().join("pom.db")).expect("abrir banco de teste"),
        );
        let service = Arc::new(MockLicenseService::single(
            "logs",
            activation("logs", "ent-1"),
        ));
        let manager = PluginManager::test_manager_with_database(&["logs"], service.clone(), db);
        let runtime = manager.runtime_for("logs").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;
        {
            let mut persisted = runtime.persisted.lock().await;
            persisted.as_mut().unwrap().plugin_path =
                Some(plugin_path.to_string_lossy().into_owned());
        }
        // The manifest declares the lifecycle, and the process has not started yet.
        assert!(runtime.host.lock().await.is_none());
        runtime
            .remember_ui_manifest(
                serde_json::json!({
                    "schema": "pom-plugin-ui/v1",
                    "plugin_code": "logs",
                    "lifecycle": "on_demand",
                    "screens": {},
                    "assets": []
                }),
                true,
            )
            .await;
        // Boot and heartbeat do not start an on-demand process.
        let persisted = runtime.persisted_state().await.expect("persisted");
        runtime.ensure_host_running(&persisted).await;
        assert!(
            runtime.host.lock().await.is_none(),
            "boot does not start the process"
        );
        runtime
            .heartbeat("nonce-on-demand")
            .await
            .expect("heartbeat de sucesso");
        assert!(
            runtime.host.lock().await.is_none(),
            "heartbeat does not start the process"
        );

        let status = runtime.status().await;
        assert!(status.active, "active: {status:?}");
        assert!(status.on_demand, "on_demand: {status:?}");
        assert!(!status.running);
        // The menu keeps listing the plugin, through the saved manifest.
        assert!(runtime.ui_manifest().await.is_some());

        runtime
            .open_on_demand()
            .await
            .expect("opening starts the process");
        assert!(runtime.status().await.running);
        runtime
            .close_on_demand()
            .await
            .expect("fechar encerra o processo");
        let status = runtime.status().await;
        assert!(
            !status.running && status.active,
            "closing does not remove the license or the installation"
        );
    }

    #[test]
    fn the_saved_manifest_decides_on_demand_before_any_process_starts() {
        let on_demand = serde_json::json!({ "lifecycle": "on_demand" });
        let resident = serde_json::json!({ "lifecycle": "resident" });
        assert!(manifest_is_on_demand(Some(&on_demand)));
        assert!(!manifest_is_on_demand(Some(&resident)));
        assert!(!manifest_is_on_demand(Some(&serde_json::json!({}))));
        assert!(!manifest_is_on_demand(None));
    }
}

#[cfg(test)]
mod plugin_event_tests {
    use super::*;

    #[test]
    fn the_hub_keeps_a_bounded_sequence_and_never_replays_the_past_to_a_new_reader() {
        let hub = PluginEventHub::default();
        assert_eq!(hub.after(None), (0, Vec::new()));
        for index in 0..(PLUGIN_EVENT_BACKLOG + 5) {
            hub.push(serde_json::json!({ "index": index }));
        }
        let (latest, fresh) = hub.after(None);
        assert_eq!(latest, (PLUGIN_EVENT_BACKLOG + 5) as u64);
        assert!(fresh.is_empty());
        let (_, events) = hub.after(Some(latest - 2));
        assert_eq!(
            events.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            [latest - 1, latest]
        );
        let (_, all) = hub.after(Some(0));
        assert_eq!(all.len(), PLUGIN_EVENT_BACKLOG);
        assert_eq!(all[0].0, 6);
    }

    #[test]
    fn serving_transitions_report_new_and_gone_models() {
        let snapshot = |models: &[&str]| {
            models
                .iter()
                .map(|model| (model.to_string(), serde_json::json!({ "model": model })))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let events = serving_transitions(&snapshot(&["a", "b"]), &snapshot(&["b", "c"]));
        assert_eq!(
            events,
            vec![
                ("model.serving", serde_json::json!({ "model": "c" })),
                ("model.stopped", serde_json::json!({ "model": "a" })),
            ]
        );
        assert!(serving_transitions(&snapshot(&["b"]), &snapshot(&["b"])).is_empty());
    }

    #[test]
    fn envelopes_follow_the_plugin_events_protocol() {
        let envelope = plugin_event_envelope(
            "deployment.started",
            "*",
            serde_json::json!({ "deploy_id": "d1" }),
        );
        assert_eq!(envelope["protocol"], PLUGIN_EVENTS_PROTOCOL);
        assert_eq!(envelope["type"], "deployment.started");
        assert_eq!(envelope["target"], "*");
        assert_eq!(envelope["source"], "pom");
        assert_eq!(envelope["payload"]["deploy_id"], "d1");
    }
}

#[cfg(test)]
mod plugin_checkout_tests {
    use super::*;
    use axum::routing::{get, post};

    #[test]
    fn only_stripe_or_the_license_server_can_receive_the_browser() {
        let base = "https://license.pieceofmind.cloud";
        assert!(checkout_url_allowed(
            "https://checkout.stripe.com/c/pay/cs_test_1",
            base
        ));
        assert!(checkout_url_allowed(
            "https://license.pieceofmind.cloud/mock/cs_1",
            base
        ));
        assert!(!checkout_url_allowed(
            "http://checkout.stripe.com/c/pay/cs_1",
            base
        ));
        assert!(!checkout_url_allowed(
            "https://evil.example/stripe.com",
            base
        ));
        assert!(!checkout_url_allowed(
            "https://stripe.com.evil.example/",
            base
        ));
        assert!(!checkout_url_allowed("javascript:alert(1)", base));
    }

    async fn stub(
        session_url: &'static str,
    ) -> (
        String,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let recorder = seen.clone();
        let app = axum::Router::new()
            .route(
                "/v1/checkout/plans",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "plans": [{"code": "logs", "plugin_codes": ["logs"], "checkout_available": true}]
                    }))
                }),
            )
            .route(
                "/v1/checkout/sessions",
                post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let recorder = recorder.clone();
                    async move {
                        *recorder.lock().unwrap() = Some(body);
                        axum::Json(serde_json::json!({ "id": "cs_test_1", "url": session_url }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, server, seen)
    }

    #[tokio::test]
    async fn checkout_uses_the_plan_that_sells_the_plugin_and_returns_the_stripe_page() {
        let (base, server, seen) = stub("https://checkout.stripe.com/c/pay/cs_test_1").await;
        let request = PluginCheckoutRequest {
            plugin_code: "logs".into(),
            interval: Some("month".into()),
            locale: Some("en".into()),
        };
        let url = create_plugin_checkout(&reqwest::Client::new(), &base, &request)
            .await
            .unwrap();
        assert_eq!(url, "https://checkout.stripe.com/c/pay/cs_test_1");
        assert_eq!(
            seen.lock().unwrap().clone().unwrap(),
            serde_json::json!({"plan_code": "logs", "plugin_code": "logs", "interval": "month", "locale": "en"})
        );

        let unsold = PluginCheckoutRequest {
            plugin_code: "harness".into(),
            interval: None,
            locale: None,
        };
        assert_eq!(
            create_plugin_checkout(&reqwest::Client::new(), &base, &unsold)
                .await
                .unwrap_err()
                .1,
            "checkout_not_offered"
        );
        let invalid = PluginCheckoutRequest {
            plugin_code: "logs".into(),
            interval: Some("week".into()),
            locale: None,
        };
        assert_eq!(
            create_plugin_checkout(&reqwest::Client::new(), &base, &invalid)
                .await
                .unwrap_err()
                .1,
            "checkout_request_invalid"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_session_pointing_elsewhere_is_refused() {
        let (base, server, _) = stub("https://evil.example/pay").await;
        let request = PluginCheckoutRequest {
            plugin_code: "logs".into(),
            interval: None,
            locale: None,
        };
        assert_eq!(
            create_plugin_checkout(&reqwest::Client::new(), &base, &request)
                .await
                .unwrap_err()
                .1,
            "checkout_url_rejected"
        );
        server.abort();
    }
}
