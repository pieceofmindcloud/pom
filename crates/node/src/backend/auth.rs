use crate::config::Config;
use async_trait::async_trait;
use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::RwLock;

pub type AuthState = Arc<RwLock<Config>>;

/// Cabeçalho da credencial da pessoa que abre a superfície administrativa.
/// Deliberadamente diferente de `Authorization`, que continua sendo usado
/// pela connection key e pelas APIs compatíveis com OpenAI/Anthropic.
pub const ADMIN_ACCESS_TOKEN_HEADER: &str = "x-pom-access-token";
/// Marca requests de controle emitidos pelo próprio nó para outro nó. Ela não
/// é uma credencial: só vale junto com uma `connection_key` válida e nunca é
/// enviada pelo cliente da interface.
pub const INTERNAL_NODE_REQUEST_HEADER: &str = "x-pom-internal-node";

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

/// Return the non-secret identity stored with an API key. Configurations
/// written before the identity field existed get a stable fallback derived only
/// from non-secret metadata; the secret itself never leaves this function.
pub fn api_key_identity(key: &crate::config::ApiKey) -> String {
    if !key.id.is_empty() {
        return key.id.clone();
    }
    let metadata = format!("{}|{}|{}", key.label, key.node_id, key.created_at);
    let digest = Sha256::digest(metadata.as_bytes());
    format!("legacy-{}", &hex::encode(digest)[..12])
}

pub struct ConnectionAuth;

#[derive(Debug)]
pub enum AuthRejection {
    MissingHeader,
    InvalidKey,
}

/// Normalize one Authorization value without logging or copying its secret.
/// Non-Bearer values remain candidates for compatibility with the historical
/// connection-key route, while Bearer matching is case-insensitive and trims
/// surrounding whitespace as HTTP clients commonly do.
fn authorization_token(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let mut words = value.splitn(2, |character: char| character.is_ascii_whitespace());
    let scheme = words.next().unwrap_or_default();
    if scheme.eq_ignore_ascii_case("bearer") {
        let token = words.next().unwrap_or_default().trim();
        (!token.is_empty()).then_some(token)
    } else {
        Some(value)
    }
}

/// Return every credential supplied by the caller. Claude-compatible clients
/// may send both `Authorization: Bearer ...` and `x-api-key: ...`; one bad
/// header must not hide the other valid credential.
fn credential_candidates(headers: &HeaderMap) -> Vec<&str> {
    let mut candidates = Vec::with_capacity(2);
    if let Some(candidate) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(authorization_token)
    {
        candidates.push(candidate);
    }
    if let Some(candidate) = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
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

pub struct AdminAuth;

#[derive(Debug, PartialEq, Eq)]
pub enum AdminAuthRejection {
    MissingPeerAddress,
    MissingToken,
    InvalidToken,
    LocalhostOnly,
}

impl IntoResponse for AdminAuthRejection {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            AdminAuthRejection::MissingPeerAddress => (
                StatusCode::FORBIDDEN,
                "administrative request has no peer address",
            ),
            AdminAuthRejection::MissingToken => (
                StatusCode::UNAUTHORIZED,
                "missing administrative access token",
            ),
            AdminAuthRejection::InvalidToken => {
                (StatusCode::FORBIDDEN, "invalid administrative access token")
            }
            AdminAuthRejection::LocalhostOnly => (
                StatusCode::FORBIDDEN,
                "administrative interface is local-only",
            ),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

/// Compare fixed-size digests so neither token length nor the first differing
/// byte controls the comparison time. Empty values are rejected separately,
/// including the empty configured token.
pub(crate) fn constant_time_token_eq(expected: &str, supplied: &str) -> bool {
    if expected.is_empty() || supplied.is_empty() {
        return false;
    }
    let expected_digest = Sha256::digest(expected.as_bytes());
    let supplied_digest = Sha256::digest(supplied.as_bytes());
    expected_digest.ct_eq(&supplied_digest).into()
}

fn is_loopback(peer: std::net::SocketAddr) -> bool {
    match peer.ip() {
        std::net::IpAddr::V4(ip) => ip.is_loopback(),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip
                    .to_ipv4_mapped()
                    .map(|mapped| mapped.is_loopback())
                    .unwrap_or(false)
        }
    }
}

fn is_authenticated_internal_node_request(headers: &HeaderMap, connection_key: &str) -> bool {
    if connection_key.is_empty()
        || headers
            .get(INTERNAL_NODE_REQUEST_HEADER)
            .and_then(|value| value.to_str().ok())
            != Some("1")
    {
        return false;
    }
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(authorization_token)
        .is_some_and(|supplied| constant_time_token_eq(connection_key, supplied))
}

#[async_trait]
impl<S> FromRequestParts<S> for AdminAuth
where
    S: Send + Sync,
    AuthState: axum::extract::FromRef<S>,
{
    type Rejection = AdminAuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<crate::serve::PeerAddr>()
            .copied()
            .ok_or(AdminAuthRejection::MissingPeerAddress)?;
        let State(config): State<AuthState> = State::from_request_parts(parts, state)
            .await
            .map_err(|_| AdminAuthRejection::InvalidToken)?;
        let cfg = config.read().await;

        if !cfg.admin_exposure.requires_token() {
            return if is_loopback(peer.0) {
                Ok(AdminAuth)
            } else {
                Err(AdminAuthRejection::LocalhostOnly)
            };
        }

        // Node-to-node control calls still use the shared connection key. The
        // marker keeps that protocol distinct from a browser request: a
        // browser without the human access token never reaches an admin route,
        // while cluster surveys and deployment fan-out keep working.
        if parts.headers.get(ADMIN_ACCESS_TOKEN_HEADER).is_none()
            && parts.uri.path() != "/api/ui/access"
            && is_authenticated_internal_node_request(&parts.headers, &cfg.connection_key)
        {
            return Ok(AdminAuth);
        }

        let supplied = parts
            .headers
            .get(ADMIN_ACCESS_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .ok_or(AdminAuthRejection::MissingToken)?;
        if constant_time_token_eq(
            cfg.admin_access_token.as_deref().unwrap_or_default(),
            supplied,
        ) {
            Ok(AdminAuth)
        } else {
            Err(AdminAuthRejection::InvalidToken)
        }
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

        let token = authorization_token(header).ok_or(AuthRejection::MissingHeader)?;

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
    Arc<crate::db::PomDb>: axum::extract::FromRef<S>,
{
    type Rejection = AuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let State(config): State<AuthState> = State::from_request_parts(parts, state)
            .await
            .map_err(|_| AuthRejection::MissingHeader)?;
        let State(database): State<Arc<crate::db::PomDb>> = State::from_request_parts(parts, state)
            .await
            .map_err(|_| AuthRejection::MissingHeader)?;

        // `Authorization: Bearer <key>` (OpenAI/`ANTHROPIC_AUTH_TOKEN` style)
        // or `x-api-key: <key>` (Anthropic Messages API style, what
        // `ANTHROPIC_API_KEY` sends). Consider both independently because a
        // client may send both headers and one can be stale or invalid.
        let candidates = credential_candidates(&parts.headers);
        if candidates.is_empty() {
            return Err(AuthRejection::MissingHeader);
        }

        let cfg = config.read().await;
        for token in candidates {
            // The connection key is this cluster's operator credential; it
            // also opens the OpenAI/Anthropic routes, so the built-in test
            // panel and any operator can call them without first minting an
            // API key.
            if !cfg.connection_key.is_empty() && token == cfg.connection_key {
                return Ok(ApiKeyAuth(crate::reqlog::Caller::operator()));
            }
            // SQLite is authoritative here rather than the startup snapshot:
            // a key minted by another process becomes usable immediately, and
            // a revocation made there takes effect without a restart.
            if let Some(key) = database.api_key_for_token(token).ok().flatten() {
                return Ok(ApiKeyAuth(crate::reqlog::Caller {
                    key_id: api_key_identity(&key),
                    key_label: key.label,
                    key_node_id: (!key.node_id.is_empty()).then_some(key.node_id),
                }));
            }
        }
        Err(AuthRejection::InvalidKey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    async fn admin_auth_with(
        exposure: crate::config::AdminExposure,
        configured_token: Option<&str>,
        peer: std::net::SocketAddr,
        supplied_token: Option<&str>,
    ) -> Result<AdminAuth, AdminAuthRejection> {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = exposure;
            cfg.admin_access_token = configured_token.map(str::to_string);
        }
        let mut request = Request::builder().body(()).expect("requisição");
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr(peer));
        if let Some(token) = supplied_token {
            request = Request::builder()
                .header("x-pom-access-token", token)
                .body(())
                .expect("requisição com token");
            request
                .extensions_mut()
                .insert(crate::serve::PeerAddr(peer));
        }
        let (mut parts, _) = request.into_parts();
        <AdminAuth as FromRequestParts<crate::gateway::GatewayState>>::from_request_parts(
            &mut parts, &state,
        )
        .await
    }

    #[test]
    fn test_auth_rejection_status() {
        let resp = AuthRejection::MissingHeader.into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = AuthRejection::InvalidKey.into_response();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn admin_token_comparison_rejects_empty_values_and_compares_digest() {
        assert!(!constant_time_token_eq("", ""));
        assert!(!constant_time_token_eq("secret", ""));
        assert!(!constant_time_token_eq("", "secret"));
        assert!(constant_time_token_eq("secret", "secret"));
        assert!(!constant_time_token_eq("secret", "secret-typo"));
    }

    #[tokio::test]
    async fn local_admin_accepts_loopback_without_a_token_but_not_remote_peers() {
        assert!(admin_auth_with(
            crate::config::AdminExposure::Localhost,
            None,
            "127.0.0.1:4321".parse().unwrap(),
            None,
        )
        .await
        .is_ok());
        assert!(admin_auth_with(
            crate::config::AdminExposure::Localhost,
            None,
            "192.0.2.7:4321".parse().unwrap(),
            None,
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn exposed_admin_requires_a_non_empty_matching_token_even_on_loopback() {
        let peer = "127.0.0.1:4321".parse().unwrap();
        assert!(admin_auth_with(
            crate::config::AdminExposure::Network,
            Some("admin-secret"),
            peer,
            None,
        )
        .await
        .is_err());
        assert!(admin_auth_with(
            crate::config::AdminExposure::Network,
            Some("admin-secret"),
            peer,
            Some("wrong"),
        )
        .await
        .is_err());
        assert!(admin_auth_with(
            crate::config::AdminExposure::Network,
            Some("admin-secret"),
            peer,
            Some("admin-secret"),
        )
        .await
        .is_ok());
        assert!(admin_auth_with(
            crate::config::AdminExposure::Network,
            Some(""),
            peer,
            Some(""),
        )
        .await
        .is_err());
        assert!(admin_auth_with(
            crate::config::AdminExposure::Network,
            Some(""),
            peer,
            Some("anything"),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn internal_node_control_uses_connection_key_but_not_the_browser_token() {
        let state = crate::gateway::testkit::state_with("cluster-secret", vec![]).await;
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = crate::config::AdminExposure::Network;
            cfg.admin_access_token = Some("person-secret".into());
        }
        let mut request = Request::builder()
            .header(INTERNAL_NODE_REQUEST_HEADER, "1")
            .header("authorization", "Bearer cluster-secret")
            .body(())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::serve::PeerAddr("192.0.2.7:4321".parse().unwrap()));
        let (mut parts, _) = request.into_parts();
        assert!(
            <AdminAuth as FromRequestParts<crate::gateway::GatewayState>>::from_request_parts(
                &mut parts, &state,
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn changing_mode_or_regenerating_invalidates_the_previous_token() {
        let state = crate::gateway::testkit::state_with("cluster-secret", vec![]).await;
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = crate::config::AdminExposure::Network;
            cfg.admin_access_token = Some("old-token".into());
        }

        async fn check(
            state: &crate::gateway::GatewayState,
            supplied: Option<&str>,
        ) -> Result<AdminAuth, AdminAuthRejection> {
            let mut request = Request::builder().body(()).unwrap();
            request
                .extensions_mut()
                .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
            if let Some(token) = supplied {
                request = Request::builder()
                    .header(ADMIN_ACCESS_TOKEN_HEADER, token)
                    .body(())
                    .unwrap();
                request
                    .extensions_mut()
                    .insert(crate::serve::PeerAddr("127.0.0.1:4321".parse().unwrap()));
            }
            let (mut parts, _) = request.into_parts();
            <AdminAuth as FromRequestParts<crate::gateway::GatewayState>>::from_request_parts(
                &mut parts, state,
            )
            .await
        }

        assert!(check(&state, Some("old-token")).await.is_ok());
        state.auth.write().await.admin_access_token = Some("new-token".into());
        assert!(check(&state, Some("old-token")).await.is_err());
        assert!(check(&state, Some("new-token")).await.is_ok());
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = crate::config::AdminExposure::Localhost;
            cfg.admin_access_token = None;
        }
        assert!(check(&state, None).await.is_ok());
        {
            let mut cfg = state.auth.write().await;
            cfg.admin_exposure = crate::config::AdminExposure::Network;
            cfg.admin_access_token = Some("third-token".into());
        }
        assert!(check(&state, Some("new-token")).await.is_err());
        assert!(check(&state, Some("third-token")).await.is_ok());
    }

    #[test]
    fn api_key_identity_is_stable_and_never_the_secret() {
        let key = crate::config::ApiKey {
            id: "finance-id".into(),
            key: "sk-secret".into(),
            label: "financeiro".into(),
            node_id: "node-a".into(),
            active: true,
            created_at: String::new(),
        };
        assert_eq!(api_key_identity(&key), "finance-id");
        assert_ne!(api_key_identity(&key), key.key);

        let legacy = crate::config::ApiKey {
            id: String::new(),
            ..key
        };
        let legacy_id = api_key_identity(&legacy);
        assert!(legacy_id.starts_with("legacy-"));
        assert!(!legacy_id.contains("sk-secret"));
        let mut other_legacy = legacy.clone();
        other_legacy.key = "sk-other".into();
        assert_eq!(legacy_id, api_key_identity(&other_legacy));
    }

    #[test]
    fn inference_accepts_connection_key_or_active_api_key_only() {
        let cfg = crate::config::Config {
            gpus: None,
            thermal: Default::default(),
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            role: crate::config::Role::Principal,
            connection_key: "cluster-secret".into(),
            gateway_port: 8080,
            admin_exposure: Default::default(),
            admin_access_token: None,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![
                crate::config::ApiKey {
                    id: "active-id".into(),
                    key: "sk-active".into(),
                    label: "test".into(),
                    node_id: String::new(),
                    active: true,
                    created_at: String::new(),
                },
                crate::config::ApiKey {
                    id: "revoked-id".into(),
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
            huggingface_token: None,
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
            thermal: Default::default(),
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            role: crate::config::Role::Principal,
            connection_key: "cluster-secret".into(),
            gateway_port: 8080,
            admin_exposure: Default::default(),
            admin_access_token: None,
            p2p_port: 42777,
            inference_port: Some(8181),
            bootstrap_nodes: vec![],
            advertise_addr: None,
            api_keys: vec![crate::config::ApiKey {
                id: "active-id".into(),
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
            huggingface_token: None,
        };

        assert_eq!(inference_token(&cfg).as_deref(), Some("cluster-secret"));

        cfg.connection_key.clear();
        assert_eq!(inference_token(&cfg).as_deref(), Some("sk-active"));
    }

    #[tokio::test]
    async fn valid_secondary_header_preserves_api_key_caller_identity() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        let key = crate::config::ApiKey {
            id: "claude-code-id".into(),
            key: "api-secret".into(),
            label: "claude-code".into(),
            node_id: "eu".into(),
            active: true,
            created_at: String::new(),
        };
        state.database.insert_api_key(&key).unwrap();
        state.auth.write().await.api_keys.push(key);

        let request = Request::builder()
            .header("authorization", "Bearer invalid")
            .header("x-api-key", "api-secret")
            .body(())
            .expect("requisição");
        let (mut parts, _) = request.into_parts();
        let auth =
            <ApiKeyAuth as FromRequestParts<crate::gateway::GatewayState>>::from_request_parts(
                &mut parts, &state,
            )
            .await
            .expect("a credencial secundária válida deve autenticar");

        let caller = auth.into_caller();
        assert_eq!(caller.key_id, "claude-code-id");
        assert_eq!(caller.key_label, "claude-code");
        assert_eq!(caller.key_node_id.as_deref(), Some("eu"));

        state.database.revoke_api_key("api-secret", "eu").unwrap();
        let request = Request::builder()
            .header("authorization", "Bearer api-secret")
            .body(())
            .expect("requisição");
        let (mut parts, _) = request.into_parts();
        assert!(matches!(
            <ApiKeyAuth as FromRequestParts<crate::gateway::GatewayState>>::from_request_parts(
                &mut parts, &state,
            )
            .await,
            Err(AuthRejection::InvalidKey)
        ));
    }
}
