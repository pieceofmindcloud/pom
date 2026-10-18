use crate::admin_trust::{
    AdminTransferAcceptance, AdminTransferOffer, AdminTrustDocument, AuthorityPrincipal,
    EffectiveAuthority,
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
        .route("/api/ui/network/trust-sync", post(sync_trust_chain))
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

/// Asks the principal about a key this node does not know. Only the hash
/// travels: the principal never needs, and never receives, the secret.
#[derive(Debug, Serialize, Deserialize)]
struct ValidateRequest {
    key_hash: String,
}

/// A key just minted: the registry entry plus its secret, which leaves the
/// node exactly once, in this response.
#[derive(Debug, Serialize)]
pub(crate) struct IssuedApiKey {
    #[serde(flatten)]
    pub key: ApiKey,
    /// The secret. Named `key` because that is what clients copy.
    #[serde(rename = "key")]
    pub secret: String,
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

pub(crate) async fn principal_peer_for(
    network: &crate::network::Network,
    primary: &AuthorityPrincipal,
) -> Option<crate::network::PeerInfo> {
    network.alive_peers().await.into_iter().find(|peer| {
        peer.node_id != network.node_id
            && peer.node_id == primary.node_id
            && peer.libp2p_peer_id.as_deref() == Some(primary.peer_id.as_str())
            && peer.pubkey == primary.pubkey
            && peer.gateway_port.is_some()
    })
}

/// Gateway addresses of the peer, the one that already answered first
/// (`Network::gateway_bases`).
fn gateway_urls(network: &crate::network::Network, peer: &crate::network::PeerInfo) -> Vec<String> {
    network.gateway_bases(peer)
}

/// Sends a control call to each address the peer announced until one of them
/// answers and, last, through the libp2p tunnel
/// (`peer_client::send_to_gateway_over`). An address that ceased to exist must
/// not block the promotion while the same announcement still carries a live
/// candidate, and a peer only the relay reaches (Docker bridge, NAT) has no
/// direct address at all.
pub(crate) async fn post_internal_json<T: Serialize>(
    state: &crate::gateway::GatewayState,
    peer: &crate::network::PeerInfo,
    path: &str,
    body: &T,
    timeout: Duration,
) -> anyhow::Result<reqwest::Response> {
    let urls = gateway_urls(&state.network, peer);
    if urls.is_empty() && crate::peer_client::TunnelPeer::from_peer(peer).is_none() {
        anyhow::bail!("the peer does not announce a gateway port");
    }
    let client = state.peer_http.for_peer(&peer.node_id)?;
    let body_bytes = serde_json::to_vec(body)?;
    let control_headers = state.signed_control_headers("POST", path, &body_bytes)?;
    let connection_key = {
        let config = state.auth.read().await;
        crate::auth::internal_node_token(&config)
    };
    crate::peer_client::send_to_gateway_over(peer, &urls, |base| {
        let mut request = client
            .post(format!("{base}{path}"))
            .timeout(timeout)
            .headers(control_headers.clone());
        if let Some(connection_key) = &connection_key {
            request = request.bearer_auth(connection_key);
        }
        request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body_bytes.clone())
    })
    .await
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
    let urls = gateway_urls(&state.network, &peer);
    let client = state.peer_http.for_peer(&peer.node_id)?;
    let body_bytes = serde_json::to_vec(body)?;
    let connection_key = {
        let config = state.auth.read().await;
        crate::auth::internal_node_token(&config)
    };
    let mut last_error = "central admin synchronization failed".to_string();
    for attempt in 0..SYNC_ATTEMPTS {
        // Direct addresses and, last, the tunnel: a node that only reaches the
        // principal through the relay also registers and revokes keys. Each
        // attempt signs a new envelope, because the principal refuses a nonce
        // it has already seen and these mutations are idempotent.
        let control_headers = state.signed_control_headers("POST", path, &body_bytes)?;
        let result = crate::peer_client::send_to_gateway_over(&peer, &urls, |base| {
            let mut request = client
                .post(format!("{base}{path}"))
                .timeout(SYNC_TIMEOUT)
                .headers(control_headers.clone());
            if let Some(connection_key) = &connection_key {
                request = request.bearer_auth(connection_key);
            }
            request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body_bytes.clone())
        })
        .await;
        match result {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                last_error = format!(
                    "central admin rejected synchronization: {}",
                    response.status()
                )
            }
            Err(error) => last_error = format!("central admin unreachable: {error:#}"),
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

/// Mint a key. The registry (here and at the principal) keeps the hash and
/// the hint; the secret is returned to the caller and forgotten.
async fn create_key(
    state: &crate::gateway::GatewayState,
    label: String,
    node_id: String,
) -> anyhow::Result<IssuedApiKey> {
    let secret = crate::auth::generate_api_key_secret();
    let key = ApiKey {
        id: Uuid::new_v4().simple().to_string(),
        key_hash: crate::auth::api_key_hash(&secret),
        key_hint: crate::auth::api_key_hint(&secret),
        label,
        node_id,
        origin_node_id: state.network.node_id.clone(),
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    notify_key_created(state, &key).await?;
    state.database.insert_api_key(&key)?;
    refresh_api_key_snapshot(state).await?;
    Ok(IssuedApiKey { key, secret })
}

pub(crate) async fn validate_api_key_remote(
    state: &crate::gateway::GatewayState,
    token: &str,
) -> anyhow::Result<Option<ApiKey>> {
    let key_hash = crate::auth::api_key_hash(token);
    validate_api_key_remote_parts(
        &state.network,
        &state.peer_http,
        &state.auth,
        &key_hash,
        Some(state.signed_control_headers(
            "POST",
            "/api/internal/api-keys/validate",
            &serde_json::to_vec(&ValidateRequest {
                key_hash: key_hash.clone(),
            })?,
        )?),
    )
    .await
}

pub(crate) async fn validate_api_key_remote_parts(
    network: &crate::network::Network,
    peer_http: &crate::peer_client::PeerClients,
    auth: &AuthState,
    key_hash: &str,
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
        key_hash: key_hash.to_string(),
    };
    let body_bytes = serde_json::to_vec(&body)?;
    let control_headers = control_headers;
    let connection_key = {
        let config = auth.read().await;
        crate::auth::internal_node_token(&config)
    };
    let bases = gateway_urls(network, &peer);
    for attempt in 0..SYNC_ATTEMPTS {
        // Direct addresses and, last, the libp2p tunnel.
        let result = crate::peer_client::send_to_gateway_over(&peer, &bases, |base| {
            let mut request = client
                .post(format!("{base}/api/internal/api-keys/validate"))
                .timeout(SYNC_TIMEOUT)
                .headers(control_headers.clone().unwrap_or_default());
            if let Some(connection_key) = &connection_key {
                request = request.bearer_auth(connection_key);
            }
            request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body_bytes.clone())
        })
        .await;
        if let Ok(response) = result {
            if let Ok(payload) = response.json::<ValidateResponse>().await {
                return Ok(payload.valid.then_some(payload.key).flatten());
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
            "/api/internal/admin-trust/chain",
            post(internal_trust_chain),
        )
        .route(
            "/api/internal/admin-trust/complete-transfer",
            post(internal_complete_transfer),
        )
        .route("/api/internal/reload_model", post(internal_reload_model))
        .route(
            crate::webui::TOKEN_PATH,
            post(crate::webui::internal_token),
        )
        .route(
            crate::webui::ROSTER_PATH,
            post(crate::webui::internal_roster),
        )
        .route(
            crate::webui::DECIDE_PATH,
            post(crate::webui::internal_decide),
        )
        .route(
            crate::webui::REVOCATIONS_PATH,
            post(crate::webui::internal_revocations),
        )
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
/// pipeline. It starts the load and answers at once with `state`: `loading`
/// while the model comes up, `ready` when it is loaded. The gateway sends it
/// to every idle peer concurrently and repeats it until all answer `ready`, so
/// no call stays open for the length of a model load and no hop reloads
/// sequentially through the inference chain. A load that cannot happen
/// (failed, thermal, not configured) is a 503 with the reason.
async fn internal_reload_model(
    _auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(event): Json<serde_json::Value>,
) -> impl IntoResponse {
    if event.get("event").and_then(|value| value.as_str()) != Some("reload_model") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "unknown event" })),
        );
    }
    match state.runtime.wake_from_idle().await {
        Ok(wake) => (
            StatusCode::OK,
            Json(json!({
                "event": "reload_model",
                "state": if wake == crate::runtime::Wake::Ready { "ready" } else { "loading" },
                "reloaded": wake == crate::runtime::Wake::Started,
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
        == Some(crate::admin_trust::EffectiveAuthority::Principal))
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

/// A mutation that governs the network is signed by the principal itself. A
/// secondary admin deploys models and nothing else.
async fn require_signed_principal(
    state: &crate::gateway::GatewayState,
    auth: &InternalNodeAuth,
) -> bool {
    auth.principal()
        .is_some_and(|issuer| crate::gateway::issuer_is_principal(state, issuer))
}

/// A public-network node may mint exactly one class of key without being an
/// administrator: its own browser/chat key. The key is still carried over a
/// signed control request, so the primary can bind it to the authenticated
/// node identity without granting that node any administrative mutation.
fn is_self_owned_chat_key(key: &ApiKey, issuer: &AuthorityPrincipal) -> bool {
    key.active
        && !key.id.trim().is_empty()
        && !key.key_hash.trim().is_empty()
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
            && peer.pubkey == principal.pubkey
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
        Some(EffectiveAuthority::Principal) => "principal",
        Some(EffectiveAuthority::Admin) => "admin",
        None => "member",
    };
    let primary = member_view(
        &document.primary,
        &state.network.node_id,
        &local_hostname,
        &peers,
    );
    // A node promoted again after a demotion has two grants and is one admin.
    let mut secondary_principals: Vec<crate::admin_trust::AuthorityPrincipal> = Vec::new();
    for grant in document
        .grants
        .iter()
        .filter(|grant| grant.level == crate::admin_trust::AdminLevel::Secondary)
    {
        if !secondary_principals.contains(&grant.subject)
            && state
                .admin_trust
                .effective_authority(&grant.subject, NodeProfile::Admin)
                == Some(EffectiveAuthority::Admin)
        {
            secondary_principals.push(grant.subject.clone());
        }
    }
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
    let is_primary = local_authority == Some(EffectiveAuthority::Principal);
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

async fn internal_upsert_api_key(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(key): Json<ApiKey>,
) -> impl IntoResponse {
    let admin = require_signed_principal(&state, &auth).await;
    let self_owned_chat_key = auth
        .principal()
        .is_some_and(|issuer| is_self_owned_chat_key(&key, issuer));
    if !(admin || self_owned_chat_key) || key.key_hash.is_empty() || key.id.is_empty() {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Err(error) = state.database.insert_api_key(&key) {
        tracing::warn!("could not register the API key: {error:#}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    // A node's new internal key retires the previous one, whose secret the node no
    // longer has: without this it would stay active on the principal, ownerless.
    if let Err(error) = state
        .database
        .revoke_superseded_node_keys(&state.network.node_id)
    {
        tracing::warn!("could not revoke replaced internal keys: {error:#}");
    }
    match refresh_api_key_snapshot(&state).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn internal_revoke_api_key(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(request): Json<RevokeRequest>,
) -> impl IntoResponse {
    if !require_signed_principal(&state, &auth).await {
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
    if !require_signed_principal(&state, &auth).await {
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
        .api_key_for_hash(&request.key_hash)
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

/// Lists the snapshots that the **local node** knows, read from the `/health` of its own
/// inference port. It is a quick view; a deployment snapshot has one
/// slice per hop and the file name carries each one's `node_id`.
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
    // Persistent prefix cache (Phase 3), when turned on in this gateway.
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
    /// `save`, `restore` or `erase`.
    action: String,
}

#[derive(Debug, Deserialize, Default)]
struct SlotActionBody {
    /// Model (or `deploy_id`) whose pipeline receives the action.
    model: String,
    /// Live session to snapshot (`save`). Comes from the `x_pom` of a response.
    #[serde(default)]
    session_id: String,
    /// New session to create in `restore`.
    #[serde(default)]
    dest_session_id: String,
}

/// `POST /admin/slots/{name}?action=save|restore|erase` - orchestrates a KV
/// snapshot across the whole deployment. See `gateway::snapshot_deployment`.
async fn slot_action(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
    Path(name): Path<String>,
    Query(query): Query<SlotActionQuery>,
    Json(body): Json<SlotActionBody>,
) -> axum::response::Response {
    if body.model.trim().is_empty() {
        return bad_request("provide 'model'");
    }
    let plan = match crate::gateway::resolve_plan_for_model(&state, body.model.trim()).await {
        Ok(plan) => plan,
        Err(error) => return bad_request(error),
    };
    match query.action.as_str() {
        "save" => {
            if body.session_id.trim().is_empty() {
                return bad_request("save requires the 'session_id' of a live session");
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
        other => bad_request(format!("unknown action: {other}")),
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
            Json(json!({ "error": "the infer profile can only see its own machine" })),
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
                // Through where this node actually talks to the peer; `addr` is only what
                // it announces, and may be an address nobody can reach.
                "route": (p.node_id != self_id).then(|| state.network.peer_route(p)),
                "profile": p.profile,
                "trust_version": p.trust_version,
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
            Json(json!({ "error": "only the administrator can promote a node" })),
        )
            .into_response();
    }
    if node_id == state.network.node_id {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "the administrator is already this node" })),
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
            Json(json!({ "error": "unknown node or outside the network" })),
        )
            .into_response();
    };
    let Some(peer_id) = peer.libp2p_peer_id.clone() else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "the peer does not announce its libp2p identity" })),
        )
            .into_response();
    };
    if peer.pubkey.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "the peer does not announce the libp2p authority key" })),
        )
            .into_response();
    }
    // Refused before anything is signed: an inference node's role is set at
    // admission, and a grant recorded for one would stay in the chain.
    if crate::gateway::peer_is_visitor(&state, &peer) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "an inference node cannot be made an administrator",
                "reason": "visitor_target"
            })),
        )
            .into_response();
    }
    let subject = AuthorityPrincipal {
        node_id: peer.node_id.clone(),
        peer_id,
        pubkey: peer.pubkey.clone(),
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
                Json(json!({ "error": format!("could not issue the grant: {error}") })),
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
            Json(json!({ "error": format!("could not record the grant: {error}") })),
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
    // The target answered and did not become an admin: the grant is taken
    // back with a signed demotion, so the chain never names an admin that
    // is not one. Silence is different - the grant stands and the target
    // adopts it when it next asks for the chain.
    let refusal = match response {
        Ok(response) if response.status().is_success() => {
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            (body["profile"] != "admin").then(|| {
                (
                    StatusCode::CONFLICT,
                    json!({ "error": "the node did not accept the administrator role" })
                        .to_string(),
                )
            })
        }
        Ok(response) => {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "promotion refused by the node".to_string());
            Some((status, body))
        }
        Err(error) => {
            tracing::warn!(node = %node_id, "promotion recorded, not yet delivered to the node: {error:#}");
            spawn_trust_fanout(&state, Some(node_id.clone()));
            return (
                StatusCode::ACCEPTED,
                Json(json!({ "profile": "admin", "node_id": node_id, "pending": true })),
            )
                .into_response();
        }
    };
    if let Some((status, body)) = refusal {
        if let Err(error) = state
            .admin_trust
            .demote_secondary(&state.authority_signing_key, grant.subject.clone())
        {
            tracing::warn!(node = %node_id, "promotion refused and not undone: {error:#}");
        }
        return (status, body).into_response();
    }
    publish_chain_version(&state);
    spawn_trust_fanout(&state, Some(node_id.clone()));
    Json(json!({ "profile": "admin", "node_id": node_id })).into_response()
}

async fn internal_reconcile_admin_trust(
    auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(document): Json<AdminTrustDocument>,
) -> axum::response::Response {
    if !require_signed_admin(&state, &auth).await {
        return StatusCode::FORBIDDEN.into_response();
    }
    // The principal's own copy settles two copies the counters call equal.
    let from_principal = auth
        .principal()
        .is_some_and(|issuer| issuer.pubkey == state.admin_trust.current_primary().pubkey);
    match adopt_trust_chain(&state, document, from_principal).await {
        Ok(profile) => {
            Json(json!({ "profile": profile, "node_id": state.network.node_id })).into_response()
        }
        Err((status, error)) => (status, Json(json!({ "error": error }))).into_response(),
    }
}

/// Takes a newer copy of the trust chain and brings this node in line with
/// it. Every member receives the chain, not only the node it promotes or
/// demotes: a member that does not know a secondary admin refuses its
/// commands. The profile follows the chain - admin while it grants this node
/// authority, node when it no longer does - except for an inference node,
/// whose role is set at admission and never raised by a grant.
pub(crate) async fn adopt_trust_chain(
    state: &crate::gateway::GatewayState,
    document: AdminTrustDocument,
    from_principal: bool,
) -> Result<NodeProfile, (StatusCode, String)> {
    let mut cfg = state.auth.write().await;
    state
        .admin_trust
        .reconcile_from(document, from_principal)
        .map_err(|error| {
            (
                StatusCode::FORBIDDEN,
                format!("administrative chain refused: {error}"),
            )
        })?;
    publish_chain_version(state);
    state
        .network
        .set_policy_principal_pubkey(state.admin_trust.current_primary().pubkey);
    let granted = state
        .admin_trust
        .effective_authority(&state.authority_principal, NodeProfile::Admin)
        .is_some();
    let profile = match cfg.profile {
        NodeProfile::Visitor => NodeProfile::Visitor,
        _ if granted => NodeProfile::Admin,
        _ => NodeProfile::Worker,
    };
    if profile != cfg.profile {
        cfg.profile = profile;
        cfg.save().map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not save the profile: {error}"),
            )
        })?;
    }
    drop(cfg);
    state.network.set_profile(profile);
    let admin_receiver = profile == NodeProfile::Admin;
    state.cluster_logs.set_admin_receiver(admin_receiver);
    state.audit.set_admin_receiver(admin_receiver);
    Ok(profile)
}

#[derive(Debug, Default, Deserialize)]
struct TrustChainQuery {
    #[serde(default)]
    have: String,
}

/// The chain a member asks the principal for: the whole document, or only
/// "you are current" when the caller already has this version. The document
/// is not a secret - a public network publishes it - and the caller checks
/// its signatures before adopting it.
async fn internal_trust_chain(
    _auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(query): Json<TrustChainQuery>,
) -> axum::response::Response {
    if query.have == state.admin_trust.chain_version() {
        return Json(json!({ "current": true })).into_response();
    }
    Json(json!({ "current": false, "document": state.admin_trust.document() })).into_response()
}

/// Sends the current chain to every member but `skip`, each on its own task:
/// a promotion or demotion has to be known by all of them, and one slow peer
/// must not hold the others. A peer that misses it asks later
/// (`spawn_trust_chain_keeper`).
fn spawn_trust_fanout(state: &crate::gateway::GatewayState, skip: Option<String>) {
    let state = state.clone();
    tokio::spawn(async move {
        let document = state.admin_trust.document();
        for peer in state.network.alive_peers().await {
            if peer.node_id == state.network.node_id || skip.as_deref() == Some(&peer.node_id) {
                continue;
            }
            let state = state.clone();
            let document = document.clone();
            tokio::spawn(async move {
                let sent = post_internal_json(
                    &state,
                    &peer,
                    "/api/internal/admin-trust/reconcile",
                    &document,
                    SYNC_TIMEOUT,
                )
                .await;
                match sent {
                    Ok(response) if response.status().is_success() => {}
                    Ok(response) => tracing::debug!(
                        node = %peer.node_id,
                        "administrative chain: the peer answered {}",
                        response.status()
                    ),
                    Err(error) => tracing::debug!(
                        node = %peer.node_id,
                        "administrative chain did not reach the peer: {error:#}"
                    ),
                }
            });
        }
    });
}

/// Announces the version of the local chain. Called wherever the chain
/// changes; the keeper repeats it, so a missed call only delays the news.
pub(crate) fn publish_chain_version(state: &crate::gateway::GatewayState) {
    state
        .network
        .set_trust_version(state.admin_trust.chain_version());
}

/// How often a node compares its chain with what its peers announce. The
/// comparison is local; a request is only made when a peer is ahead.
const TRUST_CHAIN_CHECK: Duration = Duration::from_secs(30);
/// How many peers one round asks before giving up until the next.
const TRUST_CHAIN_SOURCES: usize = 3;

/// The peers whose announced chain is worth fetching, best first: any member
/// ahead of this node by the counters, and the principal whenever its copy
/// differs at the same counters. The announced version is only a hint of
/// where to look - what is fetched is verified signature by signature.
async fn chain_sources(
    state: &crate::gateway::GatewayState,
) -> Vec<(crate::network::PeerInfo, bool)> {
    let mine = state.admin_trust.chain_version();
    let Some(my_counters) = crate::admin_trust::version_counters(&mine) else {
        return Vec::new();
    };
    let primary = state.admin_trust.current_primary();
    let mut sources = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .filter_map(|peer| {
            let theirs = peer.trust_version.as_deref()?;
            if theirs == mine {
                return None;
            }
            let counters = crate::admin_trust::version_counters(theirs)?;
            let is_principal = peer.pubkey == primary.pubkey;
            (counters > my_counters || (counters == my_counters && is_principal)).then_some((
                counters,
                is_principal,
                peer,
            ))
        })
        .collect::<Vec<_>>();
    sources.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));
    sources
        .into_iter()
        .map(|(_, is_principal, peer)| (peer, is_principal))
        .collect()
}

/// One round of catching up: asks the best sources in turn until one hands
/// over a chain this node adopts. `Ok(true)` when the chain changed.
async fn sync_trust_chain_once(state: &crate::gateway::GatewayState) -> bool {
    publish_chain_version(state);
    if state.network.is_detached() {
        return false;
    }
    for (peer, is_principal) in chain_sources(state)
        .await
        .into_iter()
        .take(TRUST_CHAIN_SOURCES)
    {
        match refresh_trust_chain(state, &peer, is_principal).await {
            Ok(true) => {
                tracing::info!(
                    node = %peer.node_id,
                    version = %state.admin_trust.chain_version(),
                    "administrative chain updated from a peer"
                );
                return true;
            }
            Ok(false) => {}
            Err(error) => tracing::debug!(
                node = %peer.node_id,
                "administrative chain: the peer did not deliver a newer chain: {error:#}"
            ),
        }
    }
    false
}

/// Keeps this node's chain current. A peer that announces a newer version -
/// the principal or any other member, so it works with the principal offline -
/// is asked for its copy. This is what reaches a node that was away when a
/// promotion or demotion was sent around.
pub(crate) fn spawn_trust_chain_keeper(state: crate::gateway::GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(TRUST_CHAIN_CHECK);
        loop {
            ticker.tick().await;
            sync_trust_chain_once(&state).await;
        }
    });
}

/// Asks `peer` for its chain and adopts it when it is newer.
async fn refresh_trust_chain(
    state: &crate::gateway::GatewayState,
    peer: &crate::network::PeerInfo,
    from_principal: bool,
) -> anyhow::Result<bool> {
    let response = state
        .control_post_json(
            peer,
            "/api/internal/admin-trust/chain",
            &json!({ "have": state.admin_trust.chain_version() }),
            SYNC_TIMEOUT,
        )
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("the peer answered {}", response.status());
    }
    let body: serde_json::Value = response.json().await?;
    let Some(document) = body.get("document").filter(|value| !value.is_null()) else {
        return Ok(false);
    };
    let document: AdminTrustDocument = serde_json::from_value(document.clone())?;
    let before = state.admin_trust.chain_version();
    adopt_trust_chain(state, document, from_principal)
        .await
        .map_err(|(_, error)| anyhow::anyhow!(error))?;
    Ok(state.admin_trust.chain_version() != before)
}

/// What the interface shows about this node's copy of the chain.
pub(crate) async fn chain_status(state: &crate::gateway::GatewayState) -> serde_json::Value {
    let sources = chain_sources(state).await;
    json!({
        "version": state.admin_trust.chain_version(),
        "outdated": !sources.is_empty(),
        "newer_on": sources.iter().map(|(peer, _)| peer.hostname.clone()).collect::<Vec<_>>(),
    })
}

/// `POST /api/ui/network/trust-sync`: catch up now instead of waiting for
/// the keeper. Every profile may ask it of its own node.
async fn sync_trust_chain(
    _auth: ConnectionAuth,
    State(state): State<crate::gateway::GatewayState>,
) -> axum::response::Response {
    let updated = sync_trust_chain_once(&state).await;
    let mut status = chain_status(&state).await;
    status["updated"] = json!(updated);
    Json(status).into_response()
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
            Json(json!({ "error": "the offer was not issued by this principal" })),
        )
            .into_response();
    }
    if let Err(error) = state
        .admin_trust
        .complete_transfer(completion.offer, completion.acceptance)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("administrative transfer refused: {error}") })),
        )
            .into_response();
    }
    state
        .network
        .set_policy_principal_pubkey(state.admin_trust.current_primary().pubkey);
    let mut cfg = state.auth.write().await;
    cfg.profile = NodeProfile::Worker;
    if let Err(error) = cfg.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not save the profile: {error}") })),
        )
            .into_response();
    }
    drop(cfg);
    state.network.set_profile(NodeProfile::Worker);
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
            Json(json!({ "error": "only the principal can resign" })),
        )
            .into_response();
    }
    let successor = match state.admin_trust.latest_secondary() {
        Some(successor) => successor,
        None => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "there is no secondary eligible for succession" })),
            )
                .into_response()
        }
    };
    let Some(peer) = state.network.alive_peers().await.into_iter().find(|peer| {
        peer.node_id == successor.node_id
            && peer.libp2p_peer_id.as_deref() == Some(successor.peer_id.as_str())
            && peer.pubkey == successor.pubkey
            && peer.profile == NodeProfile::Admin
    }) else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "the last promoted secondary is not active" })),
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
                Json(json!({ "error": format!("resignation refused: {error}") })),
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
                .unwrap_or_else(|_| "the successor refused the offer".into());
            (status, body).into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("the offer did not reach the successor: {error}") })),
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
            Json(json!({ "error": "principal offer does not exist" })),
        )
            .into_response();
    };
    if offer.offer_id != offer_id || offer.successor != state.authority_principal {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "this offer is not addressed to this peer" })),
        )
            .into_response();
    }
    if state.auth.read().await.profile != NodeProfile::Admin {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "the successor must remain admin until it accepts" })),
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
                Json(json!({ "error": format!("acceptance refused: {error}") })),
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
            && peer.pubkey == offer.issuer.pubkey
    }) else {
        return Json(json!({
            "accepted": true,
            "offer_id": offer.offer_id,
            "synchronized": false,
            "reason": "the previous principal is offline; the accepted chain remains persisted"
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
            "reason": format!("previous principal answered {}", response.status())
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
            Json(json!({ "error": "only the principal can demote a secondary" })),
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
            Json(json!({ "error": "the node is not a secondary admin" })),
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
                Json(json!({ "error": format!("demotion refused: {error}") })),
            )
                .into_response()
        }
    };
    publish_chain_version(&state);
    spawn_trust_fanout(&state, Some(subject.node_id.clone()));
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
            Json(json!({ "error": format!("the secondary refused the signed mutation: {}", response.status()) })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("the demotion did not reach the secondary: {error}") })),
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
        tracing::error!("could not revoke the API keys of the origin: {error}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Err(error) = notify_origin_revoked(&state, &node_id, &state.network.node_id).await {
        tracing::warn!("central revocation of the origin failed: {error:#}");
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
            tracing::error!("could not read the API keys: {error:#}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not read the API keys" })),
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
/// key, which made "a token per node" impossible - the second one silently
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
            tracing::error!("could not store the API key: {error}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "could not store the API key" })),
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
    match node_chat_key(&state).await {
        Ok((key, created)) => {
            let status = if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (status, Json(key)).into_response()
        }
        Err(error) => {
            tracing::warn!("could not prepare the chat key: {error:#}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "could not prepare the chat key" })),
            )
                .into_response()
        }
    }
}

/// The API key this node uses to call its own `/v1`: the interface's Chat one
/// and the one handed to plugins (`host.configure`). It is the only class of
/// key that a node without administrative authority can register on the principal
/// (`is_self_owned_chat_key`), which is why plugins do not get one of their own.
/// Returns the key and whether it was just created.
///
/// The secret stays in the node's local vault (`PomDb::node_api_key`); the record
/// keeps only the hash. If the vault has no secret, or the secret no longer matches
/// an active key (revoked, or a database migrated from before the hash, which
/// lost the text), a new key is issued and the node's old ones are revoked.
pub(crate) async fn node_chat_key(
    state: &crate::gateway::GatewayState,
) -> anyhow::Result<(IssuedApiKey, bool)> {
    let node_id = state.network.node_id.clone();
    let label = format!("pom-{node_id}");
    if let Ok(Some((id, secret))) = state.database.node_api_key() {
        if let Ok(Some(key)) = state.database.api_key_for_token(&secret) {
            let origin = if key.origin_node_id.is_empty() {
                &key.node_id
            } else {
                &key.origin_node_id
            };
            if key.id == id && origin == &node_id && key.label == label {
                return Ok((IssuedApiKey { key, secret }, false));
            }
        }
    }
    let stale = state
        .database
        .api_keys()?
        .into_iter()
        .filter(|key| key.active && key.label == label && key.node_id == node_id)
        .map(|key| key.id)
        .collect::<Vec<_>>();
    let issued = create_key(state, label, node_id.clone()).await?;
    state
        .database
        .save_node_api_key(&issued.key.id, &issued.secret)?;
    for id in stale {
        // The previous key no longer has an owner: nobody knows its secret.
        let _ = state.database.revoke_api_key(&id, &node_id);
        if let Err(error) = notify_key_revoked(state, &id, &node_id).await {
            tracing::warn!("central revocation of the old node key failed: {error:#}");
        }
    }
    let _ = refresh_api_key_snapshot(state).await;
    Ok((issued, true))
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
            tracing::error!("could not revoke the API key: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "could not revoke the API key" })),
            )
                .into_response();
        }
    };
    if let Err(error) = notify_key_revoked(&state, &key, &revoked_by).await {
        tracing::warn!("central revocation of the API key failed: {error:#}");
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

    /// The `/admin` route returns the same offers as `GET /api/transfer/offers`,
    /// not a fixed empty list: a consumer of the administration route has to
    /// see what is waiting for an answer.
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

        let offers = body["offers"].as_array().expect("offers field");
        assert_eq!(offers.len(), 1, "the /admin route cannot return empty");
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
        assert!(secret.starts_with(crate::auth::API_KEY_PREFIX));

        // Once created, the key only shows up through its visible fragment.
        let response = admin_routes()
            .with_state(state.clone())
            .oneshot(
                Request::get("/admin/api-keys")
                    .header("authorization", "Bearer chave")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let listed = crate::gateway::testkit::body_of(response).await;
        assert!(!listed.to_string().contains(&secret));
        assert_eq!(listed[0]["key_hint"], crate::auth::api_key_hint(&secret));
        assert!(listed[0].get("key").is_none());

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
            panic!("unexpected response: {body}");
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

    /// The node's own key: the secret stays in the local vault, the record only
    /// has the hash, and an empty vault (migrated database) swaps the key and revokes the
    /// old one, which nobody would know how to use any more.
    #[tokio::test]
    async fn node_chat_key_keeps_its_secret_local_and_rotates_an_orphan() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.auth.write().await.profile = NodeProfile::Admin;
        let orphan = ApiKey {
            id: "orphan".into(),
            key_hash: crate::auth::api_key_hash("sk-lost-secret"),
            key_hint: crate::auth::api_key_hint("sk-lost-secret"),
            label: "pom-eu".into(),
            node_id: "eu".into(),
            origin_node_id: "eu".into(),
            active: true,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        state.database.insert_api_key(&orphan).unwrap();

        let (first, created) = node_chat_key(&state).await.unwrap();
        assert!(created);
        assert_ne!(first.key.id, "orphan");
        assert_eq!(
            state.database.node_api_key().unwrap(),
            Some((first.key.id.clone(), first.secret.clone()))
        );
        assert!(state
            .database
            .api_key_for_token("sk-lost-secret")
            .unwrap()
            .is_none());
        assert!(state
            .database
            .api_key_for_token(&first.secret)
            .unwrap()
            .is_some());

        let (again, created) = node_chat_key(&state).await.unwrap();
        assert!(!created);
        assert_eq!(again.key.id, first.key.id);
        assert_eq!(again.secret, first.secret);
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
            key_hash: crate::auth::api_key_hash("sk-chat"),
            key_hint: String::new(),
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
            key_hash: crate::auth::api_key_hash("sk-chat"),
            key_hint: String::new(),
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
                .api_key_for_hash(&key.key_hash)
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
            Some(crate::admin_trust::EffectiveAuthority::Admin)
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
                    // Only the hash reaches the principal; the test recognizes it
                    // by the tokens it knows.
                    let token = ["remote-valid", "admin-error", "admin-timeout"]
                        .into_iter()
                        .find(|token| crate::auth::api_key_hash(token) == request.key_hash)
                        .unwrap_or_default();
                    match token {
                        "remote-valid" => (
                            StatusCode::OK,
                            Json(ValidateResponse {
                                valid: true,
                                key: Some(ApiKey {
                                    id: "remote-id".into(),
                                    key_hash: request.key_hash.clone(),
                                    key_hint: String::new(),
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
                .expect("sign test command");
            Request::post(path)
                .header("content-type", "application/json")
                .header(
                    crate::auth::INTERNAL_NODE_REQUEST_HEADER,
                    headers
                        .get(crate::auth::INTERNAL_NODE_REQUEST_HEADER)
                        .expect("internal marker")
                        .to_str()
                        .unwrap(),
                )
                .header(
                    crate::auth::CONTROL_SIGNATURE_HEADER,
                    headers
                        .get(crate::auth::CONTROL_SIGNATURE_HEADER)
                        .expect("internal signature")
                        .to_str()
                        .unwrap(),
                )
                .body(Body::from(body))
                .unwrap()
        };
        let key = ApiKey {
            id: "retry-safe".into(),
            key_hash: crate::auth::api_key_hash("sk-retry-safe"),
            key_hint: String::new(),
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
            .api_key_for_hash(&key.key_hash)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn blocking_an_origin_revokes_local_inference_keys() {
        let state = crate::gateway::testkit::state_with("principal", vec![]).await;
        state.auth.write().await.profile = NodeProfile::Admin;
        let key = ApiKey {
            id: "blocked-origin-key".into(),
            key_hash: crate::auth::api_key_hash("sk-blocked-origin"),
            key_hint: String::new(),
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
            .api_key_for_hash(&key.key_hash)
            .unwrap()
            .is_none());

        let request = Request::get("/v1/models")
            .header("x-api-key", "sk-blocked-origin")
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

    /// A principal with its chain, and the keys of two other machines.
    struct ChainLab {
        principal_key: ed25519_dalek::SigningKey,
        principal: AuthorityPrincipal,
        trust: crate::admin_trust::AdminTrust,
        member: (ed25519_dalek::SigningKey, AuthorityPrincipal),
        other: (ed25519_dalek::SigningKey, AuthorityPrincipal),
    }

    fn chain_lab() -> ChainLab {
        let principal_key = ed25519_dalek::SigningKey::from_bytes(&[21; 32]);
        let principal =
            AuthorityPrincipal::from_signing_key("principal", "peer-principal", &principal_key);
        let trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "chain-lab",
            principal.clone(),
            &principal_key,
        )
        .unwrap();
        let key = |seed: u8, name: &str| {
            let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
            let principal =
                AuthorityPrincipal::from_signing_key(name, format!("peer-{name}"), &key);
            (key, principal)
        };
        ChainLab {
            principal_key,
            principal,
            trust,
            member: key(22, "member"),
            other: key(23, "other"),
        }
    }

    impl ChainLab {
        /// A node of this network holding the chain as it is now.
        async fn node(
            &self,
            identity: &(ed25519_dalek::SigningKey, AuthorityPrincipal),
            profile: NodeProfile,
        ) -> crate::gateway::GatewayState {
            let mut state = crate::gateway::testkit::state_with(&identity.1.node_id, vec![]).await;
            state.auth.write().await.profile = profile;
            state.network.set_profile(profile);
            state.admin_trust = std::sync::Arc::new(
                crate::admin_trust::AdminTrust::in_memory(self.trust.document()).unwrap(),
            );
            state.authority_signing_key = std::sync::Arc::new(identity.0.clone());
            state.authority_principal = identity.1.clone();
            state
        }

        fn promote(&self, subject: &AuthorityPrincipal) {
            let grant = self
                .trust
                .signed_secondary_grant(&self.principal_key, subject.clone(), "principal")
                .unwrap();
            self.trust
                .record_secondary_grant(grant, &self.principal_key)
                .unwrap();
        }
    }

    #[tokio::test]
    async fn every_member_adopts_a_promotion_not_only_the_promoted_node() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lab = chain_lab();
        let promoted = lab.node(&lab.member, NodeProfile::Worker).await;
        let bystander = lab.node(&lab.other, NodeProfile::Worker).await;
        let infer_bystander = lab.node(&lab.other, NodeProfile::Visitor).await;
        // Before the chain arrives, the bystander does not know the admin.
        lab.promote(&lab.member.1);
        assert!(!crate::admin_trust::control_issuer_is_admin(
            &bystander.admin_trust.document(),
            &lab.member.1
        ));

        let chain = lab.trust.document();
        assert_eq!(
            adopt_trust_chain(&promoted, chain.clone(), true)
                .await
                .unwrap(),
            NodeProfile::Admin
        );
        assert_eq!(promoted.network.profile(), NodeProfile::Admin);
        // The bystander keeps its role and now accepts the new admin.
        assert_eq!(
            adopt_trust_chain(&bystander, chain.clone(), false)
                .await
                .unwrap(),
            NodeProfile::Worker
        );
        assert!(crate::admin_trust::control_issuer_is_admin(
            &bystander.admin_trust.document(),
            &lab.member.1
        ));
        assert_eq!(
            adopt_trust_chain(&infer_bystander, chain.clone(), false)
                .await
                .unwrap(),
            NodeProfile::Visitor
        );

        // A demotion travels the same way and takes the role back.
        lab.trust
            .demote_secondary(&lab.principal_key, lab.member.1.clone())
            .unwrap();
        let demoted = lab.trust.document();
        assert_eq!(
            adopt_trust_chain(&promoted, demoted.clone(), true)
                .await
                .unwrap(),
            NodeProfile::Worker
        );
        adopt_trust_chain(&bystander, demoted, false).await.unwrap();
        assert!(!crate::admin_trust::control_issuer_is_admin(
            &bystander.admin_trust.document(),
            &lab.member.1
        ));
        // The copy from before the demotion is older and cannot undo it.
        let (status, _) = adopt_trust_chain(&bystander, chain, false)
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Promoting the same node again is a new grant every member adopts.
        lab.promote(&lab.member.1);
        let again = lab.trust.document();
        assert_eq!(
            adopt_trust_chain(&promoted, again.clone(), true)
                .await
                .unwrap(),
            NodeProfile::Admin
        );
        adopt_trust_chain(&bystander, again, false).await.unwrap();
        assert!(crate::admin_trust::control_issuer_is_admin(
            &bystander.admin_trust.document(),
            &lab.member.1
        ));
    }

    #[tokio::test]
    async fn a_grant_never_raises_an_inference_node() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lab = chain_lab();
        let infer = lab.node(&lab.member, NodeProfile::Visitor).await;
        lab.promote(&lab.member.1);
        assert_eq!(
            adopt_trust_chain(&infer, lab.trust.document(), false)
                .await
                .unwrap(),
            NodeProfile::Visitor
        );
        assert_eq!(infer.auth.read().await.profile, NodeProfile::Visitor);
    }

    #[tokio::test]
    async fn the_principal_refuses_to_promote_an_inference_node_before_signing() {
        let lab = chain_lab();
        let mut peer = crate::gateway::testkit::peer("member", "member", 1);
        peer.profile = NodeProfile::Visitor;
        peer.pubkey = lab.member.1.pubkey.clone();
        peer.libp2p_peer_id = Some(lab.member.1.peer_id.clone());
        let mut state = crate::gateway::testkit::state_with("principal", vec![peer]).await;
        state.auth.write().await.profile = NodeProfile::Admin;
        state.admin_trust = std::sync::Arc::new(
            crate::admin_trust::AdminTrust::in_memory(lab.trust.document()).unwrap(),
        );
        state.authority_signing_key = std::sync::Arc::new(lab.principal_key.clone());
        state.authority_principal = lab.principal.clone();
        let before = state.admin_trust.chain_version();

        let response =
            promote_node_to_admin(ConnectionAuth, State(state.clone()), Path("member".into()))
                .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            crate::gateway::testkit::body_of(response).await["reason"],
            "visitor_target"
        );
        // Nothing was signed: the chain is exactly as it was.
        assert_eq!(state.admin_trust.chain_version(), before);
    }

    #[tokio::test]
    async fn a_member_is_only_sent_the_chain_when_its_copy_differs() {
        let lab = chain_lab();
        let principal = lab
            .node(
                &(lab.principal_key.clone(), lab.principal.clone()),
                NodeProfile::Admin,
            )
            .await;
        let ask = |have: String| {
            let principal = principal.clone();
            async move {
                crate::gateway::testkit::body_of(
                    internal_trust_chain(
                        InternalNodeAuth { principal: None },
                        State(principal),
                        Json(TrustChainQuery { have }),
                    )
                    .await,
                )
                .await
            }
        };
        let current = ask(principal.admin_trust.chain_version()).await;
        assert_eq!(current["current"], true);
        assert!(current.get("document").is_none());
        let stale = ask("0.0.0.0-old".into()).await;
        assert_eq!(stale["current"], false);
        assert_eq!(stale["document"]["primary"]["node_id"], "principal");
    }

    #[tokio::test]
    async fn a_secondary_admin_is_not_the_principal() {
        let lab = chain_lab();
        lab.promote(&lab.member.1);
        let secondary = lab.node(&lab.member, NodeProfile::Admin).await;
        // With no principal named yet, the profile decides (single-node case).
        assert!(secondary.network.is_principal());
        secondary
            .network
            .set_policy_principal_pubkey(lab.principal.pubkey.clone());
        assert!(!secondary.network.is_principal());
    }

    #[tokio::test]
    async fn a_node_catches_up_from_any_member_ahead_of_it() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lab = chain_lab();
        // A node that was away holds the chain from before the promotion.
        let mut away = lab.node(&lab.other, NodeProfile::Worker).await;
        let old = away.admin_trust.chain_version();
        lab.promote(&lab.member.1);
        let new = lab.trust.chain_version();
        assert_ne!(old, new);

        // The principal is offline; another member announces the new version.
        let announcing = |version: &str| {
            let mut peer = crate::gateway::testkit::peer("member", "member", 1);
            peer.pubkey = lab.member.1.pubkey.clone();
            peer.trust_version = Some(version.to_string());
            peer
        };
        let seen = |peer| async {
            let mut state = crate::gateway::testkit::state_with("other", vec![peer]).await;
            state.admin_trust = away.admin_trust.clone();
            state.authority_principal = lab.other.1.clone();
            state
        };
        let state = seen(announcing(&new)).await;
        let status = chain_status(&state).await;
        assert_eq!(status["outdated"], true);
        assert_eq!(status["newer_on"][0], "member");
        // A peer at the same version, or behind, is nobody to ask.
        assert_eq!(
            chain_status(&seen(announcing(&old)).await).await["outdated"],
            false
        );
        assert_eq!(
            chain_status(&seen(announcing("not-a-version")).await).await["outdated"],
            false
        );

        // Its copy is adopted although it does not come from the principal,
        // because every signature in it is the principal's.
        away.auth.write().await.profile = NodeProfile::Worker;
        adopt_trust_chain(&away, lab.trust.document(), false)
            .await
            .unwrap();
        assert_eq!(away.admin_trust.chain_version(), new);
        assert_eq!(away.network.trust_version().as_deref(), Some(new.as_str()));

        // A forged copy is refused whoever sends it.
        let mut forged = lab.trust.document();
        forged.grants.last_mut().unwrap().subject = lab.other.1.clone();
        assert!(adopt_trust_chain(&away, forged, true).await.is_err());
    }

    #[test]
    fn two_copies_with_equal_counters_are_settled_by_the_principal_only() {
        let lab = chain_lab();
        let member = crate::admin_trust::AdminTrust::in_memory(lab.trust.document()).unwrap();
        // The principal grants the same seat twice, in two histories: the
        // counters match, the content does not.
        let fork = crate::admin_trust::AdminTrust::in_memory(lab.trust.document()).unwrap();
        for (trust, subject) in [(&lab.trust, &lab.member.1), (&fork, &lab.other.1)] {
            let grant = trust
                .signed_secondary_grant(&lab.principal_key, subject.clone(), "principal")
                .unwrap();
            trust
                .record_secondary_grant(grant, &lab.principal_key)
                .unwrap();
        }
        assert_ne!(lab.trust.chain_version(), fork.chain_version());
        assert_eq!(
            crate::admin_trust::version_counters(&lab.trust.chain_version()),
            crate::admin_trust::version_counters(&fork.chain_version())
        );

        assert!(member.reconcile_from(lab.trust.document(), false).unwrap());
        // The same copy again is a no-op.
        assert!(!member.reconcile_from(lab.trust.document(), false).unwrap());
        // The other history: refused from a member, accepted from the principal.
        assert!(member.reconcile_from(fork.document(), false).is_err());
        assert!(member.reconcile_from(fork.document(), true).unwrap());
        assert_eq!(member.chain_version(), fork.chain_version());
    }
}
