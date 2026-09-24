//! Gerenciador genérico de plugins independentes.
//!
//! Até a decisão do captain (2026-09-18), o POM tinha uma única "licença
//! Enterprise" para um único produto opcional. Esse modelo virou N plugins
//! independentes, cada um com seu próprio `plugin_code`, ciclo de ativação,
//! heartbeat e host de processo. Não há catálogo local de plugins: o servidor
//! de licenças fornece os `plugin_code` concedidos, e [`PluginManager`] cria
//! um [`PluginRuntime`] por código quando necessário. Cada runtime tem seu próprio
//! estado de ativação, lease, host e checagem de atualização de versão,
//! independentes das demais.
//!
//! A atualização do binário `pom` em si (`crate::pom_core_update`) NÃO faz
//! parte deste gerenciador de propósito: ela é outra fronteira de produto
//! (vai virar GitHub Releases público, sem licença nenhuma) e só empresta,
//! por baixo dos panos, o lease do plugin `"enterprise"` enquanto esse
//! mecanismo de transição não muda. Ver `crate::pom_core_update`.
//!
//! **Contrato do servidor de licenças** (confirmado contra o servidor real, e
//! fixado por `enterprise-protocol/tests/wire_contract.rs` com corpos de
//! resposta capturados dele): uma chave concede vários `plugin_code` de uma
//! vez, e todas as respostas listam esses códigos em `licensed_plugins` -
//! códigos puros, sem features. **As features de um plugin vêm do manifesto
//! do release instalado** (`PluginManifest::feature_set`, no formato
//! `"<plugin_code>.<feature>"`), porque quem define o que uma versão entrega
//! é aquela versão, não a licença. A ativação (`POST
//! /v1/licenses/activations`) é uma chamada POR `plugin_code`, e os endpoints
//! de manifest/download já eram genéricos por `plugin_code`
//! (`/v1/plugins/:plugin_code/manifest`).
//!
//! Este cliente já assumiu o contrário - que a licença listava features em
//! `entitlements` - e a ativação de plugin quebrou em produção com
//! `missing field \`entitlements\`` quando o servidor virou catálogo de
//! plugins. Não volte a derivar features da licença.

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

/// O plugin de Estatísticas/Logs de Requisições. Fica nomeado porque
/// `crate::reqlog`, `crate::backend::gateway::generation`
/// e `crate::pom_core_update` precisam de um `plugin_code` concreto, não do
/// código dinâmico de outra instância.
pub(crate) const ENTERPRISE_PLUGIN_CODE: &str = "enterprise";

const PERSISTED_SETTING_PREFIX: &str = "plugin_license:";
const GITHUB_SETTING_PREFIX: &str = "plugin_github:";
const PREFERENCES_SETTING_PREFIX: &str = "plugin_preferences:";

/// Plugin codes vêm do serviço de licenças e também compõem segmentos de URL
/// e diretórios locais. Aceitamos slugs genéricos, mas nunca separadores ou
/// segmentos de caminho.
fn is_valid_plugin_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
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
        _persisted: &PersistedPluginState,
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
        persisted: &PersistedPluginState,
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
    /// Momento em que o host da release instalada ficou pronto. Heartbeats e
    /// consultas de atualização não alteram este campo.
    #[serde(default)]
    pub installed_at: Option<String>,
    /// Última consulta de manifest concluída com sucesso, independente de
    /// haver uma release nova. Uma consulta que falha preserva a última boa.
    #[serde(default)]
    pub update_checked_at: Option<String>,
    /// A release efetivamente baixada, verificada e instalada nesta máquina.
    /// Só a instalação de `activate_and_install`/o passo final de
    /// `activate` (sem download real, usado só nos testes deste módulo) e
    /// `apply_update` concluídos podem avançar este campo - um heartbeat
    /// sozinho nunca confirma um download. `#[serde(rename)]` preserva a
    /// chave `plugin_release_id` já persistida em instalações existentes.
    ///
    /// Defeito corrigido (2026-09-21): o heartbeat escrevia aqui o
    /// `plugin_release_id` que ele mesmo recebe do servidor, mesmo sem
    /// nenhum download ter ocorrido. Como `detect_update` compara o
    /// `release_id` do manifest contra exatamente este campo, o clique em
    /// "Atualizar" (que chama heartbeat e depois `detect_update` na mesma
    /// ação) corrompia sua própria base de comparação e sempre concluía "sem
    /// atualização" - sem relação com o formato semver do release (`+build.N`
    /// funciona normalmente na comparação por string; o problema era a ordem
    /// de escrita, não o formato). Ver `docs/subsystems/licenciamento-enterprise.md`.
    #[serde(rename = "plugin_release_id")]
    pub installed_release_id: String,
    pub plugin_version: String,
    pub features: Vec<String>,
    pub plugin_sha256: String,
    pub plugin_path: Option<String>,
    #[serde(default)]
    pub preferences_schema: Option<enterprise_protocol::PluginPreferencesSchema>,
}

/// Uma atualização de versão disponível para instalação - do próprio plugin,
/// nunca do `pom_core` (ver `crate::pom_core_update::CoreUpdate`, o tipo
/// irmão deliberadamente separado).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PluginUpdateInfo {
    pub release_id: String,
    pub version: String,
    pub changelog: Option<String>,
}

/// O contrato estável que a tela de Plugins lê. Uma entrada por `plugin_code`
/// do catálogo, mesmo quando o operador nunca ativou aquele plugin - é assim
/// que a tela mostra "disponível para compra" ao lado dos já ativos.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PluginStatus {
    pub code: String,
    pub active: bool,
    pub state: LicenseState,
    pub features: Vec<String>,
    pub manage_url: String,
    pub plugin_version: Option<String>,
    pub lease_expires_at: Option<String>,
    /// Validade da LICENÇA, distinta do `lease_expires_at` técnico acima (o
    /// lease renova a cada heartbeat e costuma vencer em dias; a licença
    /// comprada costuma vencer em meses ou anos). Lida de
    /// `LicenseStatusResponse::license_expires_at`
    /// (`GET /v1/licenses/status`), nunca do lease - mostrar o lease como se
    /// fosse a validade da licença é a confusão que este campo existe para
    /// evitar.
    pub license_expires_at: Option<String>,
    pub activation_count: Option<u32>,
    pub max_activations: Option<u32>,
    pub licensed_plugins: Vec<String>,
    pub plugin_usage: Vec<enterprise_protocol::PluginUsage>,
    pub activation_candidates: Vec<enterprise_protocol::ActivationCandidate>,
    pub installed_at: Option<String>,
    pub update_checked_at: Option<String>,
    pub available_update: Option<PluginUpdateInfo>,
    /// Public repository URL configured locally, if any.
    #[serde(default)]
    pub github_repository: Option<String>,
    /// Verified GitHub release downloaded locally, waiting for explicit install.
    #[serde(default)]
    pub github_update: Option<crate::plugin_github::GithubUpdateInfo>,
    #[serde(default)]
    pub github_error: Option<String>,
    #[serde(default)]
    pub preferences_schema: Option<enterprise_protocol::PluginPreferencesSchema>,
    #[serde(default)]
    pub preferences: Option<enterprise_protocol::PluginPreferences>,
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
            github_repository: None,
            github_update: None,
            github_error: None,
            preferences_schema: None,
            preferences: None,
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

/// Estado e ciclo de vida de UM plugin (`plugin_code`). Cada instância é
/// autocontida: sua própria ativação, lease, heartbeat, host de processo e
/// checagem de atualização de versão, independentes de qualquer outro
/// plugin. `PluginManager` mantém uma por entrada do catálogo.
pub(crate) struct PluginRuntime {
    code: String,
    service: Arc<dyn LicenseService>,
    fingerprint: Fingerprint,
    status: Arc<RwLock<PluginStatus>>,
    pending: Arc<Mutex<Option<PendingActivation>>>,
    persisted: Arc<Mutex<Option<PersistedPluginState>>>,
    database: Option<Arc<crate::db::PomDb>>,
    host: Arc<Mutex<Option<crate::plugin_host::PluginHost>>>,
    update_lock: Arc<Mutex<()>>,
    github: Arc<RwLock<crate::plugin_github::GithubPluginState>>,
    /// Contexto que o nó entrega a todo host de plugin (`host.configure`),
    /// compartilhado com o [`PluginManager`]. `None` até o gateway ficar pronto.
    host_context: HostContext,
}

/// O que o nó entrega a cada host de plugin pela operação `host.configure`.
type HostContext = Arc<StdRwLock<Option<serde_json::Value>>>;

impl PluginRuntime {
    fn new(
        code: &str,
        service: Arc<dyn LicenseService>,
        fingerprint: Fingerprint,
        database: Option<Arc<crate::db::PomDb>>,
        host_context: HostContext,
    ) -> Self {
        let github = database
            .as_ref()
            .and_then(|database| {
                database
                    .local_setting_json(&format!("{GITHUB_SETTING_PREFIX}{code}"))
                    .ok()
                    .flatten()
            })
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        Self {
            code: code.to_owned(),
            service,
            fingerprint,
            status: Arc::new(RwLock::new(PluginStatus::inactive(code))),
            pending: Arc::new(Mutex::new(None)),
            persisted: Arc::new(Mutex::new(None)),
            database,
            host: Arc::new(Mutex::new(None)),
            update_lock: Arc::new(Mutex::new(())),
            github: Arc::new(RwLock::new(github)),
            host_context,
        }
    }

    /// Sobe o host do plugin e já lhe entrega o contexto do nó, se houver.
    async fn start_host(
        &self,
        path: &std::path::Path,
        database_path: &std::path::Path,
    ) -> anyhow::Result<crate::plugin_host::PluginHost> {
        let host = crate::plugin_host::PluginHost::start(&self.code, path, database_path).await?;
        self.configure_host(&host).await;
        Ok(host)
    }

    /// `host.configure`: o endpoint OpenAI local e a API key que o plugin
    /// usa para chamar os modelos do nó. Um plugin que não conhece a operação
    /// responde erro, e isso não é falha - ele simplesmente não precisa dela.
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
            if let Some(schema) = self
                .persisted
                .lock()
                .await
                .as_ref()
                .and_then(|state| state.preferences_schema.clone())
            {
                object.insert(
                    "preferences".into(),
                    serde_json::to_value(self.preferences_for(&schema)).unwrap_or_default(),
                );
            }
        }
        if let Err(error) = host.query(payload).await {
            tracing::debug!(plugin_code = %self.code, %error, "plugin não aceitou host.configure");
        }
    }

    #[cfg(test)]
    async fn stop_host_for_test(&self) {
        if let Some(host) = self.host.lock().await.take() {
            let _ = host.stop().await;
        }
    }

    /// Reentrega o contexto ao host em execução, depois de o nó mudá-lo.
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

    fn github_key(&self) -> String {
        format!("{GITHUB_SETTING_PREFIX}{}", self.code)
    }

    fn github_download_path(
        &self,
        release_id: &str,
        sha256: &str,
    ) -> Result<std::path::PathBuf, String> {
        let root = self
            .database
            .as_ref()
            .and_then(|database| database.path().parent())
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(crate::config::project_root);
        crate::plugin_github::downloaded_asset_path(
            &root,
            &self.code,
            release_id,
            sha256,
            &self.fingerprint.platform,
            &self.fingerprint.arch,
        )
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
        let github = self.github.read().await.clone();
        status.github_repository = github.repository_url;
        status.github_update = github.update;
        status.github_error = github.check_error;
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
            if status
                .github_update
                .as_ref()
                .and_then(|update| semver::Version::parse(&update.version).ok())
                .is_some_and(|version| version <= installed_version)
            {
                status.github_update = None;
            }
        }
        if let Some(persisted) = self.persisted.lock().await.as_ref() {
            if let Some(schema) = &persisted.preferences_schema {
                status.preferences_schema = Some(schema.clone());
                status.preferences = Some(self.preferences_for(schema));
            }
        }
        status
    }

    pub(crate) async fn persisted_state(&self) -> Option<PersistedPluginState> {
        self.persisted.lock().await.clone()
    }

    async fn save_preferences(
        &self,
        values: enterprise_protocol::PluginPreferences,
    ) -> Result<(), LicenseClientError> {
        if !self.status.read().await.active {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "Ative o plugin antes de editar suas preferências.",
            ));
        }
        let schema = self
            .persisted
            .lock()
            .await
            .as_ref()
            .and_then(|persisted| persisted.preferences_schema.clone())
            .ok_or_else(|| update_unavailable("este release não declara preferências"))?;
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
        Ok(())
    }

    /// Restaura o estado persistido (se houver) e, se o plugin já tinha um
    /// binário instalado e a licença continua válida (online ou dentro da
    /// graça offline), religa o host de processo. Roda uma vez na construção
    /// do runtime.
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
            return;
        };
        *self.persisted.lock().await = Some(persisted.clone());
        let Some(path) = persisted.plugin_path.clone().map(std::path::PathBuf::from) else {
            return;
        };
        if !path.is_file() {
            return;
        }
        if self.database.is_none() {
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
                let mut status = this.status.write().await;
                status.active = false;
                status.state = LicenseState::Expired;
                return;
            }
            *this.persisted.lock().await = Some(restored.clone());
            this.ensure_host_running(&restored).await;
            this.restore_ready(&restored).await;
        });
    }

    async fn restore_ready(&self, persisted: &PersistedPluginState) {
        let mut status = self.status.write().await;
        status.active = true;
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
    }

    /// Fluxo completo: valida a chave para este plugin_code, instala o
    /// binário e sobe o host. Ponto de entrada único usado tanto em produção
    /// quanto nos testes deste módulo.
    async fn activate_and_install(&self, license_key: &str) -> Result<(), LicenseClientError> {
        self.activate(license_key).await?;
        let pending = self.pending.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "Nenhuma ativação aguardando o download.",
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
        let database_path = self
            .database
            .as_ref()
            .map(|database| database.path().with_file_name(format!("{}.db", self.code)))
            .unwrap_or_else(|| crate::config::project_root().join(format!("{}.db", self.code)));
        let host = match self.start_host(&path, &database_path).await {
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
        persisted.installed_release_id = manifest.manifest.release_id;
        persisted.plugin_version = manifest.manifest.version.clone();
        persisted.plugin_sha256 = manifest.manifest.sha256.clone();
        persisted.plugin_path = Some(path.to_string_lossy().into_owned());
        persisted.preferences_schema = manifest.manifest.preferences.clone();
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
        }
        *self.persisted.lock().await = Some(persisted);
        self.mark_plugin_ready(&manifest.manifest.version).await
    }

    /// Só a etapa de licença (validate + activate + persistência do lease),
    /// sem manifest, download nem host - deliberadamente `pub(crate)` para
    /// que os testes deste módulo montem um plugin "pronto" com
    /// `mark_plugin_ready` sem depender do binário real `pom-plugin-host`,
    /// exatamente como a integração com esse processo é testada à parte
    /// (`cargo test -p pom-plugin-host`, `-p enterprise-plugin-fixture`).
    pub(crate) async fn activate(&self, license_key: &str) -> Result<(), LicenseClientError> {
        if license_key.trim().is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::InvalidKey,
                "Informe uma chave de licença.",
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
                "A resposta de ativação não contém um grant ou ABI válido.",
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
                "O release instalado não declara nenhuma feature para este plugin.",
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
        status.state = LicenseState::Unavailable;
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
        Ok(())
    }

    async fn deactivate(&self) -> Result<(), LicenseClientError> {
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
                .delete_local_setting(&self.setting_key())
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self.pending.lock().await = None;
        *self.persisted.lock().await = None;
        *self.status.write().await = PluginStatus::inactive(&self.code);
        Ok(())
    }

    /// Reinicia o host do plugin quando a licença está ativa mas o host não
    /// está rodando (por exemplo, depois que uma revogação temporária o
    /// parou e um heartbeat subsequente restaurou a licença). Não faz nada
    /// se o host já estiver de pé ou se faltar o caminho do plugin instalado.
    /// Usada tanto na restauração de boot (`restore`) quanto no self-heal do
    /// heartbeat (`heartbeat`) - mesma lógica, um só lugar.
    async fn ensure_host_running(&self, persisted: &PersistedPluginState) {
        if self.host.lock().await.is_some() {
            return;
        }
        let Some(path) = persisted.plugin_path.clone().map(std::path::PathBuf::from) else {
            return;
        };
        if !path.is_file() {
            return;
        }
        let Some(database) = &self.database else {
            return;
        };
        let database_path = database.path().with_file_name(format!("{}.db", self.code));
        match self.start_host(&path, &database_path).await {
            Ok(host) => {
                *self.host.lock().await = Some(host);
                tracing::info!(plugin_code = %self.code, "host do plugin reiniciado após licença voltar a ficar ativa");
            }
            Err(error) => {
                tracing::warn!(plugin_code = %self.code, error = %error, "host do plugin não pôde ser reiniciado após licença voltar a ficar ativa");
            }
        }
    }

    pub(crate) async fn ingest_audit_event(&self, event: crate::reqlog::AuditEvent) -> bool {
        let Some(host) = self.host.lock().await.clone() else {
            tracing::warn!(
                plugin_code = %self.code,
                "evento de auditoria descartado: host do plugin não está rodando (licença inativa ou aguardando restauração)"
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
        if let Err(error) = host.ingest(event).await {
            tracing::warn!(plugin_code = %self.code, error = %error, "evento de auditoria não entrou no plugin");
            return false;
        }
        true
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
                tracing::warn!(plugin_code = %self.code, error = %error, "consulta do plugin falhou");
                None
            }
        }
    }

    /// Manifesto de UI do plugin, pedido ao host (`ui.manifest`) e validado.
    /// `None` quando inativo, sem host, sem UI ou com manifesto inválido.
    pub(crate) async fn ui_manifest(&self) -> Option<crate::plugin_ui::PluginUiManifest> {
        let value = self
            .query_audit("ui.manifest", serde_json::json!({}))
            .await?;
        match crate::plugin_ui::PluginUiManifest::parse(&self.code, value) {
            Ok(manifest) => Some(manifest),
            Err(error) => {
                tracing::warn!(plugin_code = %self.code, %error, "manifesto de UI do plugin recusado");
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
                "Nenhuma ativação aguardando o host.",
            )
        })?;
        if pending.fingerprint != self.fingerprint {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FingerprintMismatch,
                "A ativação não pertence a esta instalação.",
            ));
        }
        let features = features_for_code(&self.code, &pending.response.plugin_manifest.feature_set);
        if features.is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "O release instalado não declara nenhuma feature para este plugin.",
            ));
        }
        let installed_at = chrono::Utc::now().to_rfc3339();
        let mut persisted_guard = self.persisted.lock().await;
        let mut persisted = persisted_guard.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Ativação local ausente.")
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
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Nenhum lease local.")
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
                // O heartbeat não traz manifesto, então as features continuam
                // sendo as do release instalado. O que ele traz é a lista de
                // plugins que a licença ainda concede: se este saiu da lista,
                // o plugin perde as features na hora.
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
                    // `installed_release_id` NÃO é escrito aqui: o heartbeat não baixa
                    // nem verifica nada, e `detect_update` compara o manifest contra
                    // exatamente este campo - escrevê-lo aqui corrompia essa própria
                    // comparação (ver o comentário no campo em `PersistedPluginState`).
                    // `plugin_release_id`/`release_changed`/`available_release_id` do
                    // heartbeat continuam consumidos, só que para alimentar
                    // `status.available_update` (abaixo), nunca esta linha de base.
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
                        "heartbeat indicou alteração de release do plugin"
                    );
                }
                let mut status = self.status.write().await;
                let was_active = status.active;
                status.active = true;
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
                if !was_active {
                    tracing::info!(
                        plugin_code = %self.code,
                        "licença do plugin voltou a ficar ativa após heartbeat bem-sucedido"
                    );
                }
                if let Some(persisted) = updated_persisted {
                    self.ensure_host_running(&persisted).await;
                }
                // O heartbeat sinalizou uma release nova; reconsulta o manifest
                // (via `detect_update`, agora com a base de comparação intacta)
                // para publicar `status.available_update` com versão e changelog
                // reais, sem esperar o próximo clique manual em "Atualizar". Isso
                // também cobre o supervisor de heartbeat periódico
                // (`spawn_heartbeat_supervisor`), que nunca chamava `detect_update`
                // por conta própria.
                if release_changed {
                    if let Err(error) = self.detect_update().await {
                        tracing::warn!(
                            plugin_code = %self.code,
                            error = %error,
                            "detecção de atualização após heartbeat falhou"
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
                    tracing::warn!(plugin_code = %runtime.code, error = %error, "heartbeat do plugin falhou");
                } else if let Err(error) = runtime.detect_update().await {
                    tracing::warn!(plugin_code = %runtime.code, error = %error, "detecção de atualização do plugin falhou");
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

    /// Reconsulta `/v1/licenses/status` para publicar `license_expires_at`
    /// atualizado - a validade da LICENÇA, distinta do `lease_expires_at`
    /// técnico que o heartbeat já renova. Chamado só pelo clique manual em
    /// "Atualizar" (`PluginManager::refresh`), não pelo supervisor periódico:
    /// o heartbeat já cobre a checagem de saúde recorrente, e esta é uma
    /// chamada extra ao mesmo endpoint que `restore()` usa na inicialização.
    /// Falha aqui nunca derruba o `refresh` chamador - é enriquecimento, não
    /// prova de validade.
    async fn refresh_license_status(&self) -> Result<(), LicenseClientError> {
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Nenhum lease local.")
        })?;
        let response = self.service.status(&persisted, &self.fingerprint).await?;
        let mut status = self.status.write().await;
        status.license_expires_at = response.payload.license_expires_at;
        Ok(())
    }

    /// Checagem de atualização de versão PRÓPRIA deste plugin - não
    /// compartilhada com outros plugins nem com o `pom_core`. Só publica o
    /// aviso no status; download, troca e reinício do host ficam reservados
    /// à confirmação explícita do operador em `apply_update`.
    pub(crate) async fn detect_update(&self) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        if !self.status().await.active {
            return Ok(());
        }
        let persisted = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(
                LicenseErrorCode::LeaseUnknown,
                "lease local ausente para checar atualização do plugin",
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
                "lease local ausente ao salvar a verificação de atualização",
            ));
        }
        let mut status = self.status.write().await;
        if status.active {
            if let Some(update) = &available {
                tracing::info!(
                    plugin_code = %self.code,
                    release_id = %update.release_id,
                    version = %update.version,
                    "release nova do plugin encontrada"
                );
            }
            status.update_checked_at = Some(update_checked_at);
            status.available_update = available;
        }
        Ok(())
    }

    /// Baixa, instala e reinicia o host somente com a release que o operador
    /// confirmou na tela de Plugins. Reconsulta o manifest para não reusar
    /// metadados de uma detecção antiga.
    pub(crate) async fn apply_update(
        &self,
        requested_release_id: &str,
    ) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        let status = self.status().await;
        if !status.active {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "este plugin precisa estar ativo para ser atualizado",
            ));
        }
        let requested_release_id = requested_release_id.trim();
        let Some(available) = status.available_update.as_ref() else {
            return Err(update_unavailable(
                "nenhuma atualização deste plugin está disponível",
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
                "lease local ausente para atualizar o plugin",
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
                "a atualização confirmada já não é uma release nova disponível",
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
        let database_path = self
            .database
            .as_ref()
            .map(|database| database.path().with_file_name(format!("{}.db", self.code)))
            .unwrap_or_else(|| crate::config::project_root().join(format!("{}.db", self.code)));
        let host = self
            .start_host(&path, &database_path)
            .await
            .map_err(|error| {
                LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
            })?;
        *self.host.lock().await = Some(host);
        let mut persisted = persisted;
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
        drop(status);
        self.reconfigure_running_host().await;
        Ok(())
    }

    async fn persist_github_state(
        &self,
        github: &crate::plugin_github::GithubPluginState,
    ) -> Result<(), String> {
        let previous = self.github.read().await.clone();
        if let Some(database) = &self.database {
            database
                .save_local_setting(&self.github_key(), github)
                .map_err(|error| error.to_string())?;
        }
        *self.github.write().await = github.clone();
        if let Some(previous_manifest) = previous.manifest {
            let retained = github.manifest.as_ref().is_some_and(|current| {
                current.release_id == previous_manifest.release_id
                    && current.sha256 == previous_manifest.sha256
            });
            if !retained {
                if let Ok(path) = self
                    .github_download_path(&previous_manifest.release_id, &previous_manifest.sha256)
                {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        Ok(())
    }

    async fn fetch_github_update(
        &self,
        repository_url: &str,
        client: &crate::plugin_github::GithubReleaseClient,
    ) -> Result<crate::plugin_github::GithubPluginState, String> {
        let current_version = self
            .persisted
            .lock()
            .await
            .as_ref()
            .map(|state| state.plugin_version.clone());
        let update = client
            .check_and_download(
                repository_url,
                &self.code,
                &self.fingerprint.platform,
                &self.fingerprint.arch,
                current_version.as_deref(),
            )
            .await?;
        let mut github = self.github.read().await.clone();
        github.repository_url = Some(repository_url.to_owned());
        github.checked_at = Some(chrono::Utc::now().to_rfc3339());
        github.check_error = None;
        if let Some(downloaded) = update {
            let path = self.github_download_path(
                &downloaded.manifest.release_id,
                &downloaded.manifest.sha256,
            )?;
            let already_staged = std::fs::read(&path).ok().is_some_and(|bytes| {
                crate::plugin_github::validate_integrity(&downloaded.manifest, &bytes, None).is_ok()
            });
            if !already_staged {
                if path.exists() {
                    std::fs::remove_file(&path).map_err(|error| error.to_string())?;
                }
                crate::plugin_github::write_downloaded_asset(&path, &downloaded.bytes)?;
            }
            github.update = Some(crate::plugin_github::GithubUpdateInfo {
                release_id: downloaded.manifest.release_id.clone(),
                version: downloaded.manifest.version.clone(),
                changelog: downloaded.manifest.changelog.clone(),
                downloaded: true,
            });
            github.manifest = Some(downloaded.manifest);
        } else {
            github.update = None;
            github.manifest = None;
        }
        Ok(github)
    }

    async fn register_github_source(
        &self,
        repository_url: &str,
        client: &crate::plugin_github::GithubReleaseClient,
    ) -> Result<(), String> {
        let _guard = self.update_lock.lock().await;
        let repository_url = crate::plugin_github::canonical_repository(repository_url)?;
        // Probe first. An invalid, private or inaccessible repository never
        // replaces a previously working source.
        let github = self.fetch_github_update(&repository_url, client).await?;
        self.persist_github_state(&github).await
    }

    async fn check_github_source(
        &self,
        client: &crate::plugin_github::GithubReleaseClient,
    ) -> Result<(), String> {
        let _guard = self.update_lock.lock().await;
        let current = self.github.read().await.clone();
        let Some(repository_url) = current.repository_url else {
            return Ok(());
        };
        match self.fetch_github_update(&repository_url, client).await {
            Ok(github) => self.persist_github_state(&github).await,
            Err(error) => {
                let mut retained = self.github.read().await.clone();
                retained.check_error = Some(error.clone());
                if let Err(persist_error) = self.persist_github_state(&retained).await {
                    tracing::warn!(plugin_code = %self.code, error = %persist_error, "GitHub plugin state could not persist");
                }
                Err(error)
            }
        }
    }

    async fn remove_github_source(&self) -> Result<(), String> {
        let _guard = self.update_lock.lock().await;
        let github = self.github.read().await.clone();
        if let Some(database) = &self.database {
            database
                .delete_local_setting(&self.github_key())
                .map_err(|error| error.to_string())?;
        }
        *self.github.write().await = crate::plugin_github::GithubPluginState::default();
        if let Some(manifest) = github.manifest {
            if let Ok(path) = self.github_download_path(&manifest.release_id, &manifest.sha256) {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }

    async fn apply_github_update(&self, release_id: &str) -> Result<(), LicenseClientError> {
        let _guard = self.update_lock.lock().await;
        let status = self.status.read().await.clone();
        if !status.active
            || !status
                .licensed_plugins
                .iter()
                .any(|code| code == &self.code)
        {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "este plugin precisa estar ativo pela licença para instalar a atualização",
            ));
        }
        let github = self.github.read().await.clone();
        let update = github
            .update
            .as_ref()
            .filter(|update| update.release_id == release_id && update.downloaded)
            .ok_or_else(|| update_unavailable("a release GitHub não está baixada"))?;
        let manifest = github
            .manifest
            .ok_or_else(|| update_unavailable("manifest GitHub ausente"))?;
        if manifest.release_id != update.release_id || manifest.version != update.version {
            return Err(update_unavailable("a release GitHub confirmada mudou"));
        }
        let installed_version = self
            .persisted
            .lock()
            .await
            .as_ref()
            .map(|persisted| persisted.plugin_version.as_str())
            .ok_or_else(|| {
                LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Ativação local ausente.")
            })?;
        let installed_version = semver::Version::parse(installed_version)
            .map_err(|_| update_unavailable("versão instalada do plugin não é SemVer válida"))?;
        let update_version = semver::Version::parse(&manifest.version)
            .map_err(|_| update_unavailable("versão GitHub não é SemVer válida"))?;
        if update_version <= installed_version {
            return Err(update_unavailable(
                "esta release GitHub não é mais nova que a instalada",
            ));
        }
        if let Some(preferences) = &manifest.preferences {
            crate::plugin_github::validate_preferences_schema(preferences)
                .map_err(|error| LicenseClientError::new(LicenseErrorCode::ServerError, error))?;
        }
        let staged_path = self
            .github_download_path(&manifest.release_id, &manifest.sha256)
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::ServerError, error))?;
        let bytes = std::fs::read(&staged_path).map_err(io_error)?;
        crate::plugin_github::validate_integrity(&manifest, &bytes, None)
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::ServerError, error))?;
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
        let already_installed = std::fs::read(&path).ok().is_some_and(|installed| {
            crate::plugin_github::validate_integrity(&manifest, &installed, None).is_ok()
        });
        if !already_installed {
            if path.exists() {
                std::fs::remove_file(&path).map_err(io_error)?;
            }
            install_plugin_atomically(&path, &bytes)?;
        }
        let previous = self.persisted.lock().await.clone().ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::LeaseUnknown, "Ativação local ausente.")
        })?;
        let mut persisted = previous.clone();
        persisted.installed_release_id = installed_id;
        persisted.plugin_version = manifest.version.clone();
        persisted.plugin_sha256 = manifest.sha256.clone();
        persisted.plugin_path = Some(path.to_string_lossy().into_owned());
        persisted.features = features_for_code(&self.code, &manifest.feature_set);
        persisted.preferences_schema = manifest.preferences.clone();
        persisted.installed_at = Some(chrono::Utc::now().to_rfc3339());
        let old_path = previous.plugin_path.clone();
        let database_path = self
            .database
            .as_ref()
            .map(|database| database.path().with_file_name(format!("{}.db", self.code)))
            .unwrap_or_else(|| crate::config::project_root().join(format!("{}.db", self.code)));
        *self.persisted.lock().await = Some(persisted.clone());
        if let Some(old_host) = self.host.lock().await.take() {
            let _ = old_host.stop().await;
        }
        let host = match self.start_host(&path, &database_path).await {
            Ok(host) => host,
            Err(error) => {
                *self.persisted.lock().await = Some(previous.clone());
                if let Some(old_path) = old_path
                    .as_deref()
                    .map(std::path::Path::new)
                    .filter(|path| path.is_file())
                {
                    match self.start_host(old_path, &database_path).await {
                        Ok(host) => *self.host.lock().await = Some(host),
                        Err(restart_error) => {
                            tracing::error!(plugin_code = %self.code, error = %restart_error, "previous plugin host could not be restored after GitHub update failure")
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
            if let Err(error) = database.save_local_setting(&self.setting_key(), &persisted) {
                let _ = host.stop().await;
                *self.persisted.lock().await = Some(previous);
                if let Some(old_path) = old_path
                    .as_deref()
                    .map(std::path::Path::new)
                    .filter(|path| path.is_file())
                {
                    match self.start_host(old_path, &database_path).await {
                        Ok(host) => *self.host.lock().await = Some(host),
                        Err(restart_error) => {
                            tracing::error!(plugin_code = %self.code, error = %restart_error, "previous plugin host could not be restored after GitHub state persistence failure")
                        }
                    }
                }
                return Err(LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    error.to_string(),
                ));
            }
        }
        *self.host.lock().await = Some(host);
        *self.persisted.lock().await = Some(persisted.clone());
        let mut status = self.status.write().await;
        status.plugin_version = Some(persisted.plugin_version);
        status.features = persisted.features;
        status.installed_at = persisted.installed_at;
        drop(status);
        let mut github = self.github.read().await.clone();
        if github
            .update
            .as_ref()
            .is_some_and(|update| update.release_id == release_id)
        {
            github.update = None;
            github.manifest = None;
            match self.persist_github_state(&github).await {
                Ok(()) => {
                    let _ = std::fs::remove_file(staged_path);
                }
                Err(error) => {
                    *self.github.write().await = github;
                    tracing::warn!(plugin_code = %self.code, error = %error, "GitHub plugin update state could not be cleared after install");
                }
            }
        }
        Ok(())
    }
}

/// Coleção dinâmica de [`PluginRuntime`], uma por `plugin_code` concedido ou
/// restaurado. É o único
/// ponto de entrada do resto do backend para o sistema de licenciamento de
/// plugins - `crate::backend::gateway::GatewayState` mantém um
/// `Arc<PluginManager>`.
pub(crate) struct PluginManager {
    fingerprint: Fingerprint,
    service: Arc<dyn LicenseService>,
    database: Option<Arc<crate::db::PomDb>>,
    runtimes: StdRwLock<HashMap<String, Arc<PluginRuntime>>>,
    host_context: HostContext,
    github_client: crate::plugin_github::GithubReleaseClient,
}

impl PluginManager {
    /// `restore_on_boot` liga a releitura do estado persistido de cada plugin
    /// (ver [`PluginRuntime::restore`]). Em produção é sempre `true`: é o que
    /// religa um plugin já ativado depois de um restart do `pom`. Os testes
    /// passam `false` porque a restauração é uma tarefa concorrente que
    /// sobrescreve o estado em memória a partir do banco em um instante que
    /// o teste não controla - deixá-la ligada tornaria qualquer cenário que
    /// monta estado à mão não determinístico, sem cobrir nada a mais.
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
            crate::plugin_github::GithubReleaseClient::new(),
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
        let manager = Arc::new(Self {
            fingerprint,
            service,
            database,
            runtimes: StdRwLock::new(runtimes),
            host_context,
            github_client,
        });
        let mut restore_tasks = Vec::new();
        for runtime in manager.runtime_values() {
            if restore_on_boot {
                restore_tasks.push(tokio::spawn({
                    let runtime = runtime.clone();
                    async move { runtime.restore().await }
                }));
            }
            runtime.spawn_heartbeat_supervisor();
        }
        if restore_on_boot {
            let manager = manager.clone();
            tokio::spawn(async move {
                for restore in restore_tasks {
                    let _ = restore.await;
                }
                loop {
                    for runtime in manager.runtime_values() {
                        if let Err(error) =
                            runtime.check_github_source(&manager.github_client).await
                        {
                            tracing::warn!(plugin_code = %runtime.code, error = %error, "GitHub plugin update check failed");
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(6 * 60 * 60)).await;
                }
            });
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
                        .local_setting_keys_with_prefix(GITHUB_SETTING_PREFIX)
                        .unwrap_or_default(),
                );
                keys.into_iter()
                    .filter_map(|key| {
                        key.strip_prefix(PERSISTED_SETTING_PREFIX)
                            .or_else(|| key.strip_prefix(GITHUB_SETTING_PREFIX))
                            .map(str::to_owned)
                    })
                    .filter(|code| is_valid_plugin_code(code))
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
            }
            Err(error) => {
                tracing::warn!(error = %error, "licenças de plugins locais não puderam ser enumeradas na inicialização");
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

    /// Como `test_manager`, mas com um `PomDb` real para persistir o código e
    /// estado de cada plugin.
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
        let github = crate::plugin_github::GithubReleaseClient::with_api_base(api_base)
            .expect("test GitHub API endpoint");
        Self::from_codes_with_github(
            plugin_codes.iter().map(|code| (*code).to_owned()),
            service,
            test_fingerprint(),
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

    /// Troca o contexto entregue aos hosts de plugin e o reentrega aos que já
    /// estão rodando. Hosts iniciados depois recebem o novo ao subir.
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
                "o servidor retornou um plugin_code inválido",
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

    /// Status local dos plugins já conhecidos por esta instância, ordenados
    /// pelo código. A lista pública do servidor é a fonte do catálogo exibido.
    pub(crate) async fn all_statuses(&self) -> Vec<PluginStatus> {
        let runtimes = self.runtime_values();
        let mut statuses = Vec::with_capacity(runtimes.len());
        for runtime in runtimes {
            statuses.push(runtime.status().await);
        }
        statuses.sort_by(|a, b| a.code.cmp(&b.code));
        statuses
    }

    pub(crate) async fn set_github_source(
        &self,
        code: &str,
        repository_url: &str,
    ) -> Result<(), LicenseClientError> {
        if !is_valid_plugin_code(code) {
            return Err(update_unavailable("plugin_code inválido"));
        }
        let runtime = self.get_or_create_runtime(code)?;
        runtime
            .register_github_source(repository_url, &self.github_client)
            .await
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, error))
    }

    pub(crate) async fn remove_github_source(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self
            .runtime_for(code)
            .ok_or_else(|| update_unavailable("plugin desconhecido"))?;
        runtime
            .remove_github_source()
            .await
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::ServerError, error))
    }

    pub(crate) async fn check_github_update(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self
            .runtime_for(code)
            .ok_or_else(|| update_unavailable("plugin desconhecido"))?;
        runtime
            .check_github_source(&self.github_client)
            .await
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, error))
    }

    pub(crate) async fn apply_github_update(
        &self,
        code: &str,
        release_id: &str,
    ) -> Result<(), LicenseClientError> {
        let runtime = self
            .runtime_for(code)
            .ok_or_else(|| update_unavailable("plugin desconhecido"))?;
        runtime.apply_github_update(release_id).await
    }

    pub(crate) async fn save_preferences(
        &self,
        code: &str,
        preferences: enterprise_protocol::PluginPreferences,
    ) -> Result<(), LicenseClientError> {
        let runtime = self
            .runtime_for(code)
            .ok_or_else(|| update_unavailable("plugin desconhecido"))?;
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

    pub(crate) async fn query_audit(
        &self,
        code: &str,
        operation: &str,
        query: serde_json::Value,
    ) -> Option<serde_json::Value> {
        self.runtime_for(code)?.query_audit(operation, query).await
    }

    pub(crate) async fn ui_manifest(
        &self,
        code: &str,
    ) -> Option<crate::plugin_ui::PluginUiManifest> {
        self.runtime_for(code)?.ui_manifest().await
    }

    /// Valida a chave uma vez para descobrir quais `plugin_code` ela concede,
    /// cria os runtimes necessários e ativa/instala cada um deles. Concede
    /// sucesso se ao menos um plugin ativou; plugins que falharam ficam só no
    /// log - o operador vê o resultado real na lista de status atualizada.
    pub(crate) async fn activate(
        &self,
        license_key: &str,
    ) -> Result<Vec<String>, LicenseClientError> {
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
                    "o servidor retornou um plugin_code inválido",
                ));
            }
            if seen.insert(code.clone()) {
                granted_plugins.push(code);
            }
        }
        if granted_plugins.is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "A chave não concede nenhum plugin.",
            ));
        }
        let mut activated = Vec::new();
        let mut last_error = None;
        for code in granted_plugins {
            let runtime = self.get_or_create_runtime(&code)?;
            match runtime.activate_and_install(license_key).await {
                Ok(()) => activated.push(code),
                Err(error) => {
                    tracing::warn!(plugin_code = %code, error = %error, "ativação do plugin falhou");
                    last_error = Some(error);
                }
            }
        }
        if activated.is_empty() {
            return Err(last_error.unwrap_or_else(|| {
                LicenseClientError::new(
                    LicenseErrorCode::ServerError,
                    "nenhum plugin concedido pôde ser ativado",
                )
            }));
        }
        Ok(activated)
    }

    pub(crate) async fn deactivate(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self.runtime_for(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "plugin desconhecido")
        })?;
        runtime.deactivate().await
    }

    pub(crate) async fn refresh(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self.runtime_for(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "plugin desconhecido")
        })?;
        runtime.heartbeat(&uuid::Uuid::new_v4().to_string()).await?;
        if let Err(error) = runtime.detect_update().await {
            tracing::warn!(plugin_code = %code, error = %error, "detecção de atualização do plugin falhou");
        }
        if let Err(error) = runtime.refresh_license_status().await {
            tracing::warn!(plugin_code = %code, error = %error, "checagem de validade da licença falhou");
        }
        Ok(())
    }

    pub(crate) async fn check_update(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self.runtime_for(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "plugin desconhecido")
        })?;
        runtime.detect_update().await
    }

    pub(crate) async fn apply_update(
        &self,
        code: &str,
        release_id: &str,
    ) -> Result<(), LicenseClientError> {
        let runtime = self.runtime_for(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "plugin desconhecido")
        })?;
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

/// Mantém o contexto de `host.configure` em dia: o endpoint OpenAI deste nó
/// em loopback (texto claro de loopback é sempre aceito, mesmo com TLS) e a
/// chave de chat do nó. Reavaliado periodicamente para acompanhar uma chave
/// revogada e recriada; um nó que ainda não alcança o principal tenta de novo.
pub(crate) fn spawn_host_context(state: crate::gateway::GatewayState) {
    const REFRESH: std::time::Duration = std::time::Duration::from_secs(600);
    const RETRY: std::time::Duration = std::time::Duration::from_secs(30);
    tokio::spawn(async move {
        loop {
            let wait = match crate::admin::node_chat_key(&state).await {
                Ok((key, _)) => {
                    let context = serde_json::json!({
                        "gateway": {
                            "openai_base_url": format!("http://127.0.0.1:{}/v1", state.gateway_port),
                            "api_key": key.key,
                        }
                    });
                    state.plugins.set_host_context(context).await;
                    REFRESH
                }
                Err(error) => {
                    tracing::warn!(%error, "contexto dos plugins sem API key; nova tentativa em 30 s");
                    RETRY
                }
            };
            tokio::time::sleep(wait).await;
        }
    });
}

/// As features que UM `plugin_code` entrega, lidas do `feature_set` do
/// manifesto do release no formato `"<plugin_code>.<feature>"`, sem o prefixo.
/// Um release só declara as suas próprias features, mas o prefixo é filtrado
/// mesmo assim: é ele que garante que um manifesto trocado não empreste
/// feature de outro plugin.
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
        return Err("preferência booleana desconhecida para este plugin".into());
    }
    match (&schema.storage_directory, &values.storage_directory) {
        (None, Some(_)) => return Err("este plugin não aceita diretório de armazenamento".into()),
        (Some(_), Some(directory)) => {
            if directory.trim().is_empty() || directory.len() > 4096 || directory.contains('\0') {
                return Err("diretório de armazenamento inválido".into());
            }
            let path = std::path::Path::new(directory);
            if !path.is_absolute() || !path.is_dir() {
                return Err("o diretório de armazenamento precisa existir e ser absoluto".into());
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
            tracing::warn!(error = %error, "ativação de plugin falhou");
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
pub(crate) struct GithubSourceRequest {
    pub repository_url: String,
}

pub(crate) async fn set_plugin_github_source(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
    axum::Json(request): axum::Json<GithubSourceRequest>,
) -> axum::response::Response {
    match state
        .plugins
        .set_github_source(&code, &request.repository_url)
        .await
    {
        Ok(()) => axum::Json(state.plugins.all_statuses().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn remove_plugin_github_source(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
) -> axum::response::Response {
    match state.plugins.remove_github_source(&code).await {
        Ok(()) => axum::Json(state.plugins.all_statuses().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn check_plugin_github_update(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
) -> axum::response::Response {
    match state.plugins.check_github_update(&code).await {
        Ok(()) => axum::Json(state.plugins.all_statuses().await).into_response(),
        Err(error) => license_error_response(error),
    }
}

pub(crate) async fn apply_plugin_github_update(
    _auth: crate::auth::ConnectionAuth,
    axum::extract::State(state): axum::extract::State<crate::gateway::GatewayState>,
    axum::extract::Path(code): axum::extract::Path<String>,
    axum::Json(request): axum::Json<PluginUpdateApplyRequest>,
) -> axum::response::Response {
    match state
        .plugins
        .apply_github_update(&code, &request.release_id)
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
            axum::Json(serde_json::json!({"error": "desafio de fingerprint inválido"})),
        )
            .into_response();
    }
    let identity = crate::identity::Identity::load_or_create(&crate::identity::key_path());
    if identity.node_id.0 != state.plugins.fingerprint().node_id {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": "identidade do peer divergente"})),
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
    code: &str,
    release_id: &str,
    os: &str,
    arch: &str,
) -> Result<std::path::PathBuf, LicenseClientError> {
    // '+' aparece no metadado de build SemVer (ex: "ent-0.1.0+build.1-linux-x86_64"),
    // gerado pelo auto-incremento de versao do pom-release.sh. E seguro em nomes de
    // arquivo nas tres plataformas (Linux/macOS/Windows) e nao habilita path
    // traversal, pois nao e '.'/'/'/'\\'.
    if release_id.is_empty()
        || !release_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
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
            "manifest do plugin incompatível com a ativação",
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
            "manifest do plugin incompatível com esta instalação",
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
fn test_fingerprint() -> Fingerprint {
    // Runtime OS/arch, not a hardcoded pair: `validate_manifest`/`detect_update`
    // compare a manifest's `os`/`arch` against `std::env::consts::{OS,ARCH}`,
    // so a fixed "linux"/"x86_64" pair only passed on that exact CI target and
    // failed every local run on macOS or another architecture.
    Fingerprint::from_components(
        std::env::consts::OS,
        std::env::consts::ARCH,
        "node-a",
        "machine",
        "board",
        "installation",
    )
}

#[cfg(test)]
pub(crate) mod tests_support {
    //! Dublê de [`LicenseService`] compartilhado pelos testes deste crate. Ao
    //! contrário do antigo `MockLicenseService` de plugin único, este guarda
    //! uma resposta POR `plugin_code`, para que testes possam provar que dois
    //! plugins ativam e falham de forma independente.
    use super::*;

    #[derive(Clone)]
    pub(crate) struct MockLicenseService {
        responses: Arc<Mutex<HashMap<String, MockResponse>>>,
        heartbeat_error: Arc<Mutex<Option<LicenseErrorCode>>>,
        /// `Some(release_id)` faz o próximo heartbeat responder
        /// `release_changed: true` e `available_release_id: Some(release_id)`,
        /// como o servidor de licenças faz ao detectar uma release nova.
        heartbeat_release_signal: Arc<Mutex<Option<String>>>,
        artifacts: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        manifest_calls: Arc<Mutex<Vec<String>>>,
        manifest_errors: Arc<Mutex<HashMap<String, LicenseErrorCode>>>,
        download_calls: Arc<Mutex<u32>>,
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
                .expect("uso síncrono em setup de teste")
                .insert(code.to_owned(), MockResponse::Valid(response));
            self.artifacts
                .try_lock()
                .expect("uso síncrono em setup de teste")
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
                    "mock: plugin_code sem resposta registrada",
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
                    "mock: plugin_code sem manifest registrado",
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
    use super::tests_support::MockLicenseService;
    use super::*;
    use enterprise_protocol::{ActivationResponse, Grant, LicenseErrorCode, PluginManifest};

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
    fn plugin_path_accepts_semver_build_metadata_in_release_id() {
        let path = plugin_path(
            "enterprise",
            "ent-0.1.0+build.1-linux-x86_64",
            "linux",
            "x86_64",
        )
        .expect("release_id com '+' de metadado de build deve ser aceito");

        assert!(path
            .to_string_lossy()
            .ends_with("plugins/enterprise/ent-0.1.0+build.1-linux-x86_64-x86_64.so"));
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

    /// As features de um plugin vêm do `feature_set` do manifesto do release,
    /// e cada release só declara as suas. O prefixo `"<plugin_code>."` é
    /// filtrado mesmo assim: é ele que impede que um manifesto de outro
    /// plugin conceda feature a este.
    #[test]
    fn features_for_code_takes_only_the_features_of_that_plugin() {
        let feature_set = vec![
            "enterprise.request_logs".to_string(),
            "enterprise.statistics".to_string(),
            "sample.other_feature".to_string(),
        ];

        assert_eq!(
            features_for_code("enterprise", &feature_set),
            vec!["request_logs".to_string(), "statistics".to_string()]
        );
        assert_eq!(
            features_for_code("sample", &feature_set),
            vec!["other_feature".to_string()]
        );
        assert!(features_for_code("outro", &feature_set).is_empty());
    }

    /// Monta um plugin "pronto" sem manifest/download/host real - a mesma
    /// divisão em duas etapas que a suíte original usava, porque subir o
    /// host de verdade é integração testada à parte.
    async fn activate_ready(runtime: &Arc<PluginRuntime>, license_key: &str, version: &str) {
        runtime.activate(license_key).await.expect("activate");
        runtime
            .mark_plugin_ready(version)
            .await
            .expect("mark ready");
    }

    #[tokio::test]
    async fn plugin_preferences_use_manifest_defaults_and_persist_locally_by_code() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database =
            Arc::new(crate::db::PomDb::open(directory.path().join("pom.db")).expect("database"));
        let service = Arc::new(MockLicenseService::new());
        let mut response = activation("enterprise", "ent-1");
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
            .grant("enterprise", response, b"plugin-binary".to_vec())
            .await;
        let manager = PluginManager::test_manager_with_database(
            &["enterprise"],
            service.clone(),
            database.clone(),
        );
        let runtime = manager.runtime_for("enterprise").expect("runtime");
        activate_ready(&runtime, "PM-enterprise", "0.1.0").await;
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
        assert!(manager
            .save_preferences("enterprise", invalid_key)
            .await
            .is_err());
        let invalid_directory = enterprise_protocol::PluginPreferences {
            features: [("include_headers".into(), true)].into(),
            storage_directory: Some("relative/path".into()),
        };
        assert!(manager
            .save_preferences("enterprise", invalid_directory)
            .await
            .is_err());
        manager
            .save_preferences("enterprise", preferences.clone())
            .await
            .expect("save local preferences");
        assert_eq!(
            runtime.preferences_for(&schema),
            preferences,
            "local values must override schema defaults"
        );
        let persisted = database
            .local_setting_json("plugin_license:enterprise")
            .unwrap()
            .expect("license record");
        assert!(!persisted.contains(&storage.to_string_lossy().to_string()));

        let restored =
            PluginManager::test_manager_with_database(&["enterprise"], service, database);
        let restored_runtime = restored
            .runtime_for("enterprise")
            .expect("restored runtime");
        restored_runtime.restore().await;
        assert_eq!(
            restored_runtime.preferences_for(&schema),
            preferences,
            "local preferences survive a runtime restart"
        );
    }

    #[tokio::test]
    async fn github_download_is_staged_and_last_good_result_survives_network_failure_and_restart() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database =
            Arc::new(crate::db::PomDb::open(directory.path().join("pom.db")).expect("database"));
        let (_client, api_base, server) =
            crate::plugin_github::tests::github_stub(200, false, None, false).await;
        let service = Arc::new(MockLicenseService::new());
        let manager = PluginManager::test_manager_with_github_database(
            &["analytics"],
            service.clone(),
            database.clone(),
            &api_base,
        );
        manager
            .set_github_source("analytics", "https://github.com/pom/test-plugin")
            .await
            .expect("register and download public release");
        let status = manager.status("analytics").await.expect("plugin status");
        assert!(!status.active, "GitHub source cannot activate a plugin");
        assert!(manager
            .apply_github_update("analytics", "v1.2.0")
            .await
            .is_err());
        assert_eq!(
            status.github_repository.as_deref(),
            Some("https://github.com/pom/test-plugin")
        );
        let last_good = status.github_update.expect("downloaded GitHub update");
        assert!(last_good.downloaded);
        let runtime = manager.runtime_for("analytics").expect("runtime");
        let manifest = runtime
            .github
            .read()
            .await
            .manifest
            .clone()
            .expect("persisted GitHub manifest");
        let staged_path = runtime
            .github_download_path(&manifest.release_id, &manifest.sha256)
            .expect("staging path");
        assert_eq!(
            std::fs::read(&staged_path).expect("downloaded asset"),
            b"test plugin binary"
        );

        server.abort();
        assert!(manager.check_github_update("analytics").await.is_err());
        let retained = manager
            .status("analytics")
            .await
            .expect("status after failure");
        assert_eq!(retained.github_update, Some(last_good.clone()));
        assert!(retained.github_error.is_some());
        assert!(
            staged_path.is_file(),
            "network failure must retain the last good file"
        );

        let restarted = PluginManager::test_manager_with_github_database(
            &["analytics"],
            service,
            database,
            "http://127.0.0.1:1",
        );
        let restored = restarted
            .status("analytics")
            .await
            .expect("restored status");
        assert_eq!(restored.github_update, Some(last_good));
        assert_eq!(
            restored.github_repository.as_deref(),
            Some("https://github.com/pom/test-plugin")
        );
        assert!(staged_path.is_file());
    }

    #[tokio::test]
    async fn two_plugins_activate_and_deactivate_independently() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant(
                "enterprise",
                activation("enterprise", "ent-1"),
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
        let manager = PluginManager::test_manager(&["enterprise", "sample"], service.clone());
        let enterprise = manager.runtime_for("enterprise").expect("runtime");
        let sample = manager.runtime_for("sample").expect("runtime");
        activate_ready(&enterprise, "PM-enterprise", "0.1.0").await;
        activate_ready(&sample, "PM-sample", "0.1.0").await;

        assert!(manager.status("enterprise").await.expect("status").active);
        assert!(manager.status("sample").await.expect("status").active);

        manager
            .deactivate("enterprise")
            .await
            .expect("deactivate enterprise");

        assert!(!manager.status("enterprise").await.expect("status").active);
        assert!(
            manager.status("sample").await.expect("status").active,
            "revogar um plugin não pode desativar o outro"
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
            crate::db::PomDb::open(directory.path().join("pom.db")).expect("abrir banco de teste"),
        );
        database
            .save_local_setting("plugin_license:future.plugin-v2", &serde_json::json!({}))
            .expect("salvar código de plugin persistido");

        let manager = PluginManager::runtime_with_database("node-dynamic", database);

        assert!(manager.runtime_for("future.plugin-v2").is_some());
        assert!(manager.runtime_for(ENTERPRISE_PLUGIN_CODE).is_none());
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
    async fn activation_batches_enterprise_and_base_without_a_compiled_catalog() {
        let service = Arc::new(MockLicenseService::new());
        for (code, activation_id) in [("enterprise", "ent-1"), ("base", "base-1")] {
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
            .activate("PM-enterprise-and-base")
            .await
            .expect_err("both mock manifests fail after their codes are selected");

        assert_eq!(error.code(), LicenseErrorCode::ServerError);
        let mut calls = service.manifest_calls().await;
        calls.sort();
        assert_eq!(calls, ["base", "enterprise"]);
        assert!(manager.runtime_for("base").is_some());
        assert!(manager.runtime_for("enterprise").is_some());
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
        service
            .deny("enterprise", LicenseErrorCode::InvalidKey)
            .await;
        let manager = PluginManager::test_manager(&["enterprise"], service);

        let error = manager
            .activate("PM-invalid")
            .await
            .expect_err("must reject");

        assert_eq!(error.code(), LicenseErrorCode::InvalidKey);
        assert!(!manager.status("enterprise").await.expect("status").active);
    }

    #[tokio::test]
    async fn heartbeat_persists_authoritative_lease_data() {
        let service = Arc::new(MockLicenseService::single(
            "enterprise",
            activation("enterprise", "ent-1"),
        ));
        let manager = PluginManager::test_manager(&["enterprise"], service);
        let runtime = manager.runtime_for("enterprise").expect("runtime");
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
        assert_eq!(persisted.licensed_plugins, ["enterprise"]);
        assert_eq!(
            persisted.activation_candidates[0].plugin_code,
            "next_plugin"
        );

        let status = manager.status("enterprise").await.expect("status");
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

    /// O clique manual em "Atualizar" (`PluginManager::refresh`) publica
    /// `license_expires_at` a partir de `/v1/licenses/status` - a validade da
    /// LICENÇA, que o heartbeat sozinho não traz (só renova o
    /// `lease_expires_at` técnico). As duas datas são deliberadamente
    /// diferentes aqui para provar que a tela nunca mostra o lease no lugar
    /// da licença.
    #[tokio::test]
    async fn refresh_publishes_the_license_expiry_from_the_status_endpoint() {
        let service = Arc::new(MockLicenseService::single(
            "enterprise",
            activation("enterprise", "ent-1"),
        ));
        service
            .set_license_expires_at(Some("2027-09-15T00:00:00.000Z"))
            .await;
        let manager = PluginManager::test_manager(&["enterprise"], service);
        let runtime = manager.runtime_for("enterprise").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;
        assert_eq!(
            runtime.status().await.license_expires_at.as_deref(),
            Some("2099-12-31T00:00:00Z")
        );

        manager.refresh("enterprise").await.expect("refresh");

        let status = manager.status("enterprise").await.expect("status");
        assert_eq!(
            status.license_expires_at.as_deref(),
            Some("2027-09-15T00:00:00.000Z")
        );
        assert_ne!(status.license_expires_at, status.lease_expires_at);
    }

    #[tokio::test]
    async fn transient_heartbeat_failure_enters_degraded_without_deactivating() {
        let service = Arc::new(MockLicenseService::single(
            "enterprise",
            activation("enterprise", "ent-1"),
        ));
        let manager = PluginManager::test_manager(&["enterprise"], service);
        let runtime = manager.runtime_for("enterprise").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;

        runtime.mark_heartbeat_failure(true).await;

        let status = runtime.status().await;
        assert_eq!(status.state, LicenseState::Degraded);
        assert!(status.active, "uma falha transitória não desativa o plugin");
    }

    #[tokio::test]
    async fn revoked_heartbeat_disables_only_that_plugin() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant(
                "enterprise",
                activation("enterprise", "ent-1"),
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
        let manager = PluginManager::test_manager(&["enterprise", "sample"], service.clone());
        let enterprise = manager.runtime_for("enterprise").expect("runtime");
        let sample = manager.runtime_for("sample").expect("runtime");
        activate_ready(&enterprise, "PM-enterprise", "0.1.0").await;
        activate_ready(&sample, "PM-sample", "0.1.0").await;
        service.set_heartbeat_error(LicenseErrorCode::Revoked).await;

        let runtime = manager.runtime_for("enterprise").expect("runtime");
        let error = runtime.heartbeat("nonce-1").await.expect_err("revoked");

        assert_eq!(error.code(), LicenseErrorCode::Revoked);
        assert!(!manager.status("enterprise").await.expect("status").active);
        assert!(
            manager.status("sample").await.expect("status").active,
            "o heartbeat de um plugin não deve afetar o outro"
        );
    }

    #[tokio::test]
    async fn persisted_state_never_contains_the_license_key() {
        let service = Arc::new(MockLicenseService::single(
            "enterprise",
            activation("enterprise", "ent-1"),
        ));
        let manager = PluginManager::test_manager(&["enterprise"], service);
        let runtime = manager.runtime_for("enterprise").expect("runtime");
        activate_ready(&runtime, "PM-secret-license", "0.1.0").await;

        let persisted = serde_json::to_vec(&runtime.persisted_state().await.expect("persisted"))
            .expect("state JSON");

        assert!(!String::from_utf8_lossy(&persisted).contains("PM-secret-license"));
        assert!(String::from_utf8_lossy(&persisted).contains("lease-enterprise"));
    }

    /// `apply_update` baixando e reiniciando o host de verdade é integração
    /// testada à parte (precisa do binário real `pom-plugin-host`); aqui só
    /// `detect_update` roda de ponta a ponta, e a rejeição de `apply_update`
    /// por release não confirmada é o outro ramo que não toca download nem
    /// host.
    #[tokio::test]
    async fn detects_a_plugins_own_update_without_touching_pom_core() {
        let mut updated = activation("enterprise", "ent-1");
        let service = Arc::new(MockLicenseService::single("enterprise", updated.clone()));
        let manager = PluginManager::test_manager(&["enterprise"], service.clone());
        let runtime = manager.runtime_for("enterprise").expect("runtime");
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
            .grant("enterprise", updated, b"plugin-binary".to_vec())
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

    /// Reproduz o bug relatado pelo captain: um heartbeat bem-sucedido não
    /// pode apagar uma atualização real, mesmo quando o release_id do
    /// servidor só muda no metadado de build SemVer (`+build.N`) - formato
    /// que a comparação por igualdade de string já tratava corretamente,
    /// mas que o heartbeat corrompia ao escrever seu próprio
    /// `plugin_release_id` como se fosse a release instalada, antes do
    /// `detect_update` seguinte (mesmo clique em "Atualizar",
    /// `PluginManager::refresh`) comparar contra essa base já adulterada.
    /// Sem revogar a release anterior em nenhum momento.
    #[tokio::test]
    async fn refresh_button_does_not_lose_an_update_signalled_by_heartbeat() {
        let mut updated = activation("enterprise", "ent-0.1.0+build.1-linux-x86_64");
        let service = Arc::new(MockLicenseService::single("enterprise", updated.clone()));
        let manager = PluginManager::test_manager(&["enterprise"], service.clone());
        let runtime = manager.runtime_for("enterprise").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;
        assert!(runtime.status().await.available_update.is_none());

        // O servidor publica a release seguinte, só com o metadado de build
        // avançado - nunca revogado, nunca desativado.
        updated.plugin_manifest.release_id = "ent-0.1.0+build.2-linux-x86_64".into();
        updated.plugin_manifest.version = "0.1.0+build.2".into();
        service
            .grant("enterprise", updated, b"plugin-binary".to_vec())
            .await;
        // O heartbeat do servidor já sinaliza a mudança, e (como o incidente
        // relatado) seu próprio campo `plugin_release_id` também avança para
        // a release nova - sem que nada tenha sido baixado ainda.
        service
            .signal_heartbeat_release_change("ent-0.1.0+build.2-linux-x86_64")
            .await;

        manager.refresh("enterprise").await.expect("refresh");

        let available = runtime
            .status()
            .await
            .available_update
            .expect("o heartbeat não pode apagar a atualização real");
        assert_eq!(available.release_id, "ent-0.1.0+build.2-linux-x86_64");
        assert_eq!(available.version, "0.1.0+build.2");
        // `installed_release_id` só avança com um download confirmado - o
        // heartbeat sozinho nunca pode reivindicar tê-lo feito.
        assert_eq!(
            runtime
                .persisted_state()
                .await
                .expect("persisted")
                .installed_release_id,
            "ent-0.1.0+build.1-linux-x86_64",
        );
    }

    /// O supervisor de heartbeat periódico (`spawn_heartbeat_supervisor`)
    /// nunca chamava `detect_update` por conta própria; uma release
    /// sinalizada em segundo plano, sem qualquer clique do operador, também
    /// precisa aparecer.
    #[tokio::test]
    async fn a_background_heartbeat_alone_also_surfaces_the_update() {
        let mut updated = activation("enterprise", "ent-1");
        let service = Arc::new(MockLicenseService::single("enterprise", updated.clone()));
        let manager = PluginManager::test_manager(&["enterprise"], service.clone());
        let runtime = manager.runtime_for("enterprise").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;

        updated.plugin_manifest.release_id = "ent-2".into();
        updated.plugin_manifest.version = "0.2.0".into();
        service
            .grant("enterprise", updated, b"plugin-binary".to_vec())
            .await;
        service.signal_heartbeat_release_change("ent-2").await;

        runtime.heartbeat("nonce-1").await.expect("heartbeat");

        let available = runtime
            .status()
            .await
            .available_update
            .expect("a release sinalizada em segundo plano deve aparecer");
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
        let mut updated = activation("enterprise", "ent-1");
        let service = Arc::new(MockLicenseService::single("enterprise", updated.clone()));
        let manager = PluginManager::test_manager(&["enterprise"], service.clone());
        let runtime = manager.runtime_for("enterprise").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;

        updated.plugin_release_id = "ent-2".into();
        updated.plugin_manifest.release_id = "ent-2".into();
        service
            .grant("enterprise", updated, b"plugin-binary".to_vec())
            .await;
        runtime.detect_update().await.expect("detect update");

        let error = runtime
            .apply_update("ent-not-confirmed")
            .await
            .expect_err("must require the advertised release");

        assert_eq!(error.code(), LicenseErrorCode::UpdateUnavailable);
        assert_eq!(service.download_calls().await, 0);
    }
    /// Reproduz o bug relatado pelo captain: uma revogação para o host de UM
    /// plugin; o próximo heartbeat bem-sucedido (self-heal, sem o processo
    /// `pom` inteiro reiniciar) precisa colocá-lo de pé de novo, senão
    /// Logs de Requisições/Estatísticas ficam mudos indefinidamente até o
    /// próximo restart completo.
    /// Os testes que sobem um host real mudam `POM_PLUGIN_HOST` e o log do
    /// fake host, variáveis do processo inteiro: rodam um de cada vez.
    static REAL_HOST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn fake_host_binary() -> std::path::PathBuf {
        let fake_host = std::env::current_exe()
            .expect("localizar binário de teste")
            .parent()
            .and_then(|deps| deps.parent())
            .expect("localizar diretório target/<profile>")
            .join(if cfg!(windows) {
                "pom-fake-enterprise-plugin-host.exe"
            } else {
                "pom-fake-enterprise-plugin-host"
            });
        assert!(
            fake_host.is_file(),
            "binário auxiliar {} não foi compilado; rode `cargo test -p node` (não `--lib`) para produzir todos os alvos [[bin]]",
            fake_host.display()
        );
        fake_host
    }

    /// Contexto do nó (`host.configure`) chega ao host na subida, no
    /// reinício e quando muda; e o proxy da UI encaminha pelo router real até
    /// o servidor que o plugin anuncia em `ui.upstream`.
    #[tokio::test]
    async fn host_context_and_ui_proxy_reach_a_real_plugin_host() {
        let _serial = REAL_HOST.lock().await;
        // Servidor web "do plugin": devolve o que recebeu.
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
        let plugin_path = dir.path().join("enterprise-plugin.bin");
        std::fs::write(&plugin_path, b"fake-plugin").expect("escrever plugin fake");
        let host_log = dir.path().join("host.log");
        let token = "t".repeat(32);
        // SAFETY: serializado por `REAL_HOST`.
        std::env::set_var("POM_PLUGIN_HOST", fake_host_binary());
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);
        std::env::set_var(
            "POM_FAKE_HOST_UPSTREAM",
            serde_json::json!({"status": "ready", "port": upstream_port, "token": token})
                .to_string(),
        );

        let db = Arc::new(
            crate::db::PomDb::open(dir.path().join("pom.db")).expect("abrir banco de teste"),
        );
        let service = Arc::new(MockLicenseService::single(
            "enterprise",
            activation("enterprise", "ent-1"),
        ));
        let manager =
            PluginManager::test_manager_with_database(&["enterprise"], service.clone(), db);
        let context = |key: &str| serde_json::json!({"gateway": {"openai_base_url": "http://127.0.0.1:8080/v1", "api_key": key}});
        manager.set_host_context(context("sk-first")).await;

        let runtime = manager.runtime_for("enterprise").expect("runtime");
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
            "host recém-iniciado recebe o contexto"
        );

        manager.set_host_context(context("sk-first")).await;
        assert_eq!(configures("sk-first"), 1, "contexto igual não é reenviado");
        manager.set_host_context(context("sk-second")).await;
        assert_eq!(
            configures("sk-second"),
            1,
            "host em execução recebe o contexto novo"
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
            .oneshot(request("/api/ui/plugins/enterprise/proxy/hello?x=1", None))
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["uri"], "/hello?x=1");
        assert_eq!(body["token"], token.as_str());
        assert_eq!(body["cookie"], false, "a sessão do POM não chega ao plugin");

        let forged = app
            .clone()
            .oneshot(request(
                "/api/ui/plugins/enterprise/proxy/hello",
                Some("http://evil.example"),
            ))
            .await
            .unwrap();
        assert_eq!(forged.status(), axum::http::StatusCode::FORBIDDEN);

        let unknown = app
            .oneshot(request("/api/ui/plugins/nope/proxy/hello", None))
            .await
            .unwrap();
        assert_eq!(unknown.status(), axum::http::StatusCode::NOT_FOUND);

        runtime.stop_host_for_test().await;
        runtime.ensure_host_running(&persisted).await;
        assert_eq!(
            configures("sk-second"),
            2,
            "host reiniciado recebe o contexto atual"
        );
        std::env::remove_var("POM_FAKE_HOST_UPSTREAM");
        runtime.stop_host_for_test().await;
    }

    #[tokio::test]
    async fn heartbeat_success_restarts_the_stopped_plugin_host() {
        let _serial = REAL_HOST.lock().await;
        // `CARGO_BIN_EXE_*` só é injetado em testes de integração
        // (`tests/*.rs`); este é um teste de unidade dentro do binário `pom`,
        // então localizamos o binário auxiliar do mesmo jeito que
        // `plugin_host::host_executable` localiza o `pom-plugin-host` real em
        // produção: ao lado do diretório de saída do cargo
        // (`target/<profile>/deps/pom-<hash>` -> `target/<profile>/`).
        let fake_host = std::env::current_exe()
            .expect("localizar binário de teste")
            .parent()
            .and_then(|deps| deps.parent())
            .expect("localizar diretório target/<profile>")
            .join(if cfg!(windows) {
                "pom-fake-enterprise-plugin-host.exe"
            } else {
                "pom-fake-enterprise-plugin-host"
            });
        assert!(
            fake_host.is_file(),
            "binário auxiliar {} não foi compilado; rode `cargo test -p node` (não `--lib`) para produzir todos os alvos [[bin]]",
            fake_host.display()
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_path = dir.path().join("enterprise-plugin.bin");
        std::fs::write(&plugin_path, b"fake-plugin").expect("escrever plugin fake");
        let host_log = dir.path().join("ingest.log");

        // SAFETY: nenhum outro teste desta suíte lê ou grava
        // POM_PLUGIN_HOST/POM_FAKE_HOST_LOG; só este exercita um
        // PluginHost::start() real.
        std::env::set_var("POM_PLUGIN_HOST", fake_host);
        std::env::set_var("POM_FAKE_HOST_LOG", &host_log);

        let db = Arc::new(
            crate::db::PomDb::open(dir.path().join("pom.db")).expect("abrir banco de teste"),
        );
        let service = Arc::new(MockLicenseService::single(
            "enterprise",
            activation("enterprise", "ent-1"),
        ));
        // `test_manager_with_database` não dispara o `restore()` de boot (ver
        // `from_codes`): aqui o estado é montado à mão logo abaixo, e uma
        // restauração concorrente o sobrescreveria a partir do banco em um
        // instante fora do controle do teste.
        let manager =
            PluginManager::test_manager_with_database(&["enterprise"], service.clone(), db);
        let runtime = manager.runtime_for("enterprise").expect("runtime");
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
            "host deveria subir na primeira ativação"
        );

        service.set_heartbeat_error(LicenseErrorCode::Revoked).await;
        runtime
            .heartbeat("nonce-revoke")
            .await
            .expect_err("heartbeat deve refletir a revogação");
        assert!(
            runtime.host.lock().await.is_none(),
            "host deveria parar quando a licença é revogada"
        );
        assert!(!runtime.status().await.active);

        service.clear_heartbeat_error().await;
        runtime
            .heartbeat("nonce-self-heal")
            .await
            .expect("heartbeat de self-heal deve ter sucesso");

        assert!(
            runtime.host.lock().await.is_some(),
            "heartbeat de sucesso deveria reiniciar o host sem restart do processo pom"
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
            "ingest deveria funcionar com o host restaurado"
        );
        let events = std::fs::read_to_string(&host_log).expect("ler log do fake host");
        assert!(
            events.contains("req-1"),
            "evento de auditoria deveria ter chegado ao host reiniciado"
        );
    }
}
