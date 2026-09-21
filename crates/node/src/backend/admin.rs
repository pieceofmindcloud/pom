use crate::admin_trust::{
    AdminTransferAcceptance, AdminTransferOffer, AdminTrustDocument, AuthorityPrincipal,
    EffectiveAuthority, NodePromotionCommand, NodePromotionRequest,
};
use crate::auth::{AuthState, ConnectionAuth, InternalNodeAuth};
use crate::config::ApiKey;
use crate::profile::{NodeProfile, ProfileAction};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;

pub fn admin_routes() -> Router<crate::gateway::GatewayState> {
    Router::new()
        .route("/admin/nodes", get(list_nodes))
        .route("/admin/nodes/:id/block", post(block_node))
        .route("/admin/nodes/:id/unblock", post(unblock_node))
        .route(
            "/admin/nodes/:id/promote-admin",
            post(promote_node_to_admin),
        )
        .route("/admin/authority/resign", post(resign_authority))
        .route(
            "/admin/authority/secondaries/:id/demote",
            post(demote_secondary),
        )
        .route("/api/ui/network/admin-authority", get(admin_authority_view))
        .route("/api/ui/network/request-node", post(request_node_promotion))
        .route("/api/ui/network/relinquish-node", post(relinquish_node))
        .route("/admin/nodes/:id", delete(remove_node))
        .route("/admin/regenerate-key", post(regenerate_key))
        .route("/admin/api-keys", get(list_api_keys).post(generate_api_key))
        .route("/admin/api-keys/:key", delete(revoke_api_key))
        .route("/admin/transfer-offers", get(transfer_offers))
        .route("/admin/slots", get(list_slots))
        .route("/admin/slots/:name", post(slot_action))
        .merge(internal_routes())
}

#[cfg(test)]
const SYNC_TIMEOUT: Duration = Duration::from_millis(50);
#[cfg(not(test))]
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const PROMOTION_REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(not(test))]
const PROMOTION_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SYNC_ATTEMPTS: usize = 3;

#[derive(Debug, Serialize, Deserialize)]
struct RevokeRequest {
    key_or_id: String,
    revoked_by: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RevokeOriginRequest {
    origin_node_id: String,
    revoked_by: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ValidateRequest {
    token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ValidateResponse {
    valid: bool,
    #[serde(default)]
    key: Option<ApiKey>,
}

async fn refresh_api_key_snapshot(state: &crate::gateway::GatewayState) -> anyhow::Result<()> {
    let keys = state.database.api_keys()?;
    state.auth.write().await.api_keys = keys;
    Ok(())
}

async fn principal_peer_for(
    network: &crate::network::Network,
    primary: &AuthorityPrincipal,
) -> Option<crate::network::PeerInfo> {
    network.alive_peers().await.into_iter().find(|peer| {
        peer.node_id != network.node_id
            && peer.node_id == primary.node_id
            && peer.libp2p_peer_id.as_deref() == Some(primary.peer_id.as_str())
            && peer.authority_pubkey == primary.pubkey
            && peer.gateway_port.is_some()
    })
}

fn gateway_urls(peer: &crate::network::PeerInfo) -> Vec<String> {
    let mut urls = peer.gateway_candidates();
    if urls.is_empty() {
        if let Some(url) = peer.gateway_url() {
            urls.push(url);
        }
    }
    urls
}

/// Envia uma chamada de controle para cada endereço anunciado pelo peer até
/// que um deles responda. Um endereço que deixou de existir não pode impedir
/// a promoção quando o mesmo anúncio ainda traz um candidato vivo.
async fn post_internal_json<T: Serialize>(
    state: &crate::gateway::GatewayState,
    peer: &crate::network::PeerInfo,
    path: &str,
    body: &T,
    timeout: Duration,
) -> anyhow::Result<reqwest::Response> {
    let urls = gateway_urls(peer);
    if urls.is_empty() {
        anyhow::bail!("o peer não anuncia porta de gateway");
    }
    let client = state.peer_http.for_peer(&peer.node_id)?;
    let body_bytes = serde_json::to_vec(body)?;
    let control_headers = state.signed_control_headers("POST", path, &body_bytes)?;
    let connection_key = {
        let config = state.auth.read().await;
        crate::auth::internal_node_token(&config)
    };

    let mut last_error = "nenhum endereço de gateway respondeu".to_string();
    for base in &urls {
        let mut request = client
            .post(format!("{base}{path}"))
            .timeout(timeout)
            .headers(control_headers.clone());
        if let Some(connection_key) = &connection_key {
            request = request.bearer_auth(connection_key);
        }
        match request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body_bytes.clone())
            .send()
            .await
        {
            Ok(response) => return Ok(response),
            Err(error) => {
                last_error = format!("{base}: {error}");
            }
        }
    }
    anyhow::bail!(last_error)
}

async fn sync_json<T: Serialize>(
    state: &crate::gateway::GatewayState,
    path: &str,
    body: &T,
) -> anyhow::Result<()> {
    let primary = state.admin_trust.current_primary();
    let peer = principal_peer_for(&state.network, &primary)
        .await
        .ok_or_else(|| anyhow::anyhow!("central admin node is unavailable"))?;
    let urls = gateway_urls(&peer);
    let client = state.peer_http.for_peer(&peer.node_id)?;
    let body_bytes = serde_json::to_vec(body)?;
    let control_headers = state.signed_control_headers("POST", path, &body_bytes)?;
    let connection_key = {
        let config = state.auth.read().await;
        crate::auth::internal_node_token(&config)
    };
    let mut last_error = "central admin synchronization failed".to_string();
    for attempt in 0..SYNC_ATTEMPTS {
        for base in &urls {
            let mut request = client
                .post(format!("{base}{path}"))
                .timeout(SYNC_TIMEOUT)
                .headers(control_headers.clone());
            if let Some(connection_key) = &connection_key {
                request = request.bearer_auth(connection_key);
            }
            let result = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body_bytes.clone())
                .send()
                .await;
            match result {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(_) => last_error = "central admin rejected synchronization".into(),
                Err(_) => last_error = "central admin synchronization timed out".into(),
            }
        }
        if attempt + 1 < SYNC_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
        }
    }
    anyhow::bail!(last_error)
}

async fn notify_key_created(
    state: &crate::gateway::GatewayState,
    key: &ApiKey,
) -> anyhow::Result<()> {
    if require_principal(state).await.is_ok() {
        return Ok(());
    }
    sync_json(state, "/api/internal/api-keys/upsert", key).await
}

async fn notify_key_revoked(
    state: &crate::gateway::GatewayState,
    key_or_id: &str,
    revoked_by: &str,
) -> anyhow::Result<()> {
    if require_principal(state).await.is_ok() {
        return Ok(());
    }
    sync_json(
        state,
        "/api/internal/api-keys/revoke",
        &RevokeRequest {
            key_or_id: key_or_id.to_string(),
            revoked_by: revoked_by.to_string(),
        },
    )
    .await
}

async fn notify_origin_revoked(
    state: &crate::gateway::GatewayState,
    origin_node_id: &str,
    revoked_by: &str,
) -> anyhow::Result<()> {
    if require_principal(state).await.is_ok() {
        return Ok(());
    }
    sync_json(
        state,
        "/api/internal/api-keys/revoke-origin",
        &RevokeOriginRequest {
            origin_node_id: origin_node_id.to_string(),
            revoked_by: revoked_by.to_string(),
        },
    )
    .await
}

async fn create_key(
    state: &crate::gateway::GatewayState,
    label: String,
    node_id: String,
) -> anyhow::Result<ApiKey> {
    let key = ApiKey {
        id: Uuid::new_v4().simple().to_string(),
        key: format!("sk-{}", Uuid::new_v4()),
        label,
        node_id,
        origin_node_id: state.network.node_id.clone(),
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    notify_key_created(state, &key).await?;
    state.database.insert_api_key(&key)?;
    refresh_api_key_snapshot(state).await?;
    Ok(key)
}

pub(crate) async fn validate_api_key_remote(
    state: &crate::gateway::GatewayState,
    token: &str,
) -> anyhow::Result<Option<ApiKey>> {
    validate_api_key_remote_parts(
        &state.network,
        &state.peer_http,
        &state.auth,
        token,
        Some(state.signed_control_headers(
            "POST",
            "/api/internal/api-keys/validate",
            &serde_json::to_vec(&ValidateRequest {
                token: token.to_string(),
            })?,
        )?),
    )
    .await
}

pub(crate) async fn validate_api_key_remote_parts(
    network: &crate::network::Network,
    peer_http: &crate::peer_client::PeerClients,
    auth: &AuthState,
    token: &str,
    control_headers: Option<reqwest::header::HeaderMap>,
) -> anyhow::Result<Option<ApiKey>> {
    let peer = network
        .alive_peers()
        .await
        .into_iter()
        .find(|peer| {
            peer.node_id != network.node_id
                && peer.profile == NodeProfile::Admin
                && peer.gateway_port.is_some()
        })
        .ok_or_else(|| anyhow::anyhow!("central admin node is unavailable"))?;
    let client = peer_http.for_peer(&peer.node_id)?;
    let body = ValidateRequest {
        token: token.to_string(),
    };
    let body_bytes = serde_json::to_vec(&body)?;
    let control_headers = control_headers;
    let connection_key = {
        let config = auth.read().await;
        crate::auth::internal_node_token(&config)
    };
    for attempt in 0..SYNC_ATTEMPTS {
        for base in gateway_urls(&peer) {
            let mut request = client
                .post(format!("{base}/api/internal/api-keys/validate"))
                .timeout(SYNC_TIMEOUT)
                .headers(control_headers.clone().unwrap_or_default());
            if let Some(connection_key) = &connection_key {
                request = request.bearer_auth(connection_key);
            }
            let result = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body_bytes.clone())
                .send()
                .await;
            if let Ok(response) = result {
                if let Ok(payload) = response.json::<ValidateResponse>().await {
                    return Ok(payload.valid.then_some(payload.key).flatten());
                }
            }
        }
        if attempt + 1 < SYNC_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
        }
    }
    anyhow::bail!("central admin validation failed")
}

fn internal_routes() -> Router<crate::gateway::GatewayState> {
    Router::new()
        .route(
            "/api/internal/api-keys/upsert",
            post(internal_upsert_api_key),
        )
        .route(
            "/api/internal/api-keys/revoke",
            post(internal_revoke_api_key),
        )
        .route(
            "/api/internal/api-keys/revoke-origin",
            post(internal_revoke_origin),
        )
        .route(
            "/api/internal/api-keys/validate",
            post(internal_validate_api_key),
        )
        .route(
            "/api/internal/admin-trust/reconcile",
            post(internal_reconcile_admin_trust),
        )
        .route(
            "/api/internal/admin-trust/complete-transfer",
            post(internal_complete_transfer),
        )
        .route(
            "/api/internal/profile/authorize-node",
            post(internal_authorize_node),
        )
        .route(
            "/api/internal/profile/accept-node",
            post(internal_accept_node_promotion),
        )
        .route("/api/internal/reload_model", post(internal_reload_model))
        .route(
            "/api/internal/enterprise/fingerprint",
            get(crate::plugins::plugin_peer_fingerprint),
        )
        .route(
            "/api/internal/network/settings",
            post(crate::webui::internal_network_settings),
        )
}

/// Named node-to-node event used to wake all idle peers in one planned
/// pipeline. The sender dispatches these requests concurrently and waits for
/// each local health watcher, so no hop reloads sequentially through the
/// inference chain.
async fn internal_reload_model(
    _auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(event): Json<serde_json::Value>,
) -> impl IntoResponse {
    if event.get("event").and_then(|value| value.as_str()) != Some("reload_model") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "evento desconhecido" })),
        );
    }
    match state.runtime.reload_from_idle().await {
        Ok(reloaded) => (
            StatusCode::OK,
            Json(json!({
                "event": "reload_model",
                "reloaded": reloaded,
            })),
        ),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "event": "reload_model", "error": error.to_string() })),
        ),
    }
}

async fn require_principal(state: &crate::gateway::GatewayState) -> Result<(), StatusCode> {
    let profile = state.auth.read().await.profile;
    (state
        .admin_trust
        .effective_authority(&state.authority_principal, profile)
        == Some(crate::admin_trust::EffectiveAuthority::Primary))
    .then_some(())
    .ok_or(StatusCode::FORBIDDEN)
}

/// Every control-plane mutation is signed by an authority in the current
/// chain. The shared connection key remains useful for private transport, but
/// it is not itself authorization to mutate control state.
async fn require_signed_admin(
    state: &crate::gateway::GatewayState,
    auth: &InternalNodeAuth,
) -> bool {
    auth.principal().is_some_and(|issuer| {
        crate::admin_trust::control_issuer_is_admin(&state.admin_trust.document(), issuer)
    })
}

/// A public-network node may mint exactly one class of key without being an
/// administrator: its own browser/chat key. The key is still carried over a
/// signed control request, so the primary can bind it to the authenticated
/// node identity without granting that node any administrative mutation.
fn is_self_owned_chat_key(key: &ApiKey, issuer: &AuthorityPrincipal) -> bool {
    key.active
        && !key.id.trim().is_empty()
        && !key.key.trim().is_empty()
        && key.node_id == issuer.node_id
        && key.origin_node_id == issuer.node_id
        && key.label == format!("pom-{}", issuer.node_id)
}

#[derive(Debug, Serialize)]
struct AuthorityMemberView {
    node_id: String,
    short_id: String,
    hostname: String,
    active: bool,
}

#[derive(Debug, Serialize)]
struct AuthorityGrantView {
    sequence: u64,
    node_id: String,
    node_hostname: String,
    promoted_by_node_id: String,
    promoted_by_hostname: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ResignationView {
    Eligible { successor: AuthorityMemberView },
    NoSecondary,
    NoActiveSecondary,
}

#[derive(Debug, Serialize)]
struct AdminAuthorityView {
    viewer: &'static str,
    principal: AuthorityMemberView,
    secondary: Vec<AuthorityMemberView>,
    grants: Vec<AuthorityGrantView>,
    resignation: ResignationView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_transfer: Option<AdminTransferOfferView>,
    permissions: AuthorityPermissions,
}

#[derive(Debug, Serialize)]
struct AdminTransferOfferView {
    kind: &'static str,
    id: String,
    from_node_id: String,
    from_host: String,
    target_node_id: String,
    target_host: String,
    status: &'static str,
    issued_at: String,
}

#[derive(Debug, Serialize)]
struct AuthorityPermissions {
    promote: bool,
    resign: bool,
    demote: bool,
}

fn peer_for_principal<'a>(
    peers: &'a [crate::network::PeerInfo],
    principal: &AuthorityPrincipal,
) -> Option<&'a crate::network::PeerInfo> {
    peers.iter().find(|peer| {
        peer.node_id == principal.node_id
            && peer.libp2p_peer_id.as_deref() == Some(principal.peer_id.as_str())
            && peer.authority_pubkey == principal.pubkey
    })
}

fn member_view(
    principal: &AuthorityPrincipal,
    local_node_id: &str,
    local_hostname: &str,
    peers: &[crate::network::PeerInfo],
) -> AuthorityMemberView {
    let peer = peer_for_principal(peers, principal);
    let hostname = if principal.node_id == local_node_id {
        local_hostname.to_string()
    } else {
        peer.map(|peer| peer.hostname.clone())
            .filter(|hostname| !hostname.is_empty())
            .unwrap_or_else(|| principal.node_id.clone())
    };
    AuthorityMemberView {
        short_id: principal.node_id.chars().take(12).collect(),
        node_id: principal.node_id.clone(),
        hostname,
        active: principal.node_id == local_node_id || peer.is_some_and(|peer| peer.is_alive()),
    }
}

async fn admin_authority_view(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> impl IntoResponse {
    let (profile, local_hostname) = {
        let cfg = state.auth.read().await;
        (
            cfg.profile,
            cfg.node_name
                .clone()
                .unwrap_or_else(|| state.network.node_id.clone()),
        )
    };
    let document = state.admin_trust.document();
    let peers = state.network.alive_peers().await;
    let local_authority = state
        .admin_trust
        .effective_authority(&state.authority_principal, profile);
    let viewer = match local_authority {
        Some(EffectiveAuthority::Primary) => "principal",
        Some(EffectiveAuthority::Secondary) => "secondary",
        None => "node",
    };
    let primary = member_view(
        &document.primary,
        &state.network.node_id,
        &local_hostname,
        &peers,
    );
    let secondary_principals = document
        .grants
        .iter()
        .filter(|grant| grant.level == crate::admin_trust::AdminLevel::Secondary)
        .map(|grant| grant.subject.clone())
        .filter(|subject| {
            state
                .admin_trust
                .effective_authority(subject, NodeProfile::Admin)
                == Some(EffectiveAuthority::Secondary)
        })
        .collect::<Vec<_>>();
    let secondary = secondary_principals
        .iter()
        .map(|subject| member_view(subject, &state.network.node_id, &local_hostname, &peers))
        .collect::<Vec<_>>();
    let grants = document
        .grants
        .iter()
        .filter(|grant| grant.level == crate::admin_trust::AdminLevel::Secondary)
        .map(|grant| {
            let node = member_view(
                &grant.subject,
                &state.network.node_id,
                &local_hostname,
                &peers,
            );
            let issuer = member_view(
                &grant.issuer,
                &state.network.node_id,
                &local_hostname,
                &peers,
            );
            AuthorityGrantView {
                sequence: grant.promotion_seq,
                node_id: grant.subject.node_id.clone(),
                node_hostname: node.hostname,
                promoted_by_node_id: grant.issuer.node_id.clone(),
                promoted_by_hostname: issuer.hostname,
            }
        })
        .collect::<Vec<_>>();
    let latest_secondary = state.admin_trust.latest_secondary();
    let resignation = match latest_secondary {
        None => {
            if secondary.is_empty() {
                ResignationView::NoSecondary
            } else {
                ResignationView::NoActiveSecondary
            }
        }
        Some(successor) => {
            let member = member_view(&successor, &state.network.node_id, &local_hostname, &peers);
            if member.active {
                ResignationView::Eligible { successor: member }
            } else {
                ResignationView::NoActiveSecondary
            }
        }
    };
    let pending_transfer = document.pending_transfer.as_ref().map(|offer| {
        let from = member_view(
            &offer.issuer,
            &state.network.node_id,
            &local_hostname,
            &peers,
        );
        let target = member_view(
            &offer.successor,
            &state.network.node_id,
            &local_hostname,
            &peers,
        );
        AdminTransferOfferView {
            kind: "principal_transfer",
            id: offer.offer_id.clone(),
            from_node_id: offer.issuer.node_id.clone(),
            from_host: from.hostname,
            target_node_id: offer.successor.node_id.clone(),
            target_host: target.hostname,
            status: "pending",
            issued_at: offer.command_id.clone(),
        }
    });
    let is_primary = local_authority == Some(EffectiveAuthority::Primary);
    Json(AdminAuthorityView {
        viewer,
        principal: primary,
        secondary,
        grants,
        resignation,
        pending_transfer,
        permissions: AuthorityPermissions {
            promote: is_primary && document.pending_transfer.is_none(),
            resign: is_primary && document.pending_transfer.is_none(),
            demote: is_primary && document.pending_transfer.is_none(),
        },
    })
}

async fn request_node_promotion(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    if state.auth.read().await.profile != NodeProfile::Infer {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "somente um nó infer pode pedir entrada no processamento" })),
        )
            .into_response();
    }
    let request = match state.admin_trust.node_promotion_request(
        &state.authority_signing_key,
        state.authority_principal.clone(),
    ) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": format!("pedido de entrada recusado: {error}") })),
            )
                .into_response()
        }
    };
    let peers = state.network.alive_peers().await;
    let Some(peer) = peers.into_iter().find(|peer| {
        let Some(peer_id) = peer.libp2p_peer_id.as_deref() else {
            return false;
        };
        if peer.authority_pubkey.is_empty() {
            return false;
        }
        let admin = AuthorityPrincipal {
            node_id: peer.node_id.clone(),
            peer_id: peer_id.to_string(),
            pubkey: peer.authority_pubkey.clone(),
        };
        state
            .admin_trust
            .effective_authority(&admin, NodeProfile::Admin)
            .is_some()
            && peer.profile == NodeProfile::Admin
            && peer.gateway_port.is_some()
    }) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "não há um admin ativo para autorizar o pedido" })),
        )
            .into_response();
    };
    match post_internal_json(
        &state,
        &peer,
        "/api/internal/profile/authorize-node",
        &request,
        PROMOTION_REQUEST_TIMEOUT,
    )
    .await
    {
        Ok(response) => {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "o admin não respondeu".into());
            (status, body).into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("o pedido não chegou ao admin: {error}") })),
        )
            .into_response(),
    }
}

async fn internal_authorize_node(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(request): Json<NodePromotionRequest>,
) -> axum::response::Response {
    if auth.principal().is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if state
        .admin_trust
        .effective_authority(&state.authority_principal, state.auth.read().await.profile)
        .is_none()
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "o emissor não é um admin vigente" })),
        )
            .into_response();
    }
    let Some(peer) = state.network.alive_peers().await.into_iter().find(|peer| {
        peer.node_id == request.subject.node_id
            && peer.authority_pubkey == request.subject.pubkey
            && peer.libp2p_peer_id.as_deref() == Some(request.subject.peer_id.as_str())
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "o nó solicitante não está ativo" })),
        )
            .into_response();
    };
    let command = match state
        .admin_trust
        .authorize_node_promotion_request(&state.authority_signing_key, request)
    {
        Ok(command) => command,
        Err(error) => {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": format!("pedido de entrada recusado: {error}") })),
            )
                .into_response()
        }
    };
    match post_internal_json(
        &state,
        &peer,
        "/api/internal/profile/accept-node",
        &command,
        SYNC_TIMEOUT,
    )
    .await
    {
        Ok(response) => {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "o nó solicitante não respondeu".into());
            (status, body).into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(
                json!({ "error": format!("a autorização não chegou ao nó solicitante: {error}") }),
            ),
        )
            .into_response(),
    }
}

async fn internal_accept_node_promotion(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(command): Json<NodePromotionCommand>,
) -> axum::response::Response {
    if !require_signed_admin(&state, &auth).await {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut cfg = state.auth.write().await;
    if let Err(error) =
        state
            .admin_trust
            .apply_node_promotion(command, &state.authority_principal, cfg.profile)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": format!("autorização administrativa recusada: {error}") })),
        )
            .into_response();
    }
    cfg.profile = NodeProfile::Node;
    if let Err(error) = cfg.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("não foi possível salvar o perfil: {error}") })),
        )
            .into_response();
    }
    drop(cfg);
    state.network.set_profile(NodeProfile::Node);
    state.cluster_logs.set_admin_receiver(false);
    state.audit.set_admin_receiver(false);
    Json(json!({ "profile": "node", "node_id": state.network.node_id })).into_response()
}

async fn relinquish_node(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    let mut cfg = state.auth.write().await;
    if cfg.profile != NodeProfile::Node {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "somente um nó pode voltar a infer" })),
        )
            .into_response();
    }
    cfg.profile = NodeProfile::Infer;
    if let Err(error) = cfg.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("não foi possível salvar o perfil: {error}") })),
        )
            .into_response();
    }
    drop(cfg);
    state.network.set_profile(NodeProfile::Infer);
    Json(json!({
        "profile": "infer",
        "node_id": state.network.node_id,
        "warning": "este nó deixou de participar do processamento da rede"
    }))
    .into_response()
}

async fn internal_upsert_api_key(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(key): Json<ApiKey>,
) -> impl IntoResponse {
    let admin = require_signed_admin(&state, &auth).await;
    let self_owned_chat_key = auth
        .principal()
        .is_some_and(|issuer| is_self_owned_chat_key(&key, issuer));
    if !(admin || self_owned_chat_key) || key.key.is_empty() || key.id.is_empty() {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state.database.insert_api_key(&key) {
        Ok(()) => match refresh_api_key_snapshot(&state).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn internal_revoke_api_key(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(request): Json<RevokeRequest>,
) -> impl IntoResponse {
    if !require_signed_admin(&state, &auth).await {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state
        .database
        .revoke_api_key(&request.key_or_id, &request.revoked_by)
    {
        Ok(_) => match refresh_api_key_snapshot(&state).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn internal_revoke_origin(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(request): Json<RevokeOriginRequest>,
) -> impl IntoResponse {
    if !require_signed_admin(&state, &auth).await {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state
        .database
        .revoke_api_keys_by_origin(&request.origin_node_id, &request.revoked_by)
    {
        Ok(_) => match refresh_api_key_snapshot(&state).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn internal_validate_api_key(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(request): Json<ValidateRequest>,
) -> impl IntoResponse {
    if !require_signed_admin(&state, &auth).await {
        return (
            StatusCode::FORBIDDEN,
            Json(ValidateResponse {
                valid: false,
                key: None,
            }),
        )
            .into_response();
    }
    let key = state
        .database
        .api_key_for_token(&request.token)
        .ok()
        .flatten();
    let blocked_nodes = state.auth.read().await.blocked_nodes.clone();
    let valid = key.as_ref().is_some_and(|key| {
        let origin = if key.origin_node_id.is_empty() {
            &key.node_id
        } else {
            &key.origin_node_id
        };
        key.active && !blocked_nodes.iter().any(|blocked| blocked == origin)
    });
    (
        if valid {
            StatusCode::OK
        } else {
            StatusCode::FORBIDDEN
        },
        Json(ValidateResponse {
            valid,
            key: valid.then_some(key).flatten(),
        }),
    )
        .into_response()
}

// ── Snapshots de KV cache (`--slot-save-path`) ───────────────────────────────

/// Lista os snapshots que o **nó local** conhece, lidos do `/health` da própria
/// porta de inferência. É uma vista rápida; um snapshot de implantação tem uma
/// fatia por hop e o nome do arquivo carrega o `node_id` de cada um.
async fn list_slots(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> impl IntoResponse {
    let health = state
        .network
        .inference_port
        .map(|port| format!("http://127.0.0.1:{port}/health"));
    let mut view = json!({ "slot_persist_enabled": false, "slot_count": 0 });
    if let Some(url) = health {
        if let Ok(resp) = state.peer_http.plain().get(&url).send().await {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                view = json!({
                    "slot_persist_enabled": body.get("slot_persist_enabled").cloned().unwrap_or(json!(false)),
                    "slot_count": body.get("slot_count").cloned().unwrap_or(json!(0)),
                });
            }
        }
    }
    // Cache de prefixo persistente (Phase 3), quando ligado neste gateway.
    if let Some(disk) = &state.prefix_disk {
        let (files, bytes) = disk.stats();
        view["prefix_auto_cache"] = json!({
            "enabled": true,
            "index_entries": files,
            "index_bytes": bytes,
        });
    } else {
        view["prefix_auto_cache"] = json!({ "enabled": false });
    }
    Json(view)
}

#[derive(Debug, Deserialize)]
struct SlotActionQuery {
    /// `save`, `restore` ou `erase`.
    action: String,
}

#[derive(Debug, Deserialize, Default)]
struct SlotActionBody {
    /// Modelo (ou `deploy_id`) cujo pipeline recebe a ação.
    model: String,
    /// Sessão viva a fotografar (`save`). Vem do `x_pom` de uma resposta.
    #[serde(default)]
    session_id: String,
    /// Nova sessão a criar em `restore`.
    #[serde(default)]
    dest_session_id: String,
}

/// `POST /admin/slots/{name}?action=save|restore|erase` - orquestra um snapshot
/// de KV pela implantação inteira. Ver `gateway::snapshot_deployment`.
async fn slot_action(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
    Path(name): Path<String>,
    Query(query): Query<SlotActionQuery>,
    Json(body): Json<SlotActionBody>,
) -> axum::response::Response {
    if body.model.trim().is_empty() {
        return bad_request("informe 'model'");
    }
    let plan = match crate::gateway::resolve_plan_for_model(&state, body.model.trim()).await {
        Ok(plan) => plan,
        Err(error) => return bad_request(error),
    };
    match query.action.as_str() {
        "save" => {
            if body.session_id.trim().is_empty() {
                return bad_request("save exige 'session_id' de uma sessão viva");
            }
            match crate::gateway::snapshot_deployment(
                &plan,
                &name,
                body.session_id.trim(),
                &state.peer_http,
            )
            .await
            {
                Ok(bytes) => Json(json!({ "saved": true, "slot_name": name, "bytes": bytes }))
                    .into_response(),
                Err(error) => bad_request(error),
            }
        }
        "restore" => {
            let dest = if body.dest_session_id.trim().is_empty() {
                format!("slot-{}", Uuid::new_v4())
            } else {
                body.dest_session_id.trim().to_string()
            };
            match crate::gateway::restore_deployment(&plan, &name, &dest, &state.peer_http).await {
                Ok(tokens) => Json(json!({
                    "restored": true,
                    "slot_name": name,
                    "session_id": dest,
                    "restored_tokens": tokens,
                }))
                .into_response(),
                Err(error) => bad_request(error),
            }
        }
        "erase" => {
            crate::gateway::erase_snapshot(&plan, &name, &state.peer_http).await;
            Json(json!({ "erased": true, "slot_name": name })).into_response()
        }
        other => bad_request(format!("ação desconhecida: {other}")),
    }
}

fn bad_request(msg: impl std::fmt::Display) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": msg.to_string() })),
    )
        .into_response()
}

async fn transfer_offers(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> impl IntoResponse {
    let offers = state.transfers.offers.read().await.clone();
    Json(json!({ "offers": offers }))
}

/// Every node this machine knows about: the peers actually on the network plus
/// any blocked node that is not currently connected. Returning only the blocked
/// list left the "Nós conectados" panel permanently empty.
async fn list_nodes(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> impl IntoResponse {
    if !state
        .auth
        .read()
        .await
        .profile
        .allows(ProfileAction::ViewNetworkPeers)
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "o perfil infer só pode ver a própria máquina" })),
        )
            .into_response();
    }
    let blocked = state.auth.read().await.blocked_nodes.clone();
    let self_id = state.network.node_id.clone();

    let mut nodes: Vec<serde_json::Value> = state
        .all_peers()
        .await
        .iter()
        .map(|p| {
            json!({
                "node_id": p.node_id,
                "short_id": &p.node_id[..p.node_id.len().min(12)],
                "hostname": p.hostname,
                "addr": p.addr,
                "profile": p.profile,
                "model": p.model,
                "start_layer": p.start_layer,
                "end_layer": p.end_layer,
                "total_layers": p.total_layers,
                "is_self": p.node_id == self_id,
                "connected": true,
                "blocked": blocked.contains(&p.node_id),
                "verified": p.connection_verified,
            })
        })
        .collect();

    let connected: Vec<String> = nodes
        .iter()
        .filter_map(|n| n["node_id"].as_str().map(str::to_string))
        .collect();
    for id in blocked.iter().filter(|id| !connected.contains(id)) {
        nodes.push(json!({
            "node_id": id,
            "short_id": &id[..id.len().min(12)],
            "hostname": "",
            "addr": "",
            "is_self": false,
            "connected": false,
            "blocked": true,
            "verified": false,
        }));
    }

    Json(nodes).into_response()
}

/// Issue a one-level secondary grant from the current primary. The grant is
/// recorded in the primary's durable chain before delivery, so an offline
/// target can later reconcile the complete chain instead of trusting a
/// profile or an anonymous announcement.
async fn promote_node_to_admin(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
    Path(node_id): Path<String>,
) -> axum::response::Response {
    let profile = {
        let cfg = state.auth.read().await;
        cfg.profile
    };
    if !profile.allows(ProfileAction::PromoteNodeToAdmin)
        || require_principal(&state).await.is_err()
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "somente o administrador pode promover um nó" })),
        )
            .into_response();
    }
    if node_id == state.network.node_id {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "o administrador já é este nó" })),
        )
            .into_response();
    }

    let Some(peer) = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == node_id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "nó desconhecido ou fora da rede" })),
        )
            .into_response();
    };
    let Some(peer_id) = peer.libp2p_peer_id.clone() else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "o peer não anuncia sua identidade libp2p" })),
        )
            .into_response();
    };
    if peer.authority_pubkey.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "o peer não anuncia a chave de autoridade libp2p" })),
        )
            .into_response();
    }
    let subject = AuthorityPrincipal {
        node_id: peer.node_id.clone(),
        peer_id,
        pubkey: peer.authority_pubkey.clone(),
    };
    let grant = match state.admin_trust.signed_secondary_grant(
        &state.authority_signing_key,
        subject,
        &state.network.node_id,
    ) {
        Ok(grant) => grant,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("não foi possível emitir a concessão: {error}") })),
            )
                .into_response();
        }
    };
    if let Err(error) = state
        .admin_trust
        .record_secondary_grant(grant.clone(), &state.authority_signing_key)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("não foi possível registrar a concessão: {error}") })),
        )
            .into_response();
    }
    let trust_document = state.admin_trust.document();
    let response = post_internal_json(
        &state,
        &peer,
        "/api/internal/admin-trust/reconcile",
        &trust_document,
        SYNC_TIMEOUT,
    )
    .await;
    match response {
        Ok(response) if response.status().is_success() => {
            Json(json!({ "profile": "admin", "node_id": node_id })).into_response()
        }
        Ok(response) => {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "promoção recusada pelo nó".to_string());
            (status, body).into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("a promoção não chegou ao nó: {error}") })),
        )
            .into_response(),
    }
}

async fn internal_reconcile_admin_trust(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(document): Json<AdminTrustDocument>,
) -> axum::response::Response {
    if !require_signed_admin(&state, &auth).await {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut cfg = state.auth.write().await;
    if !document
        .grants
        .iter()
        .any(|grant| grant.subject == state.authority_principal)
        || !matches!(cfg.profile, NodeProfile::Node | NodeProfile::Admin)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "a cadeia não é uma concessão para este nó" })),
        )
            .into_response();
    }
    if let Err(error) = state.admin_trust.reconcile(document) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": format!("cadeia administrativa recusada: {error}") })),
        )
            .into_response();
    }
    state
        .network
        .set_policy_principal_pubkey(state.admin_trust.current_primary().pubkey);
    cfg.profile = if state
        .admin_trust
        .effective_authority(&state.authority_principal, NodeProfile::Admin)
        .is_some()
    {
        NodeProfile::Admin
    } else {
        NodeProfile::Node
    };
    let profile = cfg.profile;
    if let Err(error) = cfg.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("não foi possível salvar o perfil: {error}") })),
        )
            .into_response();
    }
    drop(cfg);
    state
        .network
        .set_policy_principal_pubkey(state.admin_trust.current_primary().pubkey);
    state.network.set_profile(profile);
    let admin_receiver = profile == NodeProfile::Admin;
    state.cluster_logs.set_admin_receiver(admin_receiver);
    state.audit.set_admin_receiver(admin_receiver);
    Json(json!({ "profile": profile, "node_id": state.network.node_id })).into_response()
}

#[derive(Debug, Serialize, Deserialize)]
struct TransferCompletion {
    offer: AdminTransferOffer,
    acceptance: AdminTransferAcceptance,
}

async fn internal_complete_transfer(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(completion): Json<TransferCompletion>,
) -> axum::response::Response {
    if !require_signed_admin(&state, &auth).await {
        return StatusCode::FORBIDDEN.into_response();
    }
    if completion.offer.issuer != state.authority_principal {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "a oferta não foi emitida por este principal" })),
        )
            .into_response();
    }
    if let Err(error) = state
        .admin_trust
        .complete_transfer(completion.offer, completion.acceptance)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("transferência administrativa recusada: {error}") })),
        )
            .into_response();
    }
    state
        .network
        .set_policy_principal_pubkey(state.admin_trust.current_primary().pubkey);
    let mut cfg = state.auth.write().await;
    cfg.profile = NodeProfile::Node;
    if let Err(error) = cfg.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("não foi possível salvar o perfil: {error}") })),
        )
            .into_response();
    }
    drop(cfg);
    state.network.set_profile(NodeProfile::Node);
    state.cluster_logs.set_admin_receiver(false);
    state.audit.set_admin_receiver(false);
    Json(json!({ "completed": true })).into_response()
}

async fn resign_authority(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    if require_principal(&state).await.is_err() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "somente o principal pode resignar" })),
        )
            .into_response();
    }
    let successor = match state.admin_trust.latest_secondary() {
        Some(successor) => successor,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "não há um secundário elegível para sucessão" })),
            )
                .into_response()
        }
    };
    let Some(peer) = state.network.alive_peers().await.into_iter().find(|peer| {
        peer.node_id == successor.node_id
            && peer.libp2p_peer_id.as_deref() == Some(successor.peer_id.as_str())
            && peer.authority_pubkey == successor.pubkey
            && peer.profile == NodeProfile::Admin
    }) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "o último secundário promovido não está ativo" })),
        )
            .into_response();
    };
    let offer = match state.admin_trust.begin_transfer(
        &state.authority_signing_key,
        successor,
        peer.is_alive(),
    ) {
        Ok(offer) => offer,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("resignação recusada: {error}") })),
            )
                .into_response()
        }
    };
    let document = state.admin_trust.document();
    let response = post_internal_json(
        &state,
        &peer,
        "/api/internal/admin-trust/reconcile",
        &document,
        SYNC_TIMEOUT,
    )
    .await;
    match response {
        Ok(response) if response.status().is_success() => {
            Json(json!({ "pending": true, "offer_id": offer.offer_id })).into_response()
        }
        Ok(response) => {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "o sucessor recusou a oferta".into());
            (status, body).into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("a oferta não chegou ao sucessor: {error}") })),
        )
            .into_response(),
    }
}

pub(crate) async fn accept_authority_transfer(
    state: &crate::gateway::GatewayState,
    offer_id: &str,
) -> axum::response::Response {
    let Some(offer) = state.admin_trust.pending_transfer() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "oferta de principal não existe" })),
        )
            .into_response();
    };
    if offer.offer_id != offer_id || offer.successor != state.authority_principal {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "esta oferta não é destinada a este peer" })),
        )
            .into_response();
    }
    if state.auth.read().await.profile != NodeProfile::Admin {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "o sucessor precisa continuar admin até aceitar" })),
        )
            .into_response();
    }
    let acceptance = match state.admin_trust.accept_transfer(
        offer.clone(),
        &state.authority_signing_key,
        state.authority_principal.clone(),
    ) {
        Ok(acceptance) => acceptance,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("aceitação recusada: {error}") })),
            )
                .into_response()
        }
    };
    state
        .network
        .set_policy_principal_pubkey(state.admin_trust.current_primary().pubkey);
    let peers = state.network.alive_peers().await;
    let Some(peer) = peers.into_iter().find(|peer| {
        peer.node_id == offer.issuer.node_id
            && peer.libp2p_peer_id.as_deref() == Some(offer.issuer.peer_id.as_str())
            && peer.authority_pubkey == offer.issuer.pubkey
    }) else {
        return Json(json!({
            "accepted": true,
            "offer_id": offer.offer_id,
            "synchronized": false,
            "reason": "o principal anterior está offline; a cadeia aceita permanece persistida"
        }))
        .into_response();
    };
    let completion = TransferCompletion { offer, acceptance };
    match post_internal_json(
        state,
        &peer,
        "/api/internal/admin-trust/complete-transfer",
        &completion,
        SYNC_TIMEOUT,
    )
    .await
    {
        Ok(response) if response.status().is_success() => {
            Json(json!({ "accepted": true, "offer_id": offer_id, "synchronized": true }))
                .into_response()
        }
        Ok(response) => Json(json!({
            "accepted": true,
            "offer_id": offer_id,
            "synchronized": false,
            "reason": format!("principal anterior respondeu {}", response.status())
        }))
        .into_response(),
        Err(error) => Json(json!({
            "accepted": true,
            "offer_id": offer_id,
            "synchronized": false,
            "reason": error.to_string()
        }))
        .into_response(),
    }
}

async fn demote_secondary(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
    Path(node_id): Path<String>,
) -> axum::response::Response {
    if require_principal(&state).await.is_err() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "somente o principal pode despromover um secundário" })),
        )
            .into_response();
    }
    let document = state.admin_trust.document();
    let Some(subject) = document
        .grants
        .iter()
        .rev()
        .find(|grant| {
            grant.subject.node_id == node_id
                && grant.level == crate::admin_trust::AdminLevel::Secondary
        })
        .map(|grant| grant.subject.clone())
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "o nó não é um admin secundário" })),
        )
            .into_response();
    };
    let _demotion = match state
        .admin_trust
        .demote_secondary(&state.authority_signing_key, subject.clone())
    {
        Ok(demotion) => demotion,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!("despromoção recusada: {error}") })),
            )
                .into_response()
        }
    };
    let Some(peer) = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == subject.node_id)
    else {
        return Json(json!({ "demoted": true, "node_id": node_id, "offline": true }))
            .into_response();
    };
    let document = state.admin_trust.document();
    let response = post_internal_json(
        &state,
        &peer,
        "/api/internal/admin-trust/reconcile",
        &document,
        SYNC_TIMEOUT,
    )
    .await;
    match response {
        Ok(response) if response.status().is_success() => {
            Json(json!({ "demoted": true, "node_id": node_id })).into_response()
        }
        Ok(response) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("o secundário recusou a mutação assinada: {}", response.status()) })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("a despromoção não chegou ao secundário: {error}") })),
        )
            .into_response(),
}
}

async fn block_node(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let mut cfg = state.auth.write().await;
    if !cfg.blocked_nodes.contains(&node_id) {
        cfg.blocked_nodes.push(node_id.clone());
    }
    let _ = cfg.save();
    drop(cfg);
    if let Err(error) = state
        .database
        .revoke_api_keys_by_origin(&node_id, &state.network.node_id)
    {
        tracing::error!("não foi possível revogar as API keys da origem: {error}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Err(error) = notify_origin_revoked(&state, &node_id, &state.network.node_id).await {
        tracing::warn!("revogação central da origem falhou: {error:#}");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let _ = refresh_api_key_snapshot(&state).await;
    Json(json!({ "blocked": true, "node_id": node_id })).into_response()
}

async fn unblock_node(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    cfg.blocked_nodes.retain(|id| id != &node_id);
    let _ = cfg.save();
    Json(json!({ "blocked": false, "node_id": node_id }))
}

async fn remove_node(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
    Path(node_id): Path<String>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    cfg.blocked_nodes.retain(|id| id != &node_id);
    let _ = cfg.save();
    Json(json!({ "removed": true, "node_id": node_id }))
}

async fn regenerate_key(
    _auth: ConnectionAuth,
    State(config): State<AuthState>,
) -> impl IntoResponse {
    let mut cfg = config.write().await;
    let raw = Uuid::new_v4().to_string();
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    cfg.connection_key = hex::encode(hasher.finalize());
    let _ = cfg.save();
    Json(json!({ "connection_key": cfg.connection_key }))
}

async fn list_api_keys(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    let keys = match state.database.api_keys() {
        Ok(keys) => keys,
        Err(error) => {
            tracing::error!("não foi possível ler as API keys: {error:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "não foi possível ler as API keys" })),
            )
                .into_response();
        }
    };
    let keys = keys
        .iter()
        .cloned()
        .map(|mut key| {
            if key.id.is_empty() {
                key.id = crate::auth::api_key_identity(&key);
            }
            key
        })
        .collect::<Vec<_>>();
    Json(keys).into_response()
}

#[derive(Debug, Deserialize)]
struct GenerateKeyRequest {
    label: Option<String>,
    /// Node this token is meant for. Empty means the token belongs to the
    /// cluster as a whole rather than to one machine.
    #[serde(default)]
    node_id: Option<String>,
}

/// Mint a token.
///
/// Tokens accumulate rather than replace each other: the operator hands one out
/// per node (and per harness), and the request log attributes each generation
/// back to the one that paid for it. Minting used to deactivate every existing
/// key, which made "a token per node" impossible — the second one silently
/// revoked the first.
async fn generate_api_key(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(req): Json<GenerateKeyRequest>,
) -> impl IntoResponse {
    let key = match create_key(
        &state,
        req.label.unwrap_or_else(|| "default".into()),
        req.node_id.unwrap_or_default(),
    )
    .await
    {
        Ok(key) => key,
        Err(error) => {
            tracing::error!("não foi possível guardar a API key: {error}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "não foi possível guardar a API key" })),
            )
                .into_response();
        }
    };
    (StatusCode::CREATED, Json(key)).into_response()
}

pub(crate) async fn chat_key(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> impl IntoResponse {
    let node_id = state.network.node_id.clone();
    let label = format!("pom-{node_id}");
    if let Ok(Some(key)) = state.database.active_api_key_named(&label, &node_id) {
        let origin = if key.origin_node_id.is_empty() {
            &key.node_id
        } else {
            &key.origin_node_id
        };
        if origin == &node_id {
            return (StatusCode::OK, Json(key)).into_response();
        }
    }
    match create_key(&state, label, node_id).await {
        Ok(key) => (StatusCode::CREATED, Json(key)).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "não foi possível preparar a chave do chat" })),
        )
            .into_response(),
    }
}

async fn revoke_api_key(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
    Path(key): Path<String>,
) -> impl IntoResponse {
    // A connection key identifies an operator role, not a human account. The
    // node id is the honest actor identity available at this boundary.
    let revoked_by = state.network.node_id.clone();
    let existed = match state.database.revoke_api_key(&key, &revoked_by) {
        Ok(existed) => existed,
        Err(error) => {
            tracing::error!("não foi possível revogar a API key: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "não foi possível revogar a API key" })),
            )
                .into_response();
        }
    };
    if let Err(error) = notify_key_revoked(&state, &key, &revoked_by).await {
        tracing::warn!("revogação central da API key falhou: {error:#}");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let _ = refresh_api_key_snapshot(&state).await;
    Json(json!({ "revoked": existed })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::TransferOffer;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request};
    use tower::ServiceExt;

    /// A rota `/admin` devolve as mesmas ofertas de `GET /api/transfer/offers`,
    /// não uma lista vazia fixa: um consumidor da rota de administração tem que
    /// enxergar o que está esperando resposta.
    #[tokio::test]
    async fn transfer_offers_returns_the_real_offers() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;

        state.transfers.offers.write().await.push(TransferOffer {
            pom: None,
            id: "oferta-1".into(),
            from_node_id: "peer-b".into(),
            from_host: "ksia".into(),
            from_gateway: "http://127.0.0.1:8080".into(),
            remote_path: "/models/fatia_0_15.gguf".into(),
            grant: None,
            name: "fatia_0_15.gguf".into(),
            size_bytes: 4_096,
            md5: String::new(),
            start_layer: Some(0),
            end_layer: Some(15),
        });

        let response = transfer_offers(ConnectionAuth, State(state))
            .await
            .into_response();
        let body = crate::gateway::testkit::body_of(response).await;

        let offers = body["offers"].as_array().expect("campo offers");
        assert_eq!(offers.len(), 1, "a rota /admin não pode devolver vazio");
        assert_eq!(offers[0]["name"], "fatia_0_15.gguf");
        assert_eq!(offers[0]["from_host"], "ksia");
    }

    #[tokio::test]
    async fn generated_key_is_stored_and_revocation_is_persistent() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.auth.write().await.profile = NodeProfile::Admin;
        let app = admin_routes().with_state(state.clone());
        let response = app
            .oneshot(
                Request::post("/admin/api-keys")
                    .header("authorization", "Bearer chave")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"label":"teste"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let created = crate::gateway::testkit::body_of(response).await;
        let key_id = created["id"].as_str().unwrap().to_string();
        let secret = created["key"].as_str().unwrap().to_string();
        assert!(state.database.api_key_for_token(&secret).unwrap().is_some());

        let response = admin_routes()
            .with_state(state.clone())
            .oneshot(
                Request::delete(format!("/admin/api-keys/{key_id}"))
                    .header("authorization", "Bearer chave")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if response.status() != StatusCode::OK {
            let body = crate::gateway::testkit::body_of(response).await;
            panic!("resposta inesperada: {body}");
        }
        assert!(state.database.api_key_for_token(&secret).unwrap().is_none());
        assert_eq!(state.database.api_key_revocations().unwrap()[0].1, "eu");
        assert!(state
            .auth
            .read()
            .await
            .api_keys
            .iter()
            .any(|key| key.id == key_id && !key.active));
    }

    #[test]
    fn a_node_can_register_only_its_own_chat_key() {
        let issuer = AuthorityPrincipal {
            node_id: "worker".into(),
            peer_id: "peer-worker".into(),
            pubkey: "pub-worker".into(),
        };
        let own = ApiKey {
            id: "chat-id".into(),
            key: "sk-chat".into(),
            label: "pom-worker".into(),
            node_id: "worker".into(),
            origin_node_id: "worker".into(),
            active: true,
            created_at: String::new(),
        };
        assert!(is_self_owned_chat_key(&own, &issuer));

        let mut other_node = own.clone();
        other_node.node_id = "other".into();
        other_node.origin_node_id = "other".into();
        other_node.label = "pom-other".into();
        assert!(!is_self_owned_chat_key(&other_node, &issuer));

        let mut inactive = own;
        inactive.active = false;
        assert!(!is_self_owned_chat_key(&inactive, &issuer));
    }

    #[tokio::test]
    async fn a_non_admin_chat_key_is_accepted_only_for_the_authenticated_node() {
        let state = crate::gateway::testkit::state_with("admin", vec![]).await;
        let issuer = AuthorityPrincipal {
            node_id: "worker".into(),
            peer_id: "peer-worker".into(),
            pubkey: "pub-worker".into(),
        };
        let key = ApiKey {
            id: "chat-id".into(),
            key: "sk-chat".into(),
            label: "pom-worker".into(),
            node_id: "worker".into(),
            origin_node_id: "worker".into(),
            active: true,
            created_at: String::new(),
        };

        let response = internal_upsert_api_key(
            InternalNodeAuth {
                principal: Some(issuer.clone()),
            },
            State(state.clone()),
            Json(key.clone()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            state
                .database
                .api_key_for_token(&key.key)
                .unwrap()
                .map(|stored| stored.node_id),
            Some("worker".into())
        );

        let mut forged = key;
        forged.node_id = "other".into();
        forged.origin_node_id = "other".into();
        forged.label = "pom-other".into();
        let response = internal_upsert_api_key(
            InternalNodeAuth {
                principal: Some(issuer),
            },
            State(state),
            Json(forged),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn public_admin_keeps_access_to_api_key_management() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        {
            let mut config = state.auth.write().await;
            config.profile = NodeProfile::Admin;
            config.network.visibility = crate::config::NetworkVisibility::Public;
            config.connection_key = "local-admin-credential".into();
        }
        let response = admin_routes()
            .with_state(state.clone())
            .oneshot(
                Request::post("/admin/api-keys")
                    .header("authorization", "Bearer local-admin-credential")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"label":"public-admin"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = admin_routes()
            .with_state(state)
            .oneshot(
                Request::get("/admin/api-keys")
                    .header("authorization", "Bearer local-admin-credential")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_non_admin_cannot_promote_another_node() {
        let state = crate::gateway::testkit::state_with(
            "eu",
            vec![crate::gateway::testkit::peer("outro", "salem", 8081)],
        )
        .await;
        let response =
            promote_node_to_admin(ConnectionAuth, State(state), Path("outro".to_string())).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_secondary_admin_cannot_promote_another_node() {
        let state = crate::gateway::testkit::state_with_secondary_admin(
            "secondary",
            vec![crate::gateway::testkit::peer("outro", "salem", 8081)],
        )
        .await;
        assert_eq!(state.admin_trust.current_primary().node_id, "primary");
        assert_eq!(
            state
                .admin_trust
                .effective_authority(&state.authority_principal, NodeProfile::Admin,),
            Some(crate::admin_trust::EffectiveAuthority::Secondary)
        );
        let response =
            promote_node_to_admin(ConnectionAuth, State(state), Path("outro".to_string())).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn remote_api_key_validation_is_authenticated_and_fails_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/api/internal/api-keys/validate",
            post(
                |headers: HeaderMap, Json(request): Json<ValidateRequest>| async move {
                    let trusted = headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        == Some("Bearer connection")
                        && headers
                            .get(crate::auth::INTERNAL_NODE_REQUEST_HEADER)
                            .and_then(|value| value.to_str().ok())
                            == Some("1");
                    if !trusted {
                        return (StatusCode::UNAUTHORIZED, Json(json!({}))).into_response();
                    }
                    match request.token.as_str() {
                        "remote-valid" => (
                            StatusCode::OK,
                            Json(ValidateResponse {
                                valid: true,
                                key: Some(ApiKey {
                                    id: "remote-id".into(),
                                    key: request.token,
                                    label: "remote".into(),
                                    node_id: "origin-node".into(),
                                    origin_node_id: "origin-node".into(),
                                    active: true,
                                    created_at: "2026-09-07T00:00:00Z".into(),
                                }),
                            }),
                        )
                            .into_response(),
                        "admin-error" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                        "admin-timeout" => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            StatusCode::OK.into_response()
                        }
                        _ => (
                            StatusCode::FORBIDDEN,
                            Json(ValidateResponse {
                                valid: false,
                                key: None,
                            }),
                        )
                            .into_response(),
                    }
                },
            ),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut peer = crate::gateway::testkit::peer("principal", "principal", port);
        peer.profile = NodeProfile::Admin;
        let state = crate::gateway::testkit::state_with("worker", vec![peer]).await;
        state.auth.write().await.connection_key = "connection".into();

        async fn extract(
            state: &crate::gateway::GatewayState,
            token: &str,
        ) -> Result<crate::auth::ApiKeyAuth, crate::auth::AuthRejection> {
            let request = Request::builder()
                .header("x-api-key", token)
                .body(())
                .unwrap();
            let (mut parts, _) = request.into_parts();
            <crate::auth::ApiKeyAuth as axum::extract::FromRequestParts<
                crate::gateway::GatewayState,
            >>::from_request_parts(&mut parts, state)
            .await
        }

        let valid = extract(&state, "remote-valid").await.unwrap();
        assert_eq!(valid.token(), "remote-valid");
        assert!(state
            .database
            .api_key_for_token("remote-valid")
            .unwrap()
            .is_some());
        assert!(matches!(
            extract(&state, "remote-invalid").await,
            Err(crate::auth::AuthRejection::InvalidKey)
        ));
        assert!(matches!(
            extract(&state, "admin-error").await,
            Err(crate::auth::AuthRejection::InvalidKey)
        ));
        assert!(matches!(
            extract(&state, "admin-timeout").await,
            Err(crate::auth::AuthRejection::InvalidKey)
        ));
    }

    #[tokio::test]
    async fn internal_key_transitions_require_signed_control_and_are_idempotent() {
        let state = crate::gateway::testkit::state_with("principal", vec![]).await;
        state.auth.write().await.profile = NodeProfile::Admin;
        let signed_request = |path: &str, body: Vec<u8>| {
            let headers = state
                .signed_control_headers("POST", path, &body)
                .expect("assinar comando de teste");
            Request::post(path)
                .header("content-type", "application/json")
                .header(
                    crate::auth::INTERNAL_NODE_REQUEST_HEADER,
                    headers
                        .get(crate::auth::INTERNAL_NODE_REQUEST_HEADER)
                        .expect("marcador interno")
                        .to_str()
                        .unwrap(),
                )
                .header(
                    crate::auth::CONTROL_SIGNATURE_HEADER,
                    headers
                        .get(crate::auth::CONTROL_SIGNATURE_HEADER)
                        .expect("assinatura interna")
                        .to_str()
                        .unwrap(),
                )
                .body(Body::from(body))
                .unwrap()
        };
        let key = ApiKey {
            id: "retry-safe".into(),
            key: "sk-retry-safe".into(),
            label: "retry-safe".into(),
            node_id: "worker".into(),
            origin_node_id: "worker".into(),
            active: true,
            created_at: "2026-09-07T00:00:00Z".into(),
        };
        for _ in 0..2 {
            let response = admin_routes()
                .with_state(state.clone())
                .oneshot(signed_request(
                    "/api/internal/api-keys/upsert",
                    serde_json::to_vec(&key).unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        assert_eq!(
            state
                .database
                .api_keys()
                .unwrap()
                .into_iter()
                .filter(|stored| stored.id == key.id)
                .count(),
            1
        );

        for _ in 0..2 {
            let response = admin_routes()
                .with_state(state.clone())
                .oneshot(signed_request(
                    "/api/internal/api-keys/revoke",
                    serde_json::to_vec(&RevokeRequest {
                        key_or_id: key.id.clone(),
                        revoked_by: "principal".into(),
                    })
                    .unwrap(),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        assert_eq!(state.database.api_key_revocations().unwrap().len(), 1);
        assert!(state
            .database
            .api_key_for_token(&key.key)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn blocking_an_origin_revokes_local_inference_keys() {
        let state = crate::gateway::testkit::state_with("principal", vec![]).await;
        state.auth.write().await.profile = NodeProfile::Admin;
        let key = ApiKey {
            id: "blocked-origin-key".into(),
            key: "sk-blocked-origin".into(),
            label: "worker-key".into(),
            node_id: "worker".into(),
            origin_node_id: "worker".into(),
            active: true,
            created_at: "2026-09-07T00:00:00Z".into(),
        };
        state.database.insert_api_key(&key).unwrap();
        let response = block_node(
            ConnectionAuth,
            State(state.clone()),
            Path("worker".to_string()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(state
            .database
            .api_key_for_token(&key.key)
            .unwrap()
            .is_none());

        let request = Request::get("/v1/models")
            .header("x-api-key", &key.key)
            .body(())
            .unwrap();
        let (mut parts, _) = request.into_parts();
        let result = <crate::auth::ApiKeyAuth as axum::extract::FromRequestParts<
            crate::gateway::GatewayState,
        >>::from_request_parts(&mut parts, &state)
        .await;
        assert!(matches!(
            result,
            Err(crate::auth::AuthRejection::InvalidKey)
        ));
    }

    struct EnvironmentValue {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvironmentValue {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvironmentValue {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    async fn promotion_state_pair(
        admin_port: u16,
        infer_port: u16,
        stale_infer_host: &str,
    ) -> (crate::gateway::GatewayState, crate::gateway::GatewayState) {
        let admin_key = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        let infer_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let admin_principal =
            AuthorityPrincipal::from_signing_key("admin", "peer-admin", &admin_key);
        let infer_principal =
            AuthorityPrincipal::from_signing_key("infer", "peer-infer", &infer_key);
        let admin_trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "promotion-lab",
            admin_principal.clone(),
            &admin_key,
        )
        .expect("admin ancorado no laboratório");
        let infer_trust = crate::admin_trust::AdminTrust::in_memory(admin_trust.document())
            .expect("âncora transportada para o infer");

        let admin_peer = {
            let mut peer = crate::gateway::testkit::peer("admin", "admin", admin_port);
            peer.profile = NodeProfile::Admin;
            peer.authority_pubkey = admin_principal.pubkey.clone();
            peer.libp2p_peer_id = Some(admin_principal.peer_id.clone());
            peer
        };
        let infer_peer = {
            let mut peer = crate::gateway::testkit::peer("infer", "infer", infer_port);
            peer.profile = NodeProfile::Infer;
            peer.authority_pubkey = infer_principal.pubkey.clone();
            peer.libp2p_peer_id = Some(infer_principal.peer_id.clone());
            peer.addr = format!("{stale_infer_host}:42777");
            peer.addr_candidates = vec!["127.0.0.1:42777".into()];
            peer
        };

        let mut admin = crate::gateway::testkit::state_with("admin", vec![infer_peer]).await;
        let mut infer = crate::gateway::testkit::state_with("infer", vec![admin_peer]).await;

        {
            let mut auth = admin.auth.write().await;
            auth.profile = NodeProfile::Admin;
            auth.connection_key = "shared-connection".into();
        }
        admin.network.set_profile(NodeProfile::Admin);
        admin.admin_trust = std::sync::Arc::new(admin_trust);
        admin.authority_signing_key = std::sync::Arc::new(admin_key.clone());
        admin.authority_principal = admin_principal.clone();
        admin.gateway_port = admin_port;

        {
            let mut auth = infer.auth.write().await;
            auth.profile = NodeProfile::Infer;
            auth.connection_key = "shared-connection".into();
        }
        infer.network.set_profile(NodeProfile::Infer);
        infer.admin_trust = std::sync::Arc::new(infer_trust);
        infer.authority_signing_key = std::sync::Arc::new(infer_key.clone());
        infer.authority_principal = infer_principal.clone();
        infer.gateway_port = infer_port;

        (admin, infer)
    }

    async fn spawn_admin_router(
        listener: tokio::net::TcpListener,
        state: crate::gateway::GatewayState,
    ) -> std::net::SocketAddr {
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .merge(admin_routes())
            .merge(crate::webapp::routes())
            .with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("gateway do laboratório");
        });
        address
    }

    #[tokio::test]
    async fn infer_promotion_reaches_admin_and_returns_over_a_live_candidate() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let _scheme_lock = crate::peer_client::POLITICA_GLOBAL.lock().unwrap();
        crate::peer_client::set_tls_ativo(false);
        let config_path = std::env::temp_dir().join(format!(
            "pom-promotion-lab-{}-{}.json",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        let _config = EnvironmentValue::set("POM_CONFIG", &config_path);
        std::fs::write(&config_path, "{}").expect("configuração de teste");

        let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener admin");
        let admin_port = admin_listener.local_addr().unwrap().port();
        let infer_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener infer");
        let infer_port = infer_listener.local_addr().unwrap().port();

        let (admin, infer) = promotion_state_pair(admin_port, infer_port, "[2001:db8::dead]").await;
        {
            let mut peers = infer.network.peers.write().await;
            let admin_peer = peers.get_mut("admin").expect("admin no infer");
            admin_peer.addr = "[2001:db8::dead]:42777".into();
            admin_peer.addr_candidates = vec!["127.0.0.1:42777".into()];
        }
        assert_eq!(admin.admin_trust.document().replay.len(), 0);
        assert_eq!(infer.admin_trust.document().replay.len(), 0);
        let admin_address = spawn_admin_router(admin_listener, admin.clone()).await;
        let infer_address = spawn_admin_router(infer_listener, infer.clone()).await;

        let response = reqwest::Client::new()
            .post(format!(
                "http://{infer_address}/api/ui/network/request-node"
            ))
            .bearer_auth("shared-connection")
            .send()
            .await
            .expect("pedido do infer");
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(
            admin.admin_trust.document().replay.len(),
            2,
            "a ida chegou ao admin: status={status}, body={body}, infer_replay={}, infer_profile={:?}",
            infer.admin_trust.document().replay.len(),
            infer.auth.read().await.profile,
        );
        assert_eq!(
            status,
            StatusCode::OK,
            "o admin deve receber a ida e voltar pelo candidato vivo; admin={admin_address}, body={body}"
        );
        assert_eq!(infer.auth.read().await.profile, NodeProfile::Node);
        assert_eq!(infer.network.profile(), NodeProfile::Node);
        assert_eq!(
            crate::config::Config::load().unwrap().profile,
            NodeProfile::Node
        );

        let _ = std::fs::remove_file(config_path);
    }

    #[tokio::test]
    async fn infer_receives_an_explicit_admin_refusal() {
        let _scheme_lock = crate::peer_client::POLITICA_GLOBAL.lock().unwrap();
        crate::peer_client::set_tls_ativo(false);
        let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener admin");
        let admin_port = admin_listener.local_addr().unwrap().port();
        let infer_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener infer");
        let infer_port = infer_listener.local_addr().unwrap().port();
        let (admin, infer) = promotion_state_pair(admin_port, infer_port, "127.0.0.2").await;
        admin.auth.write().await.profile = NodeProfile::Node;
        spawn_admin_router(admin_listener, admin).await;
        let infer_address = spawn_admin_router(infer_listener, infer).await;

        let response = reqwest::Client::new()
            .post(format!(
                "http://{infer_address}/api/ui/network/request-node"
            ))
            .bearer_auth("shared-connection")
            .send()
            .await
            .expect("pedido do infer");
        let status = response.status();
        let body = response.text().await.unwrap();

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("o emissor não é um admin vigente"), "{body}");
    }
}
