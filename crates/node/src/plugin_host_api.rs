//! The loopback API a running plugin calls back into its own node with.
//!
//! The node reaches a plugin through its `query`; until now nothing went the
//! other way. A plugin that takes part in the network needs a little of the
//! node: who this machine is, which machines are alive, a call to its own
//! plugin on another machine, and a signature by the node key over one kind
//! of statement. `host.configure` hands each plugin host its own secret:
//!
//! ```json
//! {"host_api": {"base_url": "http://127.0.0.1:8080/api/plugin-host", "token": "<hex>"}}
//! ```
//!
//! Every route takes it in `x-pom-plugin-host-token`. The token names the
//! plugin, so a plugin only ever speaks as itself: a peer call reaches the
//! same `plugin_code` on the other machine, through the signed peer route
//! (`crate::plugin_services`), never another plugin.
//!
//! - `GET /api/plugin-host/node`: this node's `node_id`, name, Ed25519 public key
//!   (hex and `did:key`), its role in the trust chain and the principal's
//!   `node_id`.
//! - `GET /api/plugin-host/peers`: the live peers, with whether each is the
//!   principal or a visitor.
//! - `POST /api/plugin-host/peer`: `{node_id, op, params}` runs `peer.<op>` of
//!   this plugin on that machine and answers `{reply}`.
//! - `GET /api/plugin-host/catalog/images`: this node's image model files.
//! - `POST /api/plugin-host/catalog/refresh`: rescan this node's catalog.
//! - `GET /api/plugin-host/components` and
//!   `POST /api/plugin-host/component/:deploy_id/:component/:op`: the image
//!   model components of the network, and an op run on the machine serving one.
//! - `POST /api/plugin-host/image-job/begin` and `.../complete`: a ComfyUI
//!   generation on network components, checked against the image budget and
//!   recorded as one request.
//! - `GET /api/plugin-host/image-deployments`, `GET .../image-files`,
//!   `POST .../image-deploy` and `POST .../image-undeploy`: the image deploy
//!   screen's list, survey, deploy and undeploy, only on a principal or admin
//!   node (403 otherwise), with the node's own authority.
//! - `POST /api/plugin-host/sign`: `{statement}` is signed with the node key
//!   only when it is an external identity statement in which this node's
//!   `did:key` vouches for a key (see [`signable_statement`]). The node key
//!   also signs announces, control envelopes, grants and the pkarr record, so
//!   it never signs bytes it did not recognize.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::time::Duration;

use crate::gateway::GatewayState;

/// Header that carries the plugin's own secret from `host.configure`.
pub(crate) const HOST_API_TOKEN_HEADER: &str = "x-pom-plugin-host-token";
/// Path the routes hang under; `host.configure` hands it over as `base_url`.
pub(crate) const HOST_API_PREFIX: &str = "/api/plugin-host";
/// A peer call waits this long for the other machine's plugin.
const PEER_CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// The longest statement the node signs.
const MAX_STATEMENT: usize = 1024;
/// The external identity statements the node signs, by their fixed opening.
/// `{did}` is replaced by this node's `did:key`: the statement can only say
/// that this node vouches for something, never that another identity does.
/// Ghostly's WISP 300 binding statement, with WISP 311's `did` provider.
const IDENTITY_STATEMENTS: &[&str] =
    &["Ghostly identity proof v1: I control did:{did} and authorize the Ghostly key "];

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

/// The plugin the request speaks for, from its token.
fn caller_code(state: &GatewayState, headers: &HeaderMap) -> Result<String, Response> {
    let token = headers
        .get(HOST_API_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    state
        .plugins
        .code_for_host_api_token(token)
        .ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "unknown plugin host token"))
}

/// `did:key` of an Ed25519 public key: multibase base58btc of the multicodec
/// prefix `0xed 0x01` and the key.
pub(crate) fn ed25519_did_key(public_key: &[u8; 32]) -> String {
    let mut bytes = Vec::with_capacity(34);
    bytes.extend_from_slice(&[0xed, 0x01]);
    bytes.extend_from_slice(public_key);
    format!("did:key:z{}", base58btc(&bytes))
}

fn base58btc(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let zeros = bytes.iter().take_while(|byte| **byte == 0).count();
    // Little-endian digits in base 58.
    let mut digits: Vec<u8> = Vec::with_capacity(bytes.len() * 2);
    for byte in &bytes[zeros..] {
        let mut carry = u32::from(*byte);
        for digit in &mut digits {
            carry += u32::from(*digit) << 8;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let mut encoded = String::with_capacity(zeros + digits.len());
    encoded.extend(std::iter::repeat('1').take(zeros));
    encoded.extend(
        digits
            .iter()
            .rev()
            .map(|digit| ALPHABET[*digit as usize] as char),
    );
    encoded
}

/// Whether the node signs `statement`: one line of printable ASCII, short, and
/// opening with one of [`IDENTITY_STATEMENTS`] for this node's own `did:key`.
/// None of the node's own signed messages is printable text that opens like
/// this, so a signature made here cannot stand for one of them.
pub(crate) fn signable_statement(statement: &str, did_key: &str) -> bool {
    statement.len() <= MAX_STATEMENT
        && statement.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        && IDENTITY_STATEMENTS.iter().any(|opening| {
            let opening = opening.replace("{did}", did_key);
            statement.len() > opening.len() && statement.starts_with(&opening)
        })
}

fn node_public_key(state: &GatewayState) -> [u8; 32] {
    state.authority_signing_key.verifying_key().to_bytes()
}

/// `GET /api/plugin-host/node`.
pub(crate) async fn node(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if let Err(response) = caller_code(&state, &headers) {
        return response;
    }
    let public_key = node_public_key(&state);
    let document = state.admin_trust.document();
    Json(json!({
        "node_id": state.network.node_id,
        "hostname": state.network.hostname(),
        "public_key": hex::encode(public_key),
        "did_key": ed25519_did_key(&public_key),
        "role": crate::plugin_services::local_role(&state).await,
        "principal_node_id": document.primary.node_id,
    }))
    .into_response()
}

/// `GET /api/plugin-host/catalog/images`: the image model files of this
/// node's whole-model folders. Read fresh on every call (headers are cached by
/// path, size and mtime), so a file a plugin just downloaded is in the answer.
pub(crate) async fn catalog_images(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = caller_code(&state, &headers) {
        return response;
    }
    let dirs = state.model_dirs().await;
    let files = tokio::task::spawn_blocking(move || {
        crate::image_catalog::discover_image_files(&crate::catalog::image_roots(&dirs))
    })
    .await
    .unwrap_or_default();
    Json(json!({ "image_files": files, "metadata_complete": true })).into_response()
}

/// `POST /api/plugin-host/catalog/refresh`: rescan the catalog now (after a
/// plugin wrote a model file), so the screens and the deploy survey see it
/// without waiting for the cache to turn stale.
pub(crate) async fn catalog_refresh(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = caller_code(&state, &headers) {
        return response;
    }
    crate::webui::invalidate_catalog(state.database.clone()).await;
    Json(json!({ "ok": true })).into_response()
}

/// The plugin's call, when this node may deploy: a plugin acts with the
/// node's own authority, so only a principal or admin node deploys.
async fn admin_caller(state: &GatewayState, headers: &HeaderMap) -> Result<String, Response> {
    let code = caller_code(state, headers)?;
    match crate::plugin_services::local_role(state).await {
        "principal" | "admin" => Ok(code),
        _ => Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "only an admin node deploys" })),
        )
            .into_response()),
    }
}

/// `GET /api/plugin-host/image-deployments`: what the deploy screen lists.
pub(crate) async fn image_deployments(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = admin_caller(&state, &headers).await {
        return response;
    }
    Json(crate::components::image_deployments_view(&state).await).into_response()
}

/// `GET /api/plugin-host/image-files`: the network's image files per machine.
pub(crate) async fn image_files(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if let Err(response) = admin_caller(&state, &headers).await {
        return response;
    }
    Json(crate::components::image_files_view(&state).await).into_response()
}

/// `POST /api/plugin-host/image-deploy`: the deploy screen's deploy.
pub(crate) async fn image_deploy(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<crate::image_plan::DeployBody>,
) -> Response {
    if let Err(response) = admin_caller(&state, &headers).await {
        return response;
    }
    crate::components::deploy_response(&state, request).await
}

/// `POST /api/plugin-host/image-undeploy`: the deploy screen's undeploy.
pub(crate) async fn image_undeploy(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<crate::components::ReleaseRequest>,
) -> Response {
    if let Err(response) = admin_caller(&state, &headers).await {
        return response;
    }
    Json(crate::components::undeploy_view(&state, &request.deploy_id).await).into_response()
}

/// `GET /api/plugin-host/components`: the image model components served on
/// the network now.
pub(crate) async fn components(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if let Err(response) = caller_code(&state, &headers) {
        return response;
    }
    let components = crate::components::network_components(&state).await;
    Json(json!({ "components": components })).into_response()
}

/// `POST /api/plugin-host/component/:deploy_id/:component/:op`: run an op of
/// a network component on the machine that serves it; the body is opaque.
pub(crate) async fn component_call(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::extract::Path((deploy_id, component, op)): axum::extract::Path<(String, String, String)>,
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = caller_code(&state, &headers) {
        return response;
    }
    let job = headers
        .get("x-pom-job")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let network = crate::components::components_for_call(&state, &deploy_id, &component).await;
    let forward = crate::components::live_forward(state.clone());
    let bytes_in = body.len() as u64;
    let started = std::time::Instant::now();
    // The RTT the Flow map shows, measured from this node; this node is 0.
    let measured = state.network.rtts.fresh_snapshot();
    let local = state.network.node_id.clone();
    let rtt_ms = move |node_id: &str| {
        if node_id == local {
            return Some(0.0);
        }
        measured
            .iter()
            .find(|m| m.target_node_id == node_id)
            .map(|m| m.rtt_ms)
    };
    let (status, bytes, target) = crate::components::call_component(
        &network,
        &deploy_id,
        &component,
        &op,
        body.to_vec(),
        job.clone(),
        &forward,
        &rtt_ms,
    )
    .await;
    // The generation's telemetry: which machine ran what, for how long.
    if let (Some(job), Some(target)) = (job, target) {
        crate::components::image_jobs().record(
            &job,
            crate::components::JobCall {
                component: target.component,
                deploy_id: target.deploy_id,
                name: target.name,
                node_id: target.node_id,
                hostname: target.hostname,
                ms: started.elapsed().as_millis() as u64,
                bytes_in,
                bytes_out: bytes.len() as u64,
            },
        );
    }
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        bytes,
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct ImageJobBegin {
    job_id: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    details: Value,
}

/// `POST /api/plugin-host/image-job/begin`: a ComfyUI generation that uses
/// network components starts. The image budget is checked first: a refusal is
/// 429 and recorded like a refused `/v1` request; otherwise one audit record
/// opens for the whole generation.
pub(crate) async fn image_job_begin(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<ImageJobBegin>,
) -> Response {
    let code = match caller_code(&state, &headers) {
        Ok(code) => code,
        Err(response) => return response,
    };
    if request.job_id.is_empty() || request.job_id.len() > 128 {
        return refuse(StatusCode::BAD_REQUEST, "invalid job id");
    }
    crate::components::spawn_job_sweeper(state.clone());
    let jobs = crate::components::image_jobs();
    let caller = crate::components::image_caller(&state, &code);
    let mut details = if request.details.is_object() {
        request.details.clone()
    } else {
        json!({})
    };
    details["kind"] = json!("image");
    if details.get("images").and_then(Value::as_u64).is_none() {
        details["images"] = json!(1);
    }
    let denial = if jobs.is_open(&request.job_id) {
        None
    } else {
        let tokens = (request.prompt.len() / 4) as u32;
        crate::quota::admit(&state, &caller, &request.prompt, tokens, &details).await
    };
    let images = details["images"].as_u64().unwrap_or(1);
    match jobs.begin(&request.job_id, denial, std::time::Instant::now(), images) {
        Ok(audit_id) => {
            state.audit.begin(crate::reqlog::AuditRequest {
                id: audit_id.clone(),
                received_at: chrono::Utc::now().to_rfc3339(),
                route: "/comfyui/image".into(),
                model: request.model,
                caller,
                entry_node_id: state.network.node_id.clone(),
                prompt: request.prompt,
                request_details: details,
            });
            Json(json!({ "ok": true })).into_response()
        }
        Err(denial) => {
            crate::gateway::record_rejected(
                &state,
                "/comfyui/image",
                caller,
                request.model,
                request.prompt,
                details,
                denial.message.clone(),
            );
            denial.decorate(
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({ "error": denial.message, "retry_after_secs": denial.retry_after_secs })),
                )
                    .into_response(),
            )
        }
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct ImageJobComplete {
    job_id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    images: u64,
    width: Option<u64>,
    height: Option<u64>,
    steps: Option<u64>,
}

/// `POST /api/plugin-host/image-job/complete`: the generation ended.
pub(crate) async fn image_job_complete(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(request): Json<ImageJobComplete>,
) -> Response {
    if let Err(response) = caller_code(&state, &headers) {
        return response;
    }
    let Some(job) = crate::components::image_jobs().take(&request.job_id) else {
        return refuse(StatusCode::NOT_FOUND, "unknown generation");
    };
    let status = match request.status.as_str() {
        "ok" => crate::reqlog::AuditStatus::Ok,
        "interrupted" => crate::reqlog::AuditStatus::Incomplete,
        _ => crate::reqlog::AuditStatus::Error,
    };
    crate::components::complete_image_job(
        &state,
        job,
        status,
        request.error,
        request.images,
        request.width,
        request.height,
        request.steps,
    );
    Json(json!({ "ok": true })).into_response()
}

/// `GET /api/plugin-host/peers`.
pub(crate) async fn peers(State(state): State<GatewayState>, headers: HeaderMap) -> Response {
    if let Err(response) = caller_code(&state, &headers) {
        return response;
    }
    let principal = state.admin_trust.document().primary.node_id;
    let peers: Vec<Value> = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .filter(|peer| peer.node_id != state.network.node_id)
        .map(|peer| {
            json!({
                "node_id": peer.node_id,
                "hostname": peer.hostname,
                "principal": peer.node_id == principal,
                "visitor": crate::gateway::peer_is_visitor(&state, &peer),
            })
        })
        .collect();
    Json(json!({ "peers": peers })).into_response()
}

/// `POST /api/plugin-host/peer`.
pub(crate) async fn peer_call(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let code = match caller_code(&state, &headers) {
        Ok(code) => code,
        Err(response) => return response,
    };
    let (Some(node_id), Some(op)) = (body["node_id"].as_str(), body["op"].as_str()) else {
        return refuse(StatusCode::BAD_REQUEST, "node_id and op are required");
    };
    if !crate::plugin_services::valid_peer_op(op) {
        return refuse(StatusCode::BAD_REQUEST, "invalid operation");
    }
    let params = body.get("params").cloned().unwrap_or(Value::Null);
    if serde_json::to_vec(&params).map_or(true, |bytes| {
        bytes.len() > crate::plugin_services::MAX_PEER_BODY
    }) {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "params too large");
    }
    let reply = if node_id == state.network.node_id && crate::metering::is_node_op(&code, op) {
        // The ledger asking its own node (`crate::metering::node_op`).
        let role = crate::plugin_services::local_role(&state).await;
        let mut params = params.clone();
        if let Some(object) = params.as_object_mut() {
            object.insert("local".into(), serde_json::Value::Bool(true));
        } else {
            params = json!({ "local": true });
        }
        Some(match crate::metering::node_op(&state, role, op, &params).await {
            Ok(reply) => reply,
            Err(error) => json!({ "error": error }),
        })
    } else if node_id == state.network.node_id {
        let caller = json!({
            "node_id": state.network.node_id,
            "head": false,
            "local": true,
            "role": crate::plugin_services::local_role(&state).await,
        });
        state
            .plugins
            .query_audit(
                &code,
                &format!("peer.{op}"),
                json!({ "params": params, "caller": caller }),
            )
            .await
    } else {
        let Some(peer) = state
            .network
            .alive_peers()
            .await
            .into_iter()
            .find(|peer| peer.node_id == node_id)
        else {
            return refuse(StatusCode::NOT_FOUND, "the node is not a live peer");
        };
        let path = format!("/api/internal/plugins/{code}/peer/{op}");
        match state
            .control_post_json_routed(&peer, &path, &params, PEER_CALL_TIMEOUT)
            .await
        {
            Ok(response) if response.status().is_success() => response.json().await.ok(),
            _ => None,
        }
    };
    match reply {
        Some(reply) => Json(json!({ "reply": reply })).into_response(),
        None => refuse(
            StatusCode::BAD_GATEWAY,
            "plugin unavailable or the operation was refused",
        ),
    }
}

/// `POST /api/plugin-host/sign`.
pub(crate) async fn sign(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    use ed25519_dalek::Signer;
    let code = match caller_code(&state, &headers) {
        Ok(code) => code,
        Err(response) => return response,
    };
    let public_key = node_public_key(&state);
    let did_key = ed25519_did_key(&public_key);
    let Some(statement) = body["statement"].as_str() else {
        return refuse(StatusCode::BAD_REQUEST, "statement is required");
    };
    if !signable_statement(statement, &did_key) {
        return refuse(
            StatusCode::FORBIDDEN,
            "the node signs only an identity statement for its own did:key",
        );
    }
    let signature = state.authority_signing_key.sign(statement.as_bytes());
    tracing::info!(plugin_code = %code, "node key signed an identity statement for a plugin");
    Json(json!({
        "signature": hex::encode(signature.to_bytes()),
        "public_key": hex::encode(public_key),
        "did_key": did_key,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A node whose plugin manager knows `ghostly`, and that plugin's token.
    async fn state_with_plugin() -> (GatewayState, String) {
        let mut state = crate::gateway::testkit::state_with("node", vec![]).await;
        let service = Arc::new(crate::plugins::tests_support::MockLicenseService::new());
        state.plugins = crate::plugins::PluginManager::test_manager(&["ghostly"], service);
        let token = state.plugins.host_api_token_for_test("ghostly");
        (state, token)
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(HOST_API_TOKEN_HEADER, token.parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn every_route_needs_the_plugin_token() {
        let (state, token) = state_with_plugin().await;
        assert_eq!(
            state.plugins.code_for_host_api_token(&token).as_deref(),
            Some("ghostly")
        );
        assert!(state.plugins.code_for_host_api_token("").is_none());
        for headers in [HeaderMap::new(), headers("not-the-token")] {
            let response = node(State(state.clone()), headers.clone()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = sign(
                State(state.clone()),
                headers,
                Json(json!({"statement": "x"})),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        for headers in [HeaderMap::new(), headers("not-the-token")] {
            let response = catalog_images(State(state.clone()), headers.clone()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = catalog_refresh(State(state.clone()), headers.clone()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = components(State(state.clone()), headers.clone()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = image_job_complete(
                State(state.clone()),
                headers.clone(),
                Json(serde_json::from_value(json!({"job_id": "p"})).unwrap()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let response = component_call(
                State(state.clone()),
                headers,
                axum::extract::Path(("img-1".to_owned(), "vae".to_owned(), "decode".to_owned())),
                axum::body::Bytes::new(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = node(State(state.clone()), headers(&token)).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn with_profile(state: &GatewayState, profile: crate::profile::NodeProfile) {
        state.auth.write().await.profile = profile;
    }

    #[tokio::test]
    async fn a_non_admin_node_cannot_deploy_from_a_plugin() {
        let (state, token) = state_with_plugin().await;
        with_profile(&state, crate::profile::NodeProfile::Worker).await;
        assert_eq!(crate::plugin_services::local_role(&state).await, "member");
        let request = || {
            Json(
                serde_json::from_value(json!({"name": "Z", "components": [
                    {"component": "vae", "node_id": "n", "path": "/m/ae.safetensors"}
                ]}))
                .unwrap(),
            )
        };
        let responses = [
            image_deployments(State(state.clone()), headers(&token)).await,
            image_files(State(state.clone()), headers(&token)).await,
            image_deploy(State(state.clone()), headers(&token), request()).await,
            image_undeploy(
                State(state.clone()),
                headers(&token),
                Json(serde_json::from_value(json!({"deploy_id": "img-1"})).unwrap()),
            )
            .await,
        ];
        for response in responses {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let body = crate::gateway::testkit::body_of(response).await;
            assert_eq!(body["error"], "only an admin node deploys");
        }
        // Without the plugin token nothing answers at all.
        let response = image_deployments(State(state.clone()), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_plugin_on_an_admin_node_deploys() {
        let (state, token) = state_with_plugin().await;
        with_profile(&state, crate::profile::NodeProfile::Admin).await;
        assert_eq!(
            crate::plugin_services::local_role(&state).await,
            "principal"
        );
        let response = image_deployments(State(state.clone()), headers(&token)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["deployments"], json!([]));
        let response = image_files(State(state.clone()), headers(&token)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert!(body["machines"].is_array(), "{body}");
        // The deploy itself runs: its own checks answer, not the role gate.
        let response = image_deploy(
            State(state.clone()),
            headers(&token),
            Json(
                serde_json::from_value(json!({"name": "Z", "components": [
                    {"component": "vae", "node_id": "elsewhere", "path": "/m/ae.safetensors"}
                ]}))
                .unwrap(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["error"], "elsewhere is not on the network");
        let response = image_undeploy(
            State(state),
            headers(&token),
            Json(serde_json::from_value(json!({"deploy_id": "img-unknown"})).unwrap()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_image_catalog_lists_the_node_files() {
        let (state, token) = state_with_plugin().await;
        let models = tempfile::tempdir().unwrap();
        crate::image_catalog::write_test_safetensors(
            models.path(),
            "pub/repo/ae.safetensors",
            &crate::image_catalog::test_fixture("flux_ae_vae"),
        );
        state.auth.write().await.model_dirs =
            Some(vec![models.path().to_string_lossy().into_owned()]);
        let response = catalog_images(State(state.clone()), headers(&token)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = crate::gateway::testkit::body_of(response).await;
        assert_eq!(body["image_files"][0]["name"], "pub/repo/ae.safetensors");
        assert_eq!(body["image_files"][0]["category"], "vae");
        let response = catalog_refresh(State(state), headers(&token)).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_node_signs_only_its_own_identity_statement() {
        use ed25519_dalek::Verifier;
        let (state, token) = state_with_plugin().await;
        let public_key = node_public_key(&state);
        let did = ed25519_did_key(&public_key);
        let statement = format!(
            "Ghostly identity proof v1: I control did:{did} and authorize the Ghostly key k \
             to present it to contacts I choose. Nonce: n"
        );
        let refused = sign(
            State(state.clone()),
            headers(&token),
            Json(json!({"statement": statement.replace(&did, "did:key:z6Mkother")})),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);

        let response = sign(
            State(state.clone()),
            headers(&token),
            Json(json!({"statement": statement})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let signature: [u8; 64] = hex::decode(body["signature"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        ed25519_dalek::VerifyingKey::from_bytes(&public_key)
            .unwrap()
            .verify(
                statement.as_bytes(),
                &ed25519_dalek::Signature::from_bytes(&signature),
            )
            .expect("the node key signed the statement");
        assert_eq!(body["did_key"], did);
    }

    #[test]
    fn did_key_matches_the_w3c_ed25519_vector() {
        // did:key test vector of the W3C CCG did:key spec (Ed25519 2020).
        let public_key: [u8; 32] =
            hex::decode("2e6fcce36701dc791488e0d0b1745cc1e33a4c1c9fcc41c63bd343dbbe0970e6")
                .unwrap()
                .try_into()
                .unwrap();
        assert_eq!(
            ed25519_did_key(&public_key),
            "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK"
        );
    }

    #[test]
    fn base58_keeps_leading_zeros() {
        assert_eq!(base58btc(&[0, 0, 1]), "112");
        assert_eq!(base58btc(&[]), "");
    }

    #[test]
    fn only_an_identity_statement_for_this_node_is_signable() {
        let did = "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK";
        let statement = format!(
            "Ghostly identity proof v1: I control did:{did} and authorize the Ghostly key \
             ybndrfg8ejkmcpqxot1uwisza345h769ybndrfg8ejkmcpqxot1u to present it to contacts I \
             choose, from 2026-10-19T00:00:00Z until 2027-10-19T00:00:00Z. Nonce: AAAAAAAAAAAAAAAAAAAAAA"
        );
        assert!(signable_statement(&statement, did));
        // Another identity, another provider, nothing after the opening.
        let other = statement.replace("z6Mkha", "z6Mkhb");
        assert!(!signable_statement(&other, did));
        assert!(!signable_statement(
            &statement.replace("did:did:", "ssh:did:"),
            did
        ));
        let opening = IDENTITY_STATEMENTS[0].replace("{did}", did);
        assert!(!signable_statement(&opening, did));
        // One line of printable ASCII, and short.
        assert!(!signable_statement(&format!("{statement}\n"), did));
        assert!(!signable_statement(&format!("{statement}é"), did));
        assert!(!signable_statement(
            &format!("{statement}{}", "x".repeat(MAX_STATEMENT)),
            did
        ));
        // Bytes the node signs for itself never qualify.
        assert!(!signable_statement("{\"node_id\":\"abc\"}", did));
    }
}
