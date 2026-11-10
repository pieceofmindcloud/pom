//! Member gateway: a visitor hands its requests to a member entry node
//! (`docs/superpowers/specs/2026-11-07-member-gateway-planning-design.md`),
//! which plans and runs them and, when the network charges visitors, charges
//! the visitor's home network for them
//! (`docs/superpowers/specs/2026-11-09-visitor-charging-design.md`).
//!
//! - **On the visitor** ([`visitor_layer`]): under `inference_access =
//!   member_gateway*`, a request to `/v1/chat/completions`, `/v1/completions`,
//!   `/v1/messages` or `/api/generate`, once the client's own API key is
//!   checked here (keys never leave the node that minted them), goes to an entry
//!   node's `POST /v1/gateway/delegate` with the visitor's home statement; the
//!   answer (status, content type, streamed body) comes back unchanged. Entry
//!   nodes are members announcing `gateway/delegate-v1` that the network's
//!   `entry_nodes` allows, by measured RTT. A request moves to the next entry
//!   node only when it got no answer. With none: `503 no_entry_node` under
//!   `member_gateway_only`; under `member_gateway` the visitor plans itself.
//! - **On the entry node** ([`delegate_route`]): only a signed call from a
//!   visitor (a member that is not a visitor plans itself: `409
//!   plan_locally`). When the network charges, the home statement names the
//!   account (`net:<home principal key>`) and the payment gate holds the
//!   request's worst case before it runs: `402 payment_required` (the x402
//!   `accepts` envelope) when the account cannot cover it, `503
//!   payment_unavailable` when the book cannot be read. The request then runs
//!   through this node's own router, marked as delegated (an in-process
//!   extension no outside request can carry), so every route answers as for a
//!   local client; the generation settles at its end ([`PAYMENT`]).
//! - **On a worker** ([`refuse_visitor`]): under `member_gateway_only`, a
//!   visitor's call on the inference port is refused (`visitor_must_use_gateway`).

use crate::config::{EntryNodes, InferenceAccess};
use crate::gateway::GatewayState;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;
use tower::ServiceExt;

pub(crate) const DELEGATE_PATH: &str = "/v1/gateway/delegate";
const ROUTES: [&str; 4] = ["/v1/chat/completions", "/v1/completions", "/v1/messages", "/api/generate"];
/// A delegated generation may run as long as a local one.
const DELEGATE_TIMEOUT: Duration = Duration::from_secs(1800);
const MAX_BODY: usize = 32 << 20;
/// The output ceiling assumed when a request names none (`openai::DEFAULT_MAX_TOKENS`).
const DEFAULT_MAX_TOKENS: i64 = 512;

/// The marker of a request an entry node runs for a visitor. Set in process
/// by [`delegate_route`]; `ApiKeyAuth` takes its caller instead of a key.
#[derive(Clone, Debug)]
pub(crate) struct Delegated {
    pub(crate) caller: crate::reqlog::Caller,
}

/// What a delegated, charged generation settles at its end.
#[derive(Clone, Debug)]
pub(crate) struct Charge {
    pub(crate) account: String,
    pub(crate) hold_micro: i64,
    pub(crate) pricing: crate::payments::DeploymentPricing,
}

tokio::task_local! {
    /// The charge of the generation running on this task
    /// (`gateway::spawn_logged_generation_with_stops` takes it into the
    /// generation's task and settles it at the summary).
    pub(crate) static PAYMENT: Option<Charge>;
}

pub(crate) fn current_charge() -> Option<Charge> {
    PAYMENT.try_with(|c| c.clone()).ok().flatten()
}

static ROUTER: OnceLock<axum::Router> = OnceLock::new();

/// The node's whole router, for [`delegate_route`] to run a visitor's request
/// through. Set once, where the gateway builds it.
pub(crate) fn install_router(router: axum::Router) {
    let _ = ROUTER.set(router);
}

fn refuse(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "error": { "code": code, "message": message } }))).into_response()
}

// ── Visitor ─────────────────────────────────────────────────────────────────

/// Entry nodes for this visitor: members announcing delegation that the
/// policy allows, nearest first.
pub(crate) async fn entry_nodes(state: &GatewayState, allowed: EntryNodes) -> Vec<crate::network::PeerInfo> {
    let document = state.admin_trust.document();
    let rtt = crate::route_plan::book().rtt_map(crate::route_plan::Plane::Gateway);
    let mut peers: Vec<_> = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .filter(|p| p.node_id != state.network.node_id)
        .filter(|p| p.inference_frame_capabilities.iter().any(|c| c == crate::pipeline::CAP_GATEWAY_DELEGATE))
        .filter(|p| !crate::gateway::peer_is_visitor(state, p))
        .filter(|p| match allowed {
            EntryNodes::Admins => crate::admin_trust::is_admin_node(&document, &p.node_id),
            EntryNodes::AdminsAndWorkers => true,
        })
        .collect();
    peers.sort_by(|a, b| {
        let ra = rtt.get(&a.node_id).copied().unwrap_or(f64::MAX);
        let rb = rtt.get(&b.node_id).copied().unwrap_or(f64::MAX);
        ra.total_cmp(&rb)
    });
    peers
}

/// The visitor's side, a middleware on the whole router.
pub(crate) async fn visitor_layer(State(state): State<GatewayState>, request: Request, next: Next) -> Response {
    if request.method() != Method::POST
        || !ROUTES.contains(&request.uri().path())
        || request.extensions().get::<Delegated>().is_some()
        || !state.local_profile_is_visitor()
    {
        return next.run(request).await;
    }
    let settings = crate::payments::runtime::settings(&state).await;
    if !settings.inference_access.delegates() {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    // The client's key is this node's: checked here, never sent on.
    if let Err(rejection) = <crate::auth::ApiKeyAuth as axum::extract::FromRequestParts<GatewayState>>::from_request_parts(&mut parts, &state).await {
        return rejection.into_response();
    }
    let bytes = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => return refuse(StatusCode::PAYLOAD_TOO_LARGE, "body_too_large", "the request body is too large"),
    };
    let entries = entry_nodes(&state, settings.entry_nodes).await;
    if entries.is_empty() {
        if settings.inference_access == InferenceAccess::MemberGateway {
            return next.run(Request::from_parts(parts, Body::from(bytes))).await;
        }
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "no_entry_node", "no entry node of the network is in sight");
    }
    let Ok(client_body) = serde_json::from_slice::<Value>(&bytes) else {
        return refuse(StatusCode::BAD_REQUEST, "invalid_json", "the request body is not JSON");
    };
    let payload = json!({
        "route": parts.uri.path(),
        "body": client_body,
        "home": crate::home_statement::own(&state),
    });
    let model = payload["body"]["model"].as_str().unwrap_or_default().to_string();
    let mut tried = Vec::new();
    for peer in entries {
        match state.control_post_json(&peer, DELEGATE_PATH, &payload, DELEGATE_TIMEOUT).await {
            Ok(answer) => return relay(answer, Some((state.clone(), model))),
            Err(error) => tried.push(json!({ "node": peer.node_id, "error": error.to_string() })),
        }
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": { "code": "no_entry_node", "message": "no entry node answered", "tried": tried } })),
    )
        .into_response()
}

/// The entry node's answer as the visitor's client gets it. With `spend`,
/// the charge the answer reports (`x_pom.payment`, at its end) is recorded on
/// this node once the body is through.
fn relay(answer: reqwest::Response, spend: Option<(GatewayState, String)>) -> Response {
    let status = StatusCode::from_u16(answer.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = answer
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(tap_payment(answer.bytes_stream(), spend)))
        .unwrap_or_else(|_| refuse(StatusCode::BAD_GATEWAY, "relay", "the entry node's answer could not be relayed"))
}

/// Passes the body through, keeping its tail; at the end, records the
/// payment the tail reports.
fn tap_payment<S>(stream: S, spend: Option<(GatewayState, String)>) -> impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>>
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    const TAIL: usize = 64 << 10;
    futures::stream::unfold((stream, Vec::<u8>::new(), spend), |(mut stream, mut tail, spend)| async move {
        use futures::StreamExt;
        match stream.next().await {
            Some(Ok(chunk)) => {
                tail.extend_from_slice(&chunk);
                if tail.len() > TAIL {
                    tail.drain(..tail.len() - TAIL);
                }
                Some((Ok(chunk), (stream, tail, spend)))
            }
            Some(Err(error)) => Some((Err(error), (stream, tail, spend))),
            None => {
                if let (Some((state, model)), Some(payment)) = (spend, last_payment(&tail)) {
                    record_spend(&state, &model, &payment);
                }
                None
            }
        }
    })
}

/// The last `"payment":{...}` object in `text`.
pub(crate) fn last_payment(text: &[u8]) -> Option<Value> {
    let text = String::from_utf8_lossy(text);
    let start = text.rfind("\"payment\":{")? + "\"payment\":".len();
    let mut depth = 0usize;
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&text[start..=start + i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

fn record_spend(state: &GatewayState, model: &str, payment: &Value) {
    let (Some(account), Some(charged)) = (payment["account"].as_str(), payment["charged_micro"].as_i64()) else {
        return;
    };
    let host = state.admin_trust.document().primary.node_id;
    let result = state.database.with_write("record a visitor's spend", |c| {
        c.execute(
            "INSERT INTO x402_spend (host_network, account, model, charged_micro, balance_micro, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![host, account, model, charged, payment["balance_micro"].as_i64(), chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    });
    if let Err(error) = result {
        tracing::warn!("could not record what this visitor spent: {error:#}");
    }
}

// ── Entry node ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct DelegateRequest {
    route: String,
    body: Value,
    #[serde(default)]
    home: Option<String>,
}

/// The worst case of a request, for the hold: the prompt estimated from the
/// body (four characters a token, as the gateway estimates) and the ceiling
/// the client asked for.
pub(crate) fn request_size(body: &Value) -> (i64, i64) {
    let text_len = match body.get("messages") {
        Some(messages) => messages.to_string().len(),
        None => body.get("prompt").map(|p| p.to_string().len()).unwrap_or(0),
    } + body.get("system").map(|s| s.to_string().len()).unwrap_or(0);
    let prompt = (text_len as i64 / 4).max(1);
    let ceiling = ["max_completion_tokens", "max_tokens"]
        .iter()
        .find_map(|k| body.get(*k).and_then(Value::as_i64))
        .or_else(|| body.get("options").and_then(|o| o.get("num_predict")).and_then(Value::as_i64))
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    (prompt, ceiling)
}

/// The price of `model` in the network's settings, if charged.
pub(crate) fn price_of(settings: &crate::config::NetworkSettings, model: &str) -> Option<crate::payments::DeploymentPricing> {
    // `<model>@<deploy_id>` picks a deployment of the same model: same price.
    let price = settings.payments.prices.get(crate::deployment::split_selector(model).0)?;
    Some(crate::payments::DeploymentPricing {
        price_in_micro_per_1k: price.in_micro_per_1k,
        price_out_micro_per_1k: price.out_micro_per_1k,
        min_funding_micro: 0,
    })
}

/// `POST /v1/gateway/delegate`.
pub(crate) async fn delegate_route(auth: crate::auth::InternalNodeAuth, State(state): State<GatewayState>, Json(ask): Json<DelegateRequest>) -> Response {
    let Some(principal) = auth.principal() else {
        return refuse(StatusCode::FORBIDDEN, "signed_call_required", "a signed peer call is required");
    };
    let caller_node = principal.node_id.clone();
    let peer = state.network.alive_peers().await.into_iter().find(|p| p.node_id == caller_node);
    if !peer.as_ref().is_some_and(|p| crate::gateway::peer_is_visitor(&state, p)) {
        return refuse(StatusCode::CONFLICT, "plan_locally", "only a visitor delegates; a member plans its own requests");
    }
    if !ROUTES.contains(&ask.route.as_str()) {
        return refuse(StatusCode::BAD_REQUEST, "unknown_route", "this route is not delegated");
    }
    let settings = crate::payments::runtime::settings(&state).await;
    let home = ask
        .home
        .as_deref()
        .and_then(|token| crate::home_statement::verify(token, &caller_node, chrono::Utc::now().timestamp()).ok());
    let charge = if settings.payments.enabled {
        let model = ask.body.get("model").and_then(Value::as_str).unwrap_or_default();
        match price_of(&settings, model) {
            None => None,
            Some(pricing) => {
                let Some(home) = home.as_ref() else {
                    return speak(
                        &ask.route,
                        StatusCode::FORBIDDEN,
                        "home_statement_required",
                        "this network charges visitors: the request must name the visitor's home network",
                        json!({}),
                    );
                };
                match gate(&state, &crate::home_statement::account_of(home), &pricing, &ask.body).await {
                    Ok(charge) => Some(charge),
                    Err((status, code, message, extra)) => return speak(&ask.route, status, code, &message, extra),
                }
            }
        }
    } else {
        None
    };
    let caller = crate::reqlog::Caller {
        key_id: format!("visitor:{caller_node}"),
        key_label: format!("visitor {}", &caller_node[..12.min(caller_node.len())]),
        key_node_id: Some(caller_node.clone()),
        ..crate::reqlog::Caller::anonymous()
    };
    let Some(router) = ROUTER.get().cloned() else {
        if let Some(charge) = &charge {
            crate::payments::runtime::release(&charge.account, charge.hold_micro);
        }
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "not_ready", "the entry node is starting");
    };
    let mut request = match Request::builder()
        .method(Method::POST)
        .uri(&ask.route)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&ask.body).unwrap_or_default()))
    {
        Ok(request) => request,
        Err(_) => return refuse(StatusCode::BAD_REQUEST, "bad_request", "the delegated request is malformed"),
    };
    request.extensions_mut().insert(Delegated { caller });
    let held = charge.clone();
    let answer = PAYMENT.scope(charge, router.oneshot(request)).await;
    match answer {
        Ok(response) => {
            // A refusal before the generation started (budget, quota, no
            // pipeline) never reached the settlement: give the hold back.
            if !response.status().is_success() {
                if let Some(charge) = held {
                    crate::payments::runtime::release(&charge.account, charge.hold_micro);
                }
            }
            response
        }
        Err(never) => match never {},
    }
}

/// A refusal in the vocabulary of the route the client called, with
/// `extra`'s fields (the x402 `accepts` envelope, the account) beside it.
pub(crate) fn speak(route: &str, status: StatusCode, code: &str, message: &str, extra: Value) -> Response {
    let mut body = match route {
        "/v1/messages" => json!({ "type": "error", "error": { "type": code, "message": message } }),
        "/api/generate" => json!({ "error": message, "code": code }),
        _ => json!({ "error": { "message": message, "type": code, "code": code } }),
    };
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            body.insert(k.clone(), v.clone());
        }
    }
    (status, Json(body)).into_response()
}

/// Holds the request's worst case on `account`, asking the principal for a
/// lease once when this node's book cannot cover it.
async fn gate(
    state: &GatewayState,
    account: &str,
    pricing: &crate::payments::DeploymentPricing,
    body: &Value,
) -> Result<Charge, (StatusCode, &'static str, String, Value)> {
    let (prompt, ceiling) = request_size(body);
    let payments = crate::payments::runtime::state();
    let attempt = || payments.gate(account, Some(pricing), prompt, ceiling);
    let mut result = attempt();
    if matches!(result, Err(crate::payments::GateError::Challenge(_))) && !crate::payments::runtime::is_principal(state) {
        let want = crate::payments::hold_micro(pricing, prompt, ceiling).saturating_mul(4);
        if crate::payments::runtime::lease(state, account, want).await > 0 {
            result = attempt();
        }
    }
    match result {
        Ok(reservation) => Ok(Charge {
            account: account.to_string(),
            hold_micro: reservation.hold_micro,
            pricing: pricing.clone(),
        }),
        Err(crate::payments::GateError::Challenge(challenge)) => {
            let mut accepts = challenge.body["accepts"].clone();
            accepts[0]["payTo"] = json!(format!("pk:{}", state.admin_trust.document().primary.node_id));
            let lacking = accepts[0]["maxAmountRequired"].as_i64().unwrap_or(0)
                - accepts[0]["metadata"]["balance_micro"].as_i64().unwrap_or(0);
            Err((
                StatusCode::PAYMENT_REQUIRED,
                "payment_required",
                format!("the account {account} lacks {lacking} micro-credits for this request; ask the network's admin for credit"),
                json!({ "accepts": accepts, "account": account}),
            ))
        }
        Err(crate::payments::GateError::Unavailable) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "payment_unavailable",
            "the balance could not be read".into(),
            json!({}),
        )),
    }
}

/// The settlement of a delegated generation at its summary (called from
/// `gateway::spawn_logged_generation_with_stops`): the executed prompt minus
/// what the cache spared, and the generated tokens.
pub(crate) fn settle(state: &GatewayState, charge: &Charge, summary: &crate::gateway::GenSummary) -> Value {
    let (prompt, cached) = summary
        .trace
        .as_ref()
        .map(|t| (t.input_tokens as i64, t.cached_tokens as i64))
        .unwrap_or((0, 0));
    let bill = crate::payments::bill_micro(&charge.pricing, prompt, cached, summary.tokens_generated as i64);
    // The metering bundle lines the charge up with the payouts of its cycle.
    let detail = summary
        .metering_bundle
        .as_ref()
        .map(|(bid, cycle)| json!({ "bundle": bid, "cycle": cycle }).to_string());
    crate::payments::runtime::settle(state, &charge.account, charge.hold_micro, bill, detail);
    json!({
        "account": charge.account,
        "charged_micro": bill,
        "balance_micro": crate::payments::runtime::state().book().balance(&charge.account),
    })
}

// ── Worker ──────────────────────────────────────────────────────────────────

/// Under `member_gateway_only`, whether `caller` (a peer calling the
/// inference port or the tunnel) is a visitor that must use an entry node.
pub(crate) async fn refuse_visitor(state: &GatewayState, caller: &str) -> bool {
    let settings = crate::payments::runtime::settings(state).await;
    if settings.inference_access != InferenceAccess::MemberGatewayOnly {
        return false;
    }
    state
        .network
        .alive_peers()
        .await
        .iter()
        .find(|p| p.node_id == caller)
        .is_some_and(|p| crate::gateway::peer_is_visitor(state, p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::testkit;

    fn key(seed: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
    }

    fn id_of(k: &ed25519_dalek::SigningKey) -> String {
        metering_core::node_id_of_pubkey_hex(&hex::encode(k.verifying_key().as_bytes())).unwrap()
    }

    fn signed(k: &ed25519_dalek::SigningKey) -> crate::auth::InternalNodeAuth {
        crate::auth::InternalNodeAuth {
            principal: Some(crate::admin_trust::AuthorityPrincipal {
                node_id: id_of(k),
                peer_id: String::new(),
                pubkey: hex::encode(k.verifying_key().as_bytes()),
            }),
        }
    }

    /// A home statement the visitor signs for its own single-machine network.
    fn home(k: &ed25519_dalek::SigningKey) -> String {
        let key_hex = hex::encode(k.verifying_key().as_bytes());
        let claims = crate::home_statement::HomeClaims {
            node: id_of(k),
            node_key: key_hex.clone(),
            net: "home".into(),
            principal: id_of(k),
            principal_key: key_hex,
            history: vec![],
            iat: 0,
            exp: chrono::Utc::now().timestamp() + 3600,
        };
        metering_core::token::sign(&format!(r#"{{"alg":"EdDSA","typ":"{}-home-network/v1"}}"#, crate::brand::slug()), &claims, k).unwrap()
    }

    async fn entry_state(visitor: &ed25519_dalek::SigningKey, charge: bool) -> GatewayState {
        let mut peer = testkit::peer(&id_of(visitor), "visitor", 9);
        peer.profile = crate::profile::NodeProfile::Visitor;
        let state = testkit::state_with("entry", vec![peer]).await;
        let mut settings = crate::config::NetworkSettings::default();
        settings.inference_access = InferenceAccess::MemberGatewayOnly;
        settings.payments.enabled = charge;
        settings.payments.prices.insert(
            "priced".into(),
            crate::config::ModelPrice { in_micro_per_1k: 1_000_000, out_micro_per_1k: 2_000_000 },
        );
        state.database.save_network_settings(&settings).unwrap();
        state
    }

    fn ask(model: &str, home: Option<String>) -> DelegateRequest {
        DelegateRequest {
            route: "/v1/chat/completions".into(),
            body: json!({"model": model, "messages": [{"role": "user", "content": "hello"}], "max_tokens": 10}),
            home,
        }
    }

    async fn code_of(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        (status, testkit::body_of(response).await)
    }

    #[tokio::test]
    async fn only_a_signed_visitor_delegates() {
        let visitor = key(41);
        let state = entry_state(&visitor, false).await;
        let unsigned = delegate_route(crate::auth::InternalNodeAuth { principal: None }, State(state.clone()), Json(ask("m", None))).await;
        assert_eq!(unsigned.status(), StatusCode::FORBIDDEN);
        let member = key(42);
        let (status, body) = code_of(delegate_route(signed(&member), State(state.clone()), Json(ask("m", None))).await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "plan_locally");
    }

    #[tokio::test]
    async fn a_charged_model_needs_the_home_network() {
        let visitor = key(43);
        let state = entry_state(&visitor, true).await;
        let (status, body) = code_of(delegate_route(signed(&visitor), State(state), Json(ask("priced", None))).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "home_statement_required");
        assert_eq!(body["error"]["type"], "home_statement_required");
    }

    #[tokio::test]
    async fn an_unfunded_network_gets_a_402_and_a_funded_one_is_admitted() {
        let visitor = key(44);
        let state = entry_state(&visitor, true).await;
        // This test node is the principal of its own chain: the merchant.
        let account = crate::payments::network_account(&hex::encode(visitor.verifying_key().as_bytes()));
        let (status, body) =
            code_of(delegate_route(signed(&visitor), State(state.clone()), Json(ask("priced", Some(home(&visitor))))).await).await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{body}");
        assert_eq!(body["account"], account);
        assert_eq!(body["accepts"][0]["scheme"], "pom-credit");
        assert_eq!(body["error"]["code"], "payment_required");
        // Each route answers in its own vocabulary.
        let mut messages = ask("priced", Some(home(&visitor)));
        messages.route = "/v1/messages".into();
        let (_, anthropic) = code_of(delegate_route(signed(&visitor), State(state.clone()), Json(messages)).await).await;
        assert_eq!((anthropic["type"].as_str(), anthropic["error"]["type"].as_str()), (Some("error"), Some("payment_required")));
        crate::payments::runtime::state().grant(&account, 1_000_000_000);
        let answer = delegate_route(signed(&visitor), State(state.clone()), Json(ask("priced", Some(home(&visitor))))).await;
        // Admitted by the gate; with no router in this test the request
        // cannot run, and the hold is given back.
        assert_ne!(answer.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(crate::payments::runtime::state().book().reserved(&account), 0);
        // A free model is not gated at all.
        let free = delegate_route(signed(&visitor), State(state), Json(ask("free", Some(home(&visitor))))).await;
        assert_ne!(free.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[test]
    fn the_hold_reads_the_prompt_and_the_ceiling_the_client_asked_for() {
        let (p, c) = request_size(&json!({"messages": [{"role": "user", "content": "x".repeat(400)}], "max_tokens": 64}));
        assert!(p >= 100);
        assert_eq!(c, 64);
        let (_, c) = request_size(&json!({"prompt": "hi"}));
        assert_eq!(c, DEFAULT_MAX_TOKENS);
        let (_, c) = request_size(&json!({"prompt": "hi", "options": {"num_predict": 9}}));
        assert_eq!(c, 9);
        let (_, c) = request_size(&json!({"messages": [], "max_completion_tokens": 7, "max_tokens": 100}));
        assert_eq!(c, 7);
    }

    #[test]
    fn the_visitor_reads_the_charge_at_the_end_of_the_answer() {
        let sse = br#"data: {"choices":[]}

data: {"x_pom":{"payment":{"account":"net:a","charged_micro":12,"balance_micro":88},"legs":1}}

data: [DONE]"#;
        assert_eq!(last_payment(sse).unwrap()["charged_micro"], 12);
        assert!(last_payment(br#"{"x_pom":{"payment":null}}"#).is_none());
    }

    /// An admission request with a home statement names the account; the
    /// approval opens it with the credit typed in, and the Payments screen
    /// lists the visitor under it.
    #[tokio::test]
    async fn an_approval_opens_the_visitors_account_with_its_credit() {
        let visitor = key(45);
        let state = testkit::state_with("principal-node", vec![]).await;
        let id = id_of(&visitor);
        state
            .network_chat
            .request_admission(&id, Default::default(), crate::config::EnrollmentRole::Visitor)
            .unwrap();
        // Another node's statement names nothing.
        crate::home_statement::note_admission(&state.network_chat, &id, Some(&home(&key(46))));
        assert!(state.network_chat.admission_of(&id).unwrap().account.is_none());
        crate::home_statement::note_admission(&state.network_chat, &id, Some(&home(&visitor)));
        let account = crate::payments::network_account(&hex::encode(visitor.verifying_key().as_bytes()));
        assert_eq!(state.network_chat.admission_of(&id).unwrap().account.as_deref(), Some(account.as_str()));

        let decision = crate::webui::AdmissionDecision { node_id: id.clone(), decision: "approve".into(), grant_micro: Some(5_000) };
        let response = crate::webui::apply_decision(&state, decision).await;
        assert!(response.status().is_success());
        assert_eq!(crate::payments::runtime::state().book().balance(&account), 5_000);
        let overview = testkit::body_of(crate::payments::runtime::overview(State(state)).await).await;
        let listed = overview["accounts"].as_array().unwrap().iter().find(|a| a["account"] == account.as_str()).cloned().unwrap();
        assert_eq!(listed["visitors"][0]["node_id"], id.as_str());
    }

    #[test]
    fn a_deployment_selector_pays_its_models_price() {
        let mut settings = crate::config::NetworkSettings::default();
        settings.payments.prices.insert("m".into(), crate::config::ModelPrice { in_micro_per_1k: 1, out_micro_per_1k: 2 });
        assert!(price_of(&settings, "m@d-0123abcd").is_some());
    }

    #[test]
    fn only_priced_models_are_charged() {
        let mut settings = crate::config::NetworkSettings::default();
        settings.payments.prices.insert(
            "m".into(),
            crate::config::ModelPrice { in_micro_per_1k: 10, out_micro_per_1k: 20 },
        );
        assert_eq!(price_of(&settings, "m").unwrap().price_out_micro_per_1k, 20);
        assert!(price_of(&settings, "free").is_none());
    }
}
