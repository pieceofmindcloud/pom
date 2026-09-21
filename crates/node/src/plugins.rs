//! Gerenciador genérico de plugins independentes.
//!
//! Até a decisão do captain (2026-09-18), o POM tinha uma única "licença
//! Enterprise" para um único produto opcional. Esse modelo virou N plugins
//! independentes, cada um com seu próprio `plugin_code`, ciclo de ativação,
//! heartbeat e host de processo. Hoje o catálogo (`PLUGIN_CATALOG`) só tem uma
//! entrada - `"enterprise"`, o plugin de Estatísticas/Logs de Requisições -
//! mas a estrutura abaixo é genérica para N: [`PluginManager`] mantém uma
//! [`PluginRuntime`] por `plugin_code` conhecido, e cada uma tem seu próprio
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
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Catálogo fixo de plugins conhecidos por este binário. Cada entrada é um
/// `plugin_code`; a estrutura de dados e o código ao redor são genéricos para
/// N, mesmo com uma entrada só hoje. Adicionar um plugin novo é adicionar uma
/// linha aqui - nada mais neste arquivo assume que existe só um.
pub(crate) const PLUGIN_CATALOG: &[&str] = &["enterprise"];

/// O plugin de Estatísticas/Logs de Requisições, o único do catálogo hoje.
/// Fica nomeado porque `crate::reqlog`, `crate::backend::gateway::generation`
/// e `crate::pom_core_update` precisam de um `plugin_code` concreto, não do
/// catálogo inteiro.
pub(crate) const ENTERPRISE_PLUGIN_CODE: &str = "enterprise";

const PERSISTED_SETTING_PREFIX: &str = "plugin_license:";

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
    pub lease_expires_at: String,
    pub heartbeat_due_at: String,
    pub grace_seconds: u64,
    pub last_success_at: Option<String>,
    pub plugin_release_id: String,
    pub plugin_version: String,
    pub features: Vec<String>,
    pub plugin_sha256: String,
    pub plugin_path: Option<String>,
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
    pub available_update: Option<PluginUpdateInfo>,
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
            available_update: None,
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
}

impl PluginRuntime {
    fn new(
        code: &str,
        service: Arc<dyn LicenseService>,
        fingerprint: Fingerprint,
        database: Option<Arc<crate::db::PomDb>>,
    ) -> Self {
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
        }
    }

    fn setting_key(&self) -> String {
        format!("{PERSISTED_SETTING_PREFIX}{}", self.code)
    }

    pub(crate) async fn status(&self) -> PluginStatus {
        self.status.read().await.clone()
    }

    pub(crate) async fn persisted_state(&self) -> Option<PersistedPluginState> {
        self.persisted.lock().await.clone()
    }

    /// Restaura o estado persistido (se houver) e, se o plugin já tinha um
    /// binário instalado e a licença continua válida (online ou dentro da
    /// graça offline), religa o host de processo. Roda uma vez na construção
    /// do runtime.
    async fn restore(self: &Arc<Self>) {
        let Some(persisted) = self
            .database
            .as_ref()
            .and_then(|database| database.local_setting_json(&self.setting_key()).ok().flatten())
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
            let allowed = match remote_state {
                Ok(remote) => {
                    remote.payload.activation_id == persisted.activation_id
                        && remote.payload.lease_id == persisted.lease_id
                        && matches!(remote.payload.state.as_str(), "active" | "grace")
                }
                Err(_) => lease_allows_offline(&persisted),
            };
            if !allowed {
                let mut status = this.status.write().await;
                status.active = false;
                status.state = LicenseState::Expired;
                return;
            }
            this.ensure_host_running(&persisted).await;
            this.restore_ready(&persisted).await;
        });
    }

    async fn restore_ready(&self, persisted: &PersistedPluginState) {
        let mut status = self.status.write().await;
        status.active = true;
        status.state = LicenseState::Active;
        status.features = persisted.features.clone();
        status.plugin_version = Some(persisted.plugin_version.clone());
        status.lease_expires_at = Some(persisted.lease_expires_at.clone());
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
        let host = match crate::plugin_host::PluginHost::start(&path, &database_path).await {
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
        match crate::plugin_host::PluginHost::start(&path, &database_path).await {
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
        let value = self.query_audit("ui.manifest", serde_json::json!({})).await?;
        match crate::plugin_ui::PluginUiManifest::parse(&self.code, value) {
            Ok(manifest) => Some(manifest),
            Err(error) => {
                tracing::warn!(plugin_code = %self.code, %error, "manifesto de UI do plugin recusado");
                None
            }
        }
    }

    pub(crate) async fn mark_plugin_ready(&self, plugin_version: &str) -> Result<(), LicenseClientError> {
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
        let features = features_for_code(
            &self.code,
            &pending.response.plugin_manifest.feature_set,
        );
        if features.is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "O release instalado não declara nenhuma feature para este plugin.",
            ));
        }
        let mut status = self.status.write().await;
        status.active = true;
        status.state = LicenseState::Active;
        status.features = features;
        status.plugin_version = Some(plugin_version.to_owned());
        status.lease_expires_at = Some(pending.response.lease_expires_at);
        status.available_update = None;
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
                    lease_expires_at,
                    heartbeat_due_at,
                    grace_seconds,
                    plugin_release_id,
                    release_changed,
                    available_release_id,
                    grant,
                    ..
                } = response.payload;
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
                    state.heartbeat_due_at = heartbeat_due_at;
                    state.grace_seconds = grace_seconds;
                    state.plugin_release_id = plugin_release_id.clone();
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
        let available = (manifest.manifest.release_id != persisted.plugin_release_id).then(|| {
            PluginUpdateInfo {
                release_id: manifest.manifest.release_id.clone(),
                version: manifest.manifest.version.clone(),
                changelog: manifest.manifest.changelog.clone(),
            }
        });
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
        let host = crate::plugin_host::PluginHost::start(&path, &database_path)
            .await
            .map_err(|error| LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string()))?;
        *self.host.lock().await = Some(host);
        let mut persisted = persisted;
        persisted.plugin_release_id = manifest.manifest.release_id.clone();
        persisted.plugin_version = manifest.manifest.version.clone();
        persisted.plugin_sha256 = manifest.manifest.sha256.clone();
        persisted.plugin_path = Some(path.to_string_lossy().into_owned());
        persisted.last_success_at = Some(chrono::Utc::now().to_rfc3339());
        if let Some(database) = &self.database {
            database
                .save_local_setting(&self.setting_key(), &persisted)
                .map_err(|error| {
                    LicenseClientError::new(LicenseErrorCode::ServerError, error.to_string())
                })?;
        }
        *self.persisted.lock().await = Some(persisted);
        let mut status = self.status.write().await;
        status.plugin_version = Some(manifest.manifest.version);
        status.available_update = None;
        Ok(())
    }
}

/// Coleção de [`PluginRuntime`], uma por `plugin_code` do catálogo. É o único
/// ponto de entrada do resto do backend para o sistema de licenciamento de
/// plugins - `crate::backend::gateway::GatewayState` mantém um
/// `Arc<PluginManager>`.
pub(crate) struct PluginManager {
    fingerprint: Fingerprint,
    service: Arc<dyn LicenseService>,
    runtimes: HashMap<String, Arc<PluginRuntime>>,
}

impl PluginManager {
    /// `restore_on_boot` liga a releitura do estado persistido de cada plugin
    /// (ver [`PluginRuntime::restore`]). Em produção é sempre `true`: é o que
    /// religa um plugin já ativado depois de um restart do `pom`. Os testes
    /// passam `false` porque a restauração é uma tarefa concorrente que
    /// sobrescreve o estado em memória a partir do banco em um instante que
    /// o teste não controla - deixá-la ligada tornaria qualquer cenário que
    /// monta estado à mão não determinístico, sem cobrir nada a mais.
    fn from_catalog(
        catalog: &[&str],
        service: Arc<dyn LicenseService>,
        fingerprint: Fingerprint,
        database: Option<Arc<crate::db::PomDb>>,
        restore_on_boot: bool,
    ) -> Arc<Self> {
        let runtimes = catalog
            .iter()
            .map(|code| {
                let runtime = Arc::new(PluginRuntime::new(
                    code,
                    service.clone(),
                    fingerprint.clone(),
                    database.clone(),
                ));
                (code.to_string(), runtime)
            })
            .collect::<HashMap<_, _>>();
        let manager = Arc::new(Self {
            fingerprint,
            service,
            runtimes,
        });
        for runtime in manager.runtimes.values() {
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
        Self::from_catalog(
            PLUGIN_CATALOG,
            Arc::new(HttpLicenseService::new(license_base_url())),
            fingerprint,
            Some(database),
            true,
        )
    }

    pub(crate) fn runtime(node_id: &str) -> Arc<Self> {
        let fingerprint = machine_fingerprint(node_id);
        Self::from_catalog(
            PLUGIN_CATALOG,
            Arc::new(HttpLicenseService::new(license_base_url())),
            fingerprint,
            None,
            true,
        )
    }

    #[cfg(test)]
    pub(crate) fn test_manager(catalog: &[&str], service: Arc<dyn LicenseService>) -> Arc<Self> {
        Self::from_catalog(catalog, service, test_fingerprint(), None, false)
    }

    /// Como `test_manager`, mas com um `PomDb` real - só os poucos testes que
    /// precisam persistir e restaurar (`plugin_license:<code>`) ou religar um
    /// host de verdade via `ensure_host_running` precisam disto.
    #[cfg(test)]
    pub(crate) fn test_manager_with_database(
        catalog: &[&str],
        service: Arc<dyn LicenseService>,
        database: Arc<crate::db::PomDb>,
    ) -> Arc<Self> {
        Self::from_catalog(catalog, service, test_fingerprint(), Some(database), false)
    }

    pub(crate) fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    pub(crate) fn service(&self) -> Arc<dyn LicenseService> {
        self.service.clone()
    }

    pub(crate) fn runtime_for(&self, code: &str) -> Option<Arc<PluginRuntime>> {
        self.runtimes.get(code).cloned()
    }

    pub(crate) async fn status(&self, code: &str) -> Option<PluginStatus> {
        match self.runtimes.get(code) {
            Some(runtime) => Some(runtime.status().await),
            None => None,
        }
    }

    /// Todo o catálogo, na ordem declarada, cada um com seu status atual -
    /// inclusive os que o operador nunca ativou.
    pub(crate) async fn all_statuses(&self) -> Vec<PluginStatus> {
        let mut statuses = Vec::with_capacity(self.runtimes.len());
        for runtime in self.runtimes.values() {
            statuses.push(runtime.status().await);
        }
        statuses.sort_by(|a, b| a.code.cmp(&b.code));
        statuses
    }

    pub(crate) async fn ingest_audit_event(&self, code: &str, event: crate::reqlog::AuditEvent) -> bool {
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

    pub(crate) async fn ui_manifest(&self, code: &str) -> Option<crate::plugin_ui::PluginUiManifest> {
        self.runtime_for(code)?.ui_manifest().await
    }

    /// Valida a chave uma vez para descobrir quais `plugin_code` conhecidos
    /// ela concede, então ativa e instala cada um deles. Concede sucesso se
    /// ao menos um plugin ativou; plugins que falharam ficam só no log - o
    /// operador vê o resultado real na lista de status atualizada.
    pub(crate) async fn activate(&self, license_key: &str) -> Result<Vec<String>, LicenseClientError> {
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
        let known_granted = validation
            .payload
            .licensed_plugins
            .into_iter()
            .filter(|code| self.runtimes.contains_key(code))
            .collect::<Vec<_>>();
        if known_granted.is_empty() {
            return Err(LicenseClientError::new(
                LicenseErrorCode::FeatureNotEntitled,
                "A chave não concede nenhum plugin conhecido por este nó.",
            ));
        }
        let mut activated = Vec::new();
        let mut last_error = None;
        for code in known_granted {
            let runtime = self
                .runtimes
                .get(&code)
                .expect("filtrado contra runtimes conhecidos");
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
        let runtime = self.runtimes.get(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "plugin desconhecido")
        })?;
        runtime.deactivate().await
    }

    pub(crate) async fn refresh(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self.runtimes.get(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "plugin desconhecido")
        })?;
        runtime.heartbeat(&uuid::Uuid::new_v4().to_string()).await?;
        if let Err(error) = runtime.detect_update().await {
            tracing::warn!(plugin_code = %code, error = %error, "detecção de atualização do plugin falhou");
        }
        Ok(())
    }

    pub(crate) async fn check_update(&self, code: &str) -> Result<(), LicenseClientError> {
        let runtime = self.runtimes.get(code).ok_or_else(|| {
            LicenseClientError::new(LicenseErrorCode::ServerError, "plugin desconhecido")
        })?;
        runtime.detect_update().await
    }

    pub(crate) async fn apply_update(
        &self,
        code: &str,
        release_id: &str,
    ) -> Result<(), LicenseClientError> {
        let runtime = self.runtimes.get(code).ok_or_else(|| {
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

/// As features que UM `plugin_code` entrega, lidas do `feature_set` do
/// manifesto do release no formato `"<plugin_code>.<feature>"`, sem o prefixo.
/// Um release só declara as suas próprias features, mas o prefixo é filtrado
/// mesmo assim: é ele que garante que um manifesto trocado não empreste
/// feature de outro plugin.
fn features_for_code(code: &str, feature_set: &[String]) -> Vec<String> {
    feature_set
        .iter()
        .filter_map(|feature| feature.strip_prefix(code).and_then(|rest| rest.strip_prefix('.')))
        .map(str::to_owned)
        .collect()
}

fn update_unavailable(message: impl Into<String>) -> LicenseClientError {
    LicenseClientError::new(LicenseErrorCode::UpdateUnavailable, message)
}

// ---------------------------------------------------------------------------
// Rotas HTTP
// ---------------------------------------------------------------------------

pub(crate) async fn list_plugins(
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
    axum::Json(
        state
            .plugins
            .peer_proof(&challenge, &identity.signing_key),
    )
    .into_response()
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
    if manifest.product != "pom"
        || manifest.plugin_abi != enterprise_protocol::PLUGIN_ABI_VERSION
        || manifest.release_id != activation.plugin_release_id
        || manifest.os != std::env::consts::OS
        || manifest.arch != std::env::consts::ARCH
        || manifest.signature.trim().is_empty()
        || activation.plugin_code != code
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
        artifacts: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        manifest_calls: Arc<Mutex<Vec<String>>>,
        download_calls: Arc<Mutex<u32>>,
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
                artifacts: Arc::new(Mutex::new(HashMap::new())),
                manifest_calls: Arc::new(Mutex::new(Vec::new())),
                download_calls: Arc::new(Mutex::new(0)),
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

        pub(crate) async fn grant(&self, code: &str, response: ActivationResponse, artifact: Vec<u8>) {
            self.responses
                .lock()
                .await
                .insert(code.to_owned(), MockResponse::Valid(response));
            self.artifacts.lock().await.insert(code.to_owned(), artifact);
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

        pub(crate) async fn clear_heartbeat_error(&self) {
            *self.heartbeat_error.lock().await = None;
        }

        pub(crate) async fn manifest_calls(&self) -> Vec<String> {
            self.manifest_calls.lock().await.clone()
        }

        pub(crate) async fn download_calls(&self) -> u32 {
            *self.download_calls.lock().await
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
                    lease_expires_at: "2099-01-08T00:00:00Z".into(),
                    heartbeat_due_at: "2099-01-02T00:00:00Z".into(),
                    grace_seconds: 86400,
                    plugin_release_id: response.plugin_release_id.clone(),
                    release_changed: false,
                    available_release_id: None,
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
            self.manifest_calls.lock().await.push(plugin_code.to_owned());
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
            let code = grant
                .url
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned();
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
        let path = plugin_path("enterprise", "ent-0.1.0+build.1-linux-x86_64", "linux", "x86_64")
            .expect("release_id com '+' de metadado de build deve ser aceito");

        assert!(path
            .to_string_lossy()
            .ends_with("plugins/enterprise/ent-0.1.0+build.1-linux-x86_64-x86_64.so"));
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
        runtime.mark_plugin_ready(version).await.expect("mark ready");
    }

    #[tokio::test]
    async fn two_plugins_activate_and_deactivate_independently() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant("enterprise", activation("enterprise", "ent-1"), b"plugin-binary".to_vec())
            .await;
        service
            .grant("sample", activation("sample", "sample-1"), b"plugin-binary".to_vec())
            .await;
        let manager = PluginManager::test_manager(&["enterprise", "sample"], service.clone());
        let enterprise = manager.runtime_for("enterprise").expect("runtime");
        let sample = manager.runtime_for("sample").expect("runtime");
        activate_ready(&enterprise, "PM-enterprise", "0.1.0").await;
        activate_ready(&sample, "PM-sample", "0.1.0").await;

        assert!(manager.status("enterprise").await.expect("status").active);
        assert!(manager.status("sample").await.expect("status").active);

        manager.deactivate("enterprise").await.expect("deactivate enterprise");

        assert!(!manager.status("enterprise").await.expect("status").active);
        assert!(
            manager.status("sample").await.expect("status").active,
            "revogar um plugin não pode desativar o outro"
        );
    }

    #[tokio::test]
    async fn activation_requires_a_known_granted_plugin() {
        let service = Arc::new(MockLicenseService::new());
        service
            .grant("other-product", activation("other-product", "op-1"), Vec::new())
            .await;
        let manager = PluginManager::test_manager(&["enterprise"], service);

        let error = manager
            .activate("PM-unrelated")
            .await
            .expect_err("must reject a key granting no known plugin");

        assert_eq!(error.code(), LicenseErrorCode::FeatureNotEntitled);
    }

    #[tokio::test]
    async fn invalid_license_keeps_the_plugin_inactive_and_returns_a_stable_error() {
        let service = Arc::new(MockLicenseService::new());
        service.deny("enterprise", LicenseErrorCode::InvalidKey).await;
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
        let service = Arc::new(MockLicenseService::single("enterprise", activation("enterprise", "ent-1")));
        let manager = PluginManager::test_manager(&["enterprise"], service);
        let runtime = manager.runtime_for("enterprise").expect("runtime");
        activate_ready(&runtime, "PM-test-key", "0.1.0").await;

        runtime.heartbeat("heartbeat-nonce").await.expect("heartbeat");

        let persisted = runtime.persisted_state().await.expect("persisted state");
        assert_eq!(persisted.lease_token, "plt-renewed");
        assert_eq!(persisted.lease_expires_at, "2099-01-08T00:00:00Z");
        assert_eq!(persisted.grace_seconds, 86400);

        let status = manager.status("enterprise").await.expect("status");
        assert_eq!(status.state, LicenseState::Active);
        assert_eq!(
            status.lease_expires_at.as_deref(),
            Some("2099-01-08T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn transient_heartbeat_failure_enters_degraded_without_deactivating() {
        let service = Arc::new(MockLicenseService::single("enterprise", activation("enterprise", "ent-1")));
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
            .grant("enterprise", activation("enterprise", "ent-1"), b"plugin-binary".to_vec())
            .await;
        service
            .grant("sample", activation("sample", "sample-1"), b"plugin-binary".to_vec())
            .await;
        let manager = PluginManager::test_manager(&["enterprise", "sample"], service.clone());
        let enterprise = manager.runtime_for("enterprise").expect("runtime");
        let sample = manager.runtime_for("sample").expect("runtime");
        activate_ready(&enterprise, "PM-enterprise", "0.1.0").await;
        activate_ready(&sample, "PM-sample", "0.1.0").await;
        service.set_heartbeat_error(LicenseErrorCode::Revoked).await;

        let runtime = manager.runtime_for("enterprise").expect("runtime");
        let error = runtime
            .heartbeat("nonce-1")
            .await
            .expect_err("revoked");

        assert_eq!(error.code(), LicenseErrorCode::Revoked);
        assert!(!manager.status("enterprise").await.expect("status").active);
        assert!(
            manager.status("sample").await.expect("status").active,
            "o heartbeat de um plugin não deve afetar o outro"
        );
    }

    #[tokio::test]
    async fn persisted_state_never_contains_the_license_key() {
        let service = Arc::new(MockLicenseService::single("enterprise", activation("enterprise", "ent-1")));
        let manager = PluginManager::test_manager(&["enterprise"], service);
        let runtime = manager.runtime_for("enterprise").expect("runtime");
        activate_ready(&runtime, "PM-secret-license", "0.1.0").await;

        let persisted =
            serde_json::to_vec(&runtime.persisted_state().await.expect("persisted")).expect("state JSON");

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

        updated.plugin_release_id = "ent-2".into();
        updated.plugin_manifest.release_id = "ent-2".into();
        updated.plugin_manifest.version = "0.2.0".into();
        service.grant("enterprise", updated, b"plugin-binary".to_vec()).await;

        runtime.detect_update().await.expect("detect update");
        let available = runtime
            .status()
            .await
            .available_update
            .expect("update should be advertised");
        assert_eq!(available.release_id, "ent-2");
        assert_eq!(available.version, "0.2.0");
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
        service.grant("enterprise", updated, b"plugin-binary".to_vec()).await;
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
    #[tokio::test]
    async fn heartbeat_success_restarts_the_stopped_plugin_host() {
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
        let service = Arc::new(MockLicenseService::single("enterprise", activation("enterprise", "ent-1")));
        // `test_manager_with_database` não dispara o `restore()` de boot (ver
        // `from_catalog`): aqui o estado é montado à mão logo abaixo, e uma
        // restauração concorrente o sobrescreveria a partir do banco em um
        // instante fora do controle do teste.
        let manager = PluginManager::test_manager_with_database(&["enterprise"], service.clone(), db);
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
