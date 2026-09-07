use crate::auth::{AuthState, ConnectionAuth, InternalNodeAuth};
use crate::config::{ApiKey, Role};
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
        .route("/admin/nodes/{id}/block", post(block_node))
        .route("/admin/nodes/{id}/unblock", post(unblock_node))
        .route("/admin/nodes/{id}", delete(remove_node))
        .route("/admin/regenerate-key", post(regenerate_key))
        .route("/admin/api-keys", get(list_api_keys).post(generate_api_key))
        .route("/admin/api-keys/:key", delete(revoke_api_key))
        .route("/admin/transfer-offers", get(transfer_offers))
        .route("/admin/slots", get(list_slots))
        .route("/admin/slots/:name", post(slot_action))
        .merge(internal_routes())
        .route("/api/ui/chat-key", post(chat_key))
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

async fn principal_peer_for(network: &crate::network::Network) -> Option<crate::network::PeerInfo> {
    network.alive_peers().await.into_iter().find(|peer| {
        peer.node_id != network.node_id
            && peer.role.eq_ignore_ascii_case("principal")
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

async fn sync_json<T: Serialize>(
    state: &crate::gateway::GatewayState,
    path: &str,
    body: &T,
) -> anyhow::Result<()> {
    let peer = principal_peer_for(&state.network)
        .await
        .ok_or_else(|| anyhow::anyhow!("central admin node is unavailable"))?;
    let urls = gateway_urls(&peer);
    let client = state.peer_http.for_peer(&peer.node_id)?;
    let connection_key = state.auth.read().await.connection_key.clone();
    if connection_key.is_empty() {
        anyhow::bail!("node-to-node trust is not configured");
    }
    let mut last_error = "central admin synchronization failed".to_string();
    for attempt in 0..SYNC_ATTEMPTS {
        for base in &urls {
            let result = client
                .post(format!("{base}{path}"))
                .timeout(SYNC_TIMEOUT)
                .bearer_auth(&connection_key)
                .header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, "1")
                .json(body)
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
    if state.auth.read().await.role == Role::Principal {
        return Ok(());
    }
    sync_json(state, "/api/internal/api-keys/upsert", key).await
}

async fn notify_key_revoked(
    state: &crate::gateway::GatewayState,
    key_or_id: &str,
    revoked_by: &str,
) -> anyhow::Result<()> {
    if state.auth.read().await.role == Role::Principal {
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
    if state.auth.read().await.role == Role::Principal {
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
    validate_api_key_remote_parts(&state.network, &state.peer_http, &state.auth, token).await
}

pub(crate) async fn validate_api_key_remote_parts(
    network: &crate::network::Network,
    peer_http: &crate::peer_client::PeerClients,
    auth: &AuthState,
    token: &str,
) -> anyhow::Result<Option<ApiKey>> {
    let peer = principal_peer_for(network)
        .await
        .ok_or_else(|| anyhow::anyhow!("central admin node is unavailable"))?;
    let client = peer_http.for_peer(&peer.node_id)?;
    let connection_key = auth.read().await.connection_key.clone();
    if connection_key.is_empty() {
        anyhow::bail!("node-to-node trust is not configured");
    }
    let body = ValidateRequest {
        token: token.to_string(),
    };
    for attempt in 0..SYNC_ATTEMPTS {
        for base in gateway_urls(&peer) {
            let result = client
                .post(format!("{base}/api/internal/api-keys/validate"))
                .timeout(SYNC_TIMEOUT)
                .bearer_auth(&connection_key)
                .header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, "1")
                .json(&body)
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
}

async fn require_principal(state: &crate::gateway::GatewayState) -> Result<(), StatusCode> {
    (state.auth.read().await.role == Role::Principal)
        .then_some(())
        .ok_or(StatusCode::FORBIDDEN)
}

async fn internal_upsert_api_key(
    _auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(key): Json<ApiKey>,
) -> impl IntoResponse {
    if require_principal(&state).await.is_err() || key.key.is_empty() || key.id.is_empty() {
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
    _auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(request): Json<RevokeRequest>,
) -> impl IntoResponse {
    if require_principal(&state).await.is_err() {
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
    _auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(request): Json<RevokeOriginRequest>,
) -> impl IntoResponse {
    if require_principal(&state).await.is_err() {
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
    _auth: InternalNodeAuth,
    State(state): State<crate::gateway::GatewayState>,
    Json(request): Json<ValidateRequest>,
) -> impl IntoResponse {
    if require_principal(&state).await.is_err() {
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
                "role": p.role,
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

    Json(nodes)
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

async fn chat_key(
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
        state.auth.write().await.role = Role::Principal;
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
        peer.role = "principal".into();
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
    async fn internal_key_transitions_are_authenticated_and_idempotent() {
        let state = crate::gateway::testkit::state_with("principal", vec![]).await;
        state.auth.write().await.role = Role::Principal;
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
                .oneshot(
                    Request::post("/api/internal/api-keys/upsert")
                        .header("authorization", "Bearer chave")
                        .header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, "1")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&key).unwrap()))
                        .unwrap(),
                )
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
                .oneshot(
                    Request::post("/api/internal/api-keys/revoke")
                        .header("authorization", "Bearer chave")
                        .header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, "1")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::to_vec(&RevokeRequest {
                                key_or_id: key.id.clone(),
                                revoked_by: "principal".into(),
                            })
                            .unwrap(),
                        ))
                        .unwrap(),
                )
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
        state.auth.write().await.role = Role::Principal;
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
}
