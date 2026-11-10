//! The payment state of this node and the principal's lease protocol
//! (`docs/superpowers/specs/2026-11-09-visitor-charging-design.md`, 4-6).
//!
//! - **The principal** is the merchant: its book holds every account's
//!   balance, its `LedgerWriter` the only authoritative ledger.
//! - **Another entry node** admits against **leases**: an amount the principal
//!   reserves on an account for it. The entry node mirrors the lease as that
//!   account's balance in its own book, spends it locally, and reports each
//!   settlement; the principal then deducts it (`settle`). When a lease does
//!   not cover a request the entry node asks for more, once, before it refuses.
//!   The principal away: no lease, refusal (fail closed).
//!
//! Leases live in memory on both sides. A principal that restarts rebuilds its
//! book from settled balances (nothing reserved), so outstanding leases may be
//! spent once more than they were reserved for, at most one lease per entry
//! node and account; an entry node that restarts loses its lease, which stays
//! reserved on the principal until the principal restarts (it admits less,
//! never more).

use super::{LedgerWriter, PaymentState, PaymentWrite};
use crate::gateway::GatewayState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Smallest lease an entry node asks for (one credit).
pub(crate) const MIN_LEASE_MICRO: i64 = 1_000_000;
const PEER_TIMEOUT: Duration = Duration::from_secs(3);

static STATE: OnceLock<Arc<PaymentState>> = OnceLock::new();
static LEDGER: OnceLock<Arc<LedgerWriter>> = OnceLock::new();

/// This node's book. Always present: whether anything is charged is the
/// network's `payments.enabled`, read at each request.
pub(crate) fn state() -> Arc<PaymentState> {
    STATE
        .get_or_init(|| Arc::new(PaymentState::new(true, String::new())))
        .clone()
}

pub(crate) fn ledger() -> Option<Arc<LedgerWriter>> {
    LEDGER.get().cloned()
}

/// At startup: the ledger writer, and on the principal the balances.
pub(crate) fn install(state: &GatewayState) {
    let _ = LEDGER.set(Arc::new(LedgerWriter::new(state.database.clone())));
    if is_principal(state) {
        if let Err(error) = super::ledger::load_balances(&state.database, self::state().book()) {
            tracing::warn!("payments: loading balances failed: {error}");
        }
    }
    // What visitors may learn of the network follows the access policy.
    let state = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(5));
        loop {
            ticker.tick().await;
            let settings = settings(&state).await;
            let view = (settings.inference_access == crate::config::InferenceAccess::MemberGatewayOnly)
                .then_some(settings.entry_nodes == crate::config::EntryNodes::Admins);
            state.network.set_visitor_view(view);
        }
    });
}

pub(crate) fn is_principal(state: &GatewayState) -> bool {
    state.admin_trust.document().primary.node_id == state.network.node_id
}

pub(crate) async fn settings(state: &GatewayState) -> crate::config::NetworkSettings {
    let configured = state.auth.read().await.network.clone();
    state
        .database
        .network_settings()
        .ok()
        .flatten()
        .unwrap_or(configured)
}

/// Settles a request on this node and, off the principal, reports it.
pub(crate) fn settle(state: &GatewayState, account: &str, hold: i64, bill: i64, detail: Option<String>) {
    self::state().settle(account, hold, bill);
    if is_principal(state) {
        if let Some(ledger) = ledger() {
            ledger.enqueue(PaymentWrite::settle(account, bill, None, detail));
        }
        return;
    }
    let (state, account) = (state.clone(), account.to_string());
    tokio::spawn(async move {
        let body = json!({ "account": account, "bill_micro": bill, "detail": detail });
        if let Err(error) = to_principal(&state, "/api/internal/payments/settle", &body).await {
            tracing::warn!("payments: reporting a settlement to the principal failed: {error}");
        }
    });
}

pub(crate) fn release(account: &str, hold: i64) {
    state().release(account, hold);
}

async fn to_principal(state: &GatewayState, path: &str, body: &Value) -> Result<Value, String> {
    let principal = state.admin_trust.document().primary.node_id;
    let peer = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .find(|p| p.node_id == principal)
        .ok_or("the principal is not in sight")?;
    let response = state
        .control_post_json_routed(&peer, path, body, PEER_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("the principal answered {}", response.status()));
    }
    response.json().await.map_err(|e| e.to_string())
}

/// Asks the principal for a lease of at least `want` on `account`; mirrors
/// what it grants in this node's book. Returns the amount granted.
pub(crate) async fn lease(state: &GatewayState, account: &str, want: i64) -> i64 {
    let want = want.max(MIN_LEASE_MICRO);
    match to_principal(state, "/api/internal/payments/lease", &json!({ "account": account, "want_micro": want })).await {
        Ok(answer) => {
            let granted = answer["granted_micro"].as_i64().unwrap_or(0).max(0);
            if granted > 0 {
                self::state().grant(account, granted);
            }
            granted
        }
        Err(error) => {
            tracing::warn!("payments: no lease from the principal: {error}");
            0
        }
    }
}

/// The account's balance as the principal holds it (or this node's, on the
/// principal).
pub(crate) async fn balance(state: &GatewayState, account: &str) -> Option<i64> {
    if is_principal(state) {
        return Some(self::state().book().balance(account));
    }
    to_principal(state, "/api/internal/payments/balance", &json!({ "account": account }))
        .await
        .ok()?["balance_micro"]
        .as_i64()
}

/// The principal's merchant answers to another node: only members, signed.
async fn merchant_caller(state: &GatewayState, auth: &crate::auth::InternalNodeAuth) -> Result<(), Response> {
    if !is_principal(state) {
        return Err((StatusCode::CONFLICT, Json(json!({"error": "this node is not the principal"}))).into_response());
    }
    let Some(principal) = auth.principal() else {
        return Err((StatusCode::FORBIDDEN, Json(json!({"error": "a signed peer call is required"}))).into_response());
    };
    let peers = state.network.alive_peers().await;
    let visitor = peers
        .iter()
        .find(|p| p.node_id == principal.node_id)
        .map(|p| crate::gateway::peer_is_visitor(state, p))
        .unwrap_or(true);
    if visitor {
        return Err((StatusCode::FORBIDDEN, Json(json!({"error": "only a member may hold a lease"}))).into_response());
    }
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct LeaseAsk {
    account: String,
    want_micro: i64,
}

/// `POST /api/internal/payments/lease`: reserve up to `want` of the account's
/// free balance for the calling entry node.
pub(crate) async fn lease_route(auth: crate::auth::InternalNodeAuth, State(state): State<GatewayState>, Json(ask): Json<LeaseAsk>) -> Response {
    if let Err(refusal) = merchant_caller(&state, &auth).await {
        return refusal;
    }
    Json(json!({ "granted_micro": grant_lease(&self::state(), &ask.account, ask.want_micro) })).into_response()
}

/// What a lease of `want` on `account` reserves: the free balance, capped.
pub(crate) fn grant_lease(payments: &PaymentState, account: &str, want: i64) -> i64 {
    let book = payments.book();
    let free = book.balance(account).saturating_sub(book.reserved(account)).max(0);
    let granted = want.max(0).min(free);
    if granted > 0 && book.try_reserve(account, granted) != Some(true) {
        return 0;
    }
    granted
}

#[derive(Deserialize)]
pub(crate) struct SettleReport {
    account: String,
    bill_micro: i64,
    #[serde(default)]
    detail: Option<String>,
}

/// `POST /api/internal/payments/settle`: an entry node spent `bill` of its
/// lease on `account`.
pub(crate) async fn settle_route(auth: crate::auth::InternalNodeAuth, State(state): State<GatewayState>, Json(report): Json<SettleReport>) -> Response {
    if let Err(refusal) = merchant_caller(&state, &auth).await {
        return refusal;
    }
    let bill = report.bill_micro.max(0);
    self::state().settle(&report.account, bill, bill);
    if let Some(ledger) = ledger() {
        ledger.enqueue(PaymentWrite::settle(&report.account, bill, None, report.detail));
    }
    Json(json!({ "settled": true })).into_response()
}

#[derive(Deserialize)]
pub(crate) struct BalanceAsk {
    account: String,
}

/// `POST /api/internal/payments/balance`.
pub(crate) async fn balance_route(auth: crate::auth::InternalNodeAuth, State(state): State<GatewayState>, Json(ask): Json<BalanceAsk>) -> Response {
    if auth.principal().is_none() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "a signed peer call is required"}))).into_response();
    }
    if !is_principal(&state) {
        return (StatusCode::CONFLICT, Json(json!({"error": "this node is not the principal"}))).into_response();
    }
    Json(json!({ "balance_micro": self::state().book().balance(&ask.account) })).into_response()
}

/// Opens `account` on the principal at an admission, with an optional first
/// grant: a grant of zero still lists the account.
pub(crate) fn open_account(account: &str, grant_micro: i64, node_id: &str) {
    self::state().grant(account, grant_micro.max(0));
    if grant_micro > 0 {
        if let Some(ledger) = ledger() {
            let detail = json!({ "admission": node_id }).to_string();
            ledger.enqueue(PaymentWrite::grant(account, grant_micro, Some(detail)));
        }
    }
}

// ── Prices from the deployments ───────────────────────────────────────────

/// `prices` with `model` set to `price` (zero in and out: removed). `None`
/// when nothing changes.
pub(crate) fn with_price(
    prices: &std::collections::BTreeMap<String, crate::config::ModelPrice>,
    model: &str,
    price: crate::config::ModelPrice,
) -> Option<std::collections::BTreeMap<String, crate::config::ModelPrice>> {
    let mut next = prices.clone();
    if price.in_micro_per_1k <= 0 && price.out_micro_per_1k <= 0 {
        next.remove(model);
    } else {
        next.insert(model.to_string(), price);
    }
    (next != *prices).then_some(next)
}

/// Writes a deployment's price into the network's prices, which every entry
/// node reads: on the principal directly, from another admin through it.
pub(crate) async fn mirror_price(state: &GatewayState, model: &str, price: crate::config::ModelPrice) {
    if is_principal(state) {
        let mut settings = settings(state).await;
        if let Some(prices) = with_price(&settings.payments.prices, model, price) {
            settings.payments.prices = prices;
            if let Err(error) = crate::webui::network::apply_network_settings(state, settings).await {
                tracing::warn!("could not record the price of {model}: {error}");
            }
        }
        return;
    }
    let body = json!({ "model": model, "price": price });
    if let Err(error) = to_principal(state, "/api/internal/payments/price", &body).await {
        tracing::warn!("could not send the price of {model} to the principal: {error}");
    }
}

#[derive(Deserialize)]
pub(crate) struct PriceAsk {
    model: String,
    price: crate::config::ModelPrice,
}

/// `POST /api/internal/payments/price`: an admin deployed a priced model.
pub(crate) async fn price_route(auth: crate::auth::InternalNodeAuth, State(state): State<GatewayState>, Json(ask): Json<PriceAsk>) -> Response {
    let Some(caller) = auth.principal() else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "a signed peer call is required"}))).into_response();
    };
    if !crate::admin_trust::is_admin_node(&state.admin_trust.document(), &caller.node_id) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "only an admin deploys"}))).into_response();
    }
    if !is_principal(&state) {
        return (StatusCode::CONFLICT, Json(json!({"error": "this node is not the principal"}))).into_response();
    }
    mirror_price(&state, &ask.model, ask.price).await;
    Json(json!({ "recorded": true })).into_response()
}

// ── The admin's and the visitor's screens ──────────────────────────────────

/// `GET /api/ui/payments` (principal): the switch, prices, every account's
/// balance and hold, and the latest ledger rows.
pub(crate) async fn overview(State(state): State<GatewayState>) -> Response {
    let settings = settings(&state).await;
    // The visitors each account was admitted for.
    let mut visitors: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for record in state.network_chat.admissions() {
        if let Some(account) = record.account.clone() {
            visitors
                .entry(account)
                .or_default()
                .push(json!({ "node_id": record.node_id, "name": record.card.hostname, "status": record.status }));
        }
    }
    let mut book: std::collections::BTreeMap<String, crate::payments::book::AccountState> =
        self::state().book().accounts().into_iter().collect();
    for account in visitors.keys() {
        book.entry(account.clone()).or_default();
    }
    let accounts: Vec<Value> = book
        .into_iter()
        .map(|(account, s)| {
            json!({
                "account": account,
                "balance_micro": s.balance_micro,
                "reserved_micro": s.reserved_micro,
                "visitors": visitors.get(&account).cloned().unwrap_or_default(),
            })
        })
        .collect();
    let ledger = state
        .database
        .with_read("payments ledger", |c| {
            let mut stmt = c.prepare(
                "SELECT key_id, kind, amount_micro, detail_json, created_at FROM x402_ledger ORDER BY id DESC LIMIT 200",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "account": r.get::<_, String>(0)?,
                        "kind": r.get::<_, String>(1)?,
                        "amount_micro": r.get::<_, i64>(2)?,
                        "detail": r.get::<_, Option<String>>(3)?,
                        "at": r.get::<_, String>(4)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default();
    Json(json!({
        "enabled": settings.payments.enabled,
        "inference_access": settings.inference_access,
        "prices": settings.payments.prices,
        "principal": is_principal(&state),
        "accounts": accounts,
        "requests": credit_requests(&state),
        "ledger": ledger,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(crate) struct GrantAsk {
    account: String,
    amount_micro: i64,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/ui/payments/grant` (principal): credit an account, after being
/// paid outside the POM (phase 1).
pub(crate) async fn grant_route(State(state): State<GatewayState>, Json(ask): Json<GrantAsk>) -> Response {
    if !is_principal(&state) {
        return (StatusCode::CONFLICT, Json(json!({"error": "only the principal holds the accounts"}))).into_response();
    }
    if ask.amount_micro == 0 || super::AccountId::parse(&ask.account).as_string() != ask.account {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "an account (net:<key> or key:<id>) and a non-zero amount are required"}))).into_response();
    }
    self::state().grant(&ask.account, ask.amount_micro);
    if let Some(ledger) = ledger() {
        let detail = ask.note.map(|n| json!({ "note": n }).to_string());
        ledger.enqueue(PaymentWrite::grant(&ask.account, ask.amount_micro, detail));
    }
    Json(json!({ "account": ask.account, "balance_micro": self::state().book().balance(&ask.account) })).into_response()
}

/// `GET /api/ui/payments/balance` (this machine's own, a visitor): its home
/// network's balance on the network it visits, asked of an entry node.
pub(crate) async fn my_balance(State(state): State<GatewayState>) -> Response {
    let Some(home) = crate::home_statement::own(&state) else {
        return Json(json!({ "available": false, "reason": "this machine cannot name its home network" })).into_response();
    };
    let settings = settings(&state).await;
    let body = json!({ "home": home });
    for peer in crate::delegation::entry_nodes(&state, settings.entry_nodes).await {
        if let Ok(response) = state.control_post_json(&peer, "/v1/gateway/balance", &body, PEER_TIMEOUT).await {
            if response.status().is_success() {
                if let Ok(mut answer) = response.json::<Value>().await {
                    answer["spent"] = json!(own_spend(&state));
                    return Json(answer).into_response();
                }
            }
        }
    }
    Json(json!({ "available": false, "reason": "no entry node answered" })).into_response()
}

// ── Credit requests: a visitor asks, the principal decides ──────────────────

#[derive(Deserialize)]
pub(crate) struct CreditAsk {
    amount_micro: i64,
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/ui/payments/request` (a visitor's own): asks the network it
/// visits for credit, through an entry node.
pub(crate) async fn request_credit(State(state): State<GatewayState>, Json(ask): Json<CreditAsk>) -> Response {
    if ask.amount_micro <= 0 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "the amount must be positive"}))).into_response();
    }
    let Some(home) = crate::home_statement::own(&state) else {
        return (StatusCode::CONFLICT, Json(json!({"error": "this machine cannot name its home network"}))).into_response();
    };
    let settings = settings(&state).await;
    let body = json!({ "home": home, "amount_micro": ask.amount_micro, "note": ask.note });
    let mut tried = Vec::new();
    for peer in crate::delegation::entry_nodes(&state, settings.entry_nodes).await {
        match state.control_post_json(&peer, "/v1/gateway/credit-request", &body, PEER_TIMEOUT).await {
            Ok(response) if response.status().is_success() => return Json(json!({ "requested": true })).into_response(),
            Ok(response) => {
                let status = response.status().as_u16();
                let answer = response.text().await.unwrap_or_default();
                tried.push(json!({ "node": peer.node_id, "status": status, "answer": answer }));
            }
            Err(error) => tried.push(json!({ "node": peer.node_id, "error": error.to_string() })),
        }
    }
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "no entry node took the request", "tried": tried}))).into_response()
}

/// `POST /v1/gateway/credit-request` (on an entry node, for a visitor).
pub(crate) async fn visitor_credit_request(auth: crate::auth::InternalNodeAuth, State(state): State<GatewayState>, Json(body): Json<Value>) -> Response {
    let Some(caller) = auth.principal().cloned() else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "a signed peer call is required"}))).into_response();
    };
    let Some(home) = body["home"]
        .as_str()
        .and_then(|t| crate::home_statement::verify(t, &caller.node_id, chrono::Utc::now().timestamp()).ok())
    else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "home_statement_required"}))).into_response();
    };
    let amount = body["amount_micro"].as_i64().unwrap_or(0);
    if amount <= 0 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "the amount must be positive"}))).into_response();
    }
    let request = json!({
        "account": crate::home_statement::account_of(&home),
        "node_id": caller.node_id,
        "amount_micro": amount,
        "note": body["note"].as_str().map(|n| n.chars().take(500).collect::<String>()),
    });
    if is_principal(&state) {
        return record_credit_request(&state, &request);
    }
    match to_principal(&state, "/api/internal/payments/credit-request", &request).await {
        Ok(answer) => Json(answer).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": error}))).into_response(),
    }
}

/// `POST /api/internal/payments/credit-request` (on the principal, from a member).
pub(crate) async fn credit_request_route(auth: crate::auth::InternalNodeAuth, State(state): State<GatewayState>, Json(request): Json<Value>) -> Response {
    if let Err(refusal) = merchant_caller(&state, &auth).await {
        return refusal;
    }
    record_credit_request(&state, &request)
}

fn record_credit_request(state: &GatewayState, request: &Value) -> Response {
    let (Some(account), Some(node_id), Some(amount)) = (
        request["account"].as_str(),
        request["node_id"].as_str(),
        request["amount_micro"].as_i64().filter(|a| *a > 0),
    ) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "account, node and amount are required"}))).into_response();
    };
    let result = state.database.with_write("record a credit request", |c| {
        c.execute(
            "INSERT INTO x402_credit_requests (account, node_id, amount_micro, note, created_at) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![account, node_id, amount, request["note"].as_str(), chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    });
    match result {
        Ok(()) => Json(json!({ "recorded": true })).into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": format!("{error:#}")}))).into_response(),
    }
}

fn credit_requests(state: &GatewayState) -> Vec<Value> {
    state
        .database
        .with_read("credit requests", |c| {
            let mut stmt = c.prepare(
                "SELECT id, account, node_id, amount_micro, note, status, created_at, decided_at
                 FROM x402_credit_requests ORDER BY (status = 'pending') DESC, id DESC LIMIT 100",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "account": r.get::<_, String>(1)?,
                        "node_id": r.get::<_, String>(2)?,
                        "amount_micro": r.get::<_, i64>(3)?,
                        "note": r.get::<_, Option<String>>(4)?,
                        "status": r.get::<_, String>(5)?,
                        "at": r.get::<_, String>(6)?,
                        "decided_at": r.get::<_, Option<String>>(7)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
pub(crate) struct CreditDecision {
    id: i64,
    /// `approve` or `decline`.
    decision: String,
    /// What to grant, when not the amount asked.
    #[serde(default)]
    amount_micro: Option<i64>,
}

/// `POST /api/ui/payments/decide` (principal): decide a credit request;
/// approving grants the credit.
pub(crate) async fn decide_credit(State(state): State<GatewayState>, Json(decision): Json<CreditDecision>) -> Response {
    if !is_principal(&state) {
        return (StatusCode::CONFLICT, Json(json!({"error": "only the principal holds the accounts"}))).into_response();
    }
    let approve = match decision.decision.as_str() {
        "approve" => true,
        "decline" => false,
        _ => return (StatusCode::BAD_REQUEST, Json(json!({"error": "decision must be approve or decline"}))).into_response(),
    };
    let pending = state.database.with_read("credit request", |c| {
        Ok(c.query_row(
            "SELECT account, amount_micro FROM x402_credit_requests WHERE id = ? AND status = 'pending'",
            [decision.id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?)
    });
    let Ok(Some((account, asked))) = pending else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no pending request with that id"}))).into_response();
    };
    let status = if approve { "approved" } else { "declined" };
    let _ = state.database.with_write("decide a credit request", |c| {
        c.execute(
            "UPDATE x402_credit_requests SET status = ?, decided_at = ? WHERE id = ?",
            rusqlite::params![status, chrono::Utc::now().to_rfc3339(), decision.id],
        )?;
        Ok(())
    });
    if approve {
        let amount = decision.amount_micro.filter(|a| *a > 0).unwrap_or(asked);
        self::state().grant(&account, amount);
        if let Some(ledger) = ledger() {
            let detail = json!({ "credit_request": decision.id }).to_string();
            ledger.enqueue(PaymentWrite::grant(&account, amount, Some(detail)));
        }
    }
    Json(json!({ "decided": status })).into_response()
}

/// What this visitor spent on the network it visits, newest first.
fn own_spend(state: &GatewayState) -> Vec<Value> {
    let host = state.admin_trust.document().primary.node_id;
    state
        .database
        .with_read("visitor spend", |c| {
            let mut stmt = c.prepare(
                "SELECT model, charged_micro, balance_micro, created_at FROM x402_spend
                 WHERE host_network = ? ORDER BY id DESC LIMIT 100",
            )?;
            let rows = stmt
                .query_map([host], |r| {
                    Ok(json!({
                        "model": r.get::<_, Option<String>>(0)?,
                        "charged_micro": r.get::<_, i64>(1)?,
                        "balance_micro": r.get::<_, Option<i64>>(2)?,
                        "at": r.get::<_, String>(3)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap_or_default()
}

/// `POST /v1/gateway/balance` (on an entry node, for a visitor): the balance
/// of the account the visitor's home statement names.
pub(crate) async fn visitor_balance(auth: crate::auth::InternalNodeAuth, State(state): State<GatewayState>, Json(body): Json<Value>) -> Response {
    let Some(principal) = auth.principal() else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "a signed peer call is required"}))).into_response();
    };
    let Some(home) = body["home"]
        .as_str()
        .and_then(|t| crate::home_statement::verify(t, &principal.node_id, chrono::Utc::now().timestamp()).ok())
    else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "home_statement_required"}))).into_response();
    };
    let account = crate::home_statement::account_of(&home);
    let settings = settings(&state).await;
    Json(json!({
        "available": true,
        "charging": settings.payments.enabled,
        "account": account,
        "balance_micro": balance(&state, &account).await,
        "prices": settings.payments.prices,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deployment_price_sets_changes_or_removes_the_models_price() {
        let p = |i, o| crate::config::ModelPrice { in_micro_per_1k: i, out_micro_per_1k: o };
        let empty = Default::default();
        let set = with_price(&empty, "m", p(1, 2)).unwrap();
        assert_eq!(set["m"], p(1, 2));
        assert!(with_price(&set, "m", p(1, 2)).is_none());
        assert!(with_price(&set, "m", p(0, 0)).unwrap().is_empty());
    }

    #[test]
    fn a_lease_reserves_only_the_free_balance() {
        let payments = PaymentState::new(true, "p");
        payments.grant("net:a", 5_000_000);
        assert_eq!(grant_lease(&payments, "net:a", 3_000_000), 3_000_000);
        assert_eq!(payments.book().reserved("net:a"), 3_000_000);
        // Only 2 credits are left free.
        assert_eq!(grant_lease(&payments, "net:a", 3_000_000), 2_000_000);
        assert_eq!(grant_lease(&payments, "net:a", 1), 0);
        // A settlement reported against the lease deducts it.
        payments.settle("net:a", 1_000_000, 1_000_000);
        assert_eq!(payments.book().balance("net:a"), 4_000_000);
        assert_eq!(payments.book().reserved("net:a"), 4_000_000);
        assert_eq!(grant_lease(&payments, "net:b", 1_000_000), 0);
    }
}
