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

pub struct ConnectionAuth;

#[derive(Debug)]
pub enum AuthRejection {
    MissingHeader,
    InvalidKey,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            AuthRejection::MissingHeader => (StatusCode::UNAUTHORIZED, "missing Authorization header"),
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

pub struct ApiKeyAuth;

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

        let header = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthRejection::MissingHeader)?;

        let token = header.strip_prefix("Bearer ").unwrap_or(header);

        let cfg = config.read().await;
        // The connection key is this cluster's operator credential; it also
        // opens the OpenAI routes, so the built-in test panel and any operator
        // can call them without first minting an API key.
        let is_operator = !cfg.connection_key.is_empty() && token == cfg.connection_key;
        if is_operator || cfg.api_keys.iter().any(|k| k.active && k.key == token) {
            Ok(ApiKeyAuth)
        } else {
            Err(AuthRejection::InvalidKey)
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
}
