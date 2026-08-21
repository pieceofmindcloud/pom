use crate::config::Config;
use async_trait::async_trait;
use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Json},
};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type AuthState = Arc<RwLock<Config>>;

/// Escolhe a credencial que um nó usa nas chamadas internas de inferência.
///
/// A `connection_key` continua sendo a credencial canônica e compartilhada do
/// cluster. Quando ela não está disponível, uma API key ativa é a única outra
/// credencial que a porta de inferência aceita, então ela pode ser usada para
/// manter os peers autenticados em instalações configuradas dessa forma.
/// Nunca retorna uma chave revogada nem uma string vazia.
pub fn inference_token(config: &Config) -> Option<String> {
    if !config.connection_key.is_empty() {
        return Some(config.connection_key.clone());
    }
    config
        .api_keys
        .iter()
        .find(|key| key.active && !key.key.is_empty())
        .map(|key| key.key.clone())
}

/// Valida o token que pode atravessar a porta de inferência.
///
/// A chave de conexão é compartilhada por todos os nós do cluster. As API
/// keys são a alternativa para clientes que já chegam autenticados pela
/// superfície OpenAI/Anthropic. Não aceitar string vazia evita que um nó em
/// modo setup trate uma requisição sem credencial como autenticada.
pub fn is_valid_inference_token(config: &Config, token: &str) -> bool {
    (!config.connection_key.is_empty() && token == config.connection_key)
        || config
            .api_keys
            .iter()
            .any(|key| key.active && token == key.key)
}

pub struct ConnectionAuth;

#[derive(Debug)]
pub enum AuthRejection {
    MissingHeader,
    InvalidKey,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            AuthRejection::MissingHeader => {
                (StatusCode::UNAUTHORIZED, "missing Authorization header")
            }
            AuthRejection::InvalidKey => (StatusCode::FORBIDDEN, "invalid connection key"),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for ConnectionAuth
where
    S: Send + Sync,
    AuthState: axum::extract::FromRef<S>,
{
    type Rejection = AuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let State(config): State<AuthState> = State::from_request_parts(parts, state)
            .await
            .map_err(|_| AuthRejection::MissingHeader)?;

        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthRejection::MissingHeader)?;

        let token = header.strip_prefix("Bearer ").unwrap_or(header);

        let cfg = config.read().await;
        if token == cfg.connection_key {
            Ok(ConnectionAuth)
        } else {
            Err(AuthRejection::InvalidKey)
        }
    }
}

/// A caller that presented a credential good for the `/v1` routes.
///
/// It carries *who* it was, because the request log has no other chance to find
/// out: by the time a generation finishes, the header is long gone.
pub struct ApiKeyAuth(pub crate::reqlog::Caller);

impl ApiKeyAuth {
    pub fn into_caller(self) -> crate::reqlog::Caller {
        self.0
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for ApiKeyAuth
where
    S: Send + Sync,
    AuthState: axum::extract::FromRef<S>,
{
    type Rejection = AuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let State(config): State<AuthState> = State::from_request_parts(parts, state)
            .await
            .map_err(|_| AuthRejection::MissingHeader)?;

        // `Authorization: Bearer <key>` (OpenAI/`ANTHROPIC_AUTH_TOKEN` style) or
        // `x-api-key: <key>` (Anthropic Messages API style, what `ANTHROPIC_API_KEY`
        // sends) — a client picks whichever env var it has, and both name the
        // same credential, so either header is accepted.
        let token = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|h| h.strip_prefix("Bearer ").unwrap_or(h))
            .or_else(|| parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()))
            .ok_or(AuthRejection::MissingHeader)?;

        let cfg = config.read().await;
        // The connection key is this cluster's operator credential; it also
        // opens the OpenAI/Anthropic routes, so the built-in test panel and
        // any operator can call them without first minting an API key.
        let is_operator = !cfg.connection_key.is_empty() && token == cfg.connection_key;
        if is_operator {
            return Ok(ApiKeyAuth(crate::reqlog::Caller::operator()));
        }
        match cfg.api_keys.iter().find(|k| k.active && k.key == token) {
            Some(key) => Ok(ApiKeyAuth(crate::reqlog::Caller {
                token_label: key.label.clone(),
                token_hint: crate::reqlog::Caller::hint_of(&key.key),
                token_node_id: (!key.node_id.is_empty()).then(|| key.node_id.clone()),
            })),
            None => Err(AuthRejection::InvalidKey),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_rejection_status() {
        let resp = AuthRejection::MissingHeader.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = AuthRejection::InvalidKey.into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn inference_accepts_connection_key_or_active_api_key_only() {
        let cfg = crate::config::Config {
            gpus: None,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            role: crate::config::Role::Principal,
            connection_key: "cluster-secret".into(),
            gateway_port: 8080,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![
                crate::config::ApiKey {
                    key: "sk-active".into(),
                    label: "test".into(),
                    node_id: String::new(),
                    active: true,
                    created_at: String::new(),
                },
                crate::config::ApiKey {
                    key: "sk-revoked".into(),
                    label: "revoked".into(),
                    node_id: String::new(),
                    active: false,
                    created_at: String::new(),
                },
            ],
            blocked_nodes: vec![],
            tls_required: false,
            model_dirs: None,
            shard_dir: None,
            shard_max_gb: 1.0,
            node_name: None,
        };

        assert!(is_valid_inference_token(&cfg, "cluster-secret"));
        assert!(is_valid_inference_token(&cfg, "sk-active"));
        assert!(!is_valid_inference_token(&cfg, "sk-revoked"));
        assert!(!is_valid_inference_token(&cfg, "wrong"));
    }

    #[test]
    fn internal_inference_token_prefers_connection_key_and_falls_back_to_active_api_key() {
        let mut cfg = crate::config::Config {
            gpus: None,
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            role: crate::config::Role::Principal,
            connection_key: "cluster-secret".into(),
            gateway_port: 8080,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![crate::config::ApiKey {
                key: "sk-active".into(),
                label: "test".into(),
                node_id: String::new(),
                active: true,
                created_at: String::new(),
            }],
            blocked_nodes: vec![],
            tls_required: false,
            model_dirs: None,
            shard_dir: None,
            shard_max_gb: 1.0,
            node_name: None,
        };

        assert_eq!(inference_token(&cfg).as_deref(), Some("cluster-secret"));

        cfg.connection_key.clear();
        assert_eq!(inference_token(&cfg).as_deref(), Some("sk-active"));
    }
}
