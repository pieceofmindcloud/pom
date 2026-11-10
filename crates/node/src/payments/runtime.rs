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

// ── The admin's and the visitor's screens ──────────────────────────────────

/// `GET /api/ui/payments` (principal): the switch, prices, every account's
/// balance and hold, and the latest ledger rows.
pub(crate) async fn overview(State(state): State<GatewayState>) -> Response {
    let settings = settings(&state).await;
    let accounts: Vec<Value> = self::state()
        .book()
        .accounts()
        .into_iter()
        .map(|(account, s)| json!({ "account": account, "balance_micro": s.balance_micro, "reserved_micro": s.reserved_micro }))
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
                if let Ok(answer) = response.json::<Value>().await {
                    return Json(answer).into_response();
                }
            }
        }
    }
    Json(json!({ "available": false, "reason": "no entry node answered" })).into_response()
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
