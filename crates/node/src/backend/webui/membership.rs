//! Membership tokens at run time (`crate::membership`): admins issue them,
//! members keep theirs fresh and announce it, and the secondary admins keep
//! a copy of the principal's admission list so they can issue and decide
//! too.
//!
//! The admission list stays the principal's: the lobby is the principal's,
//! and so is rotating the connection key when a member leaves. A secondary
//! reads it (`/api/internal/membership/roster`) and sends its decisions to
//! the principal (`/api/internal/membership/decide`).

use super::*;
use crate::auth::InternalNodeAuth;
use crate::network_lobby::{AdmissionRecord, AdmissionStatus};
use axum::extract::State;
use serde_json::Value;
use tracing::debug;

pub(crate) const TOKEN_PATH: &str = "/api/internal/membership/token";
pub(crate) const ROSTER_PATH: &str = "/api/internal/membership/roster";
pub(crate) const DECIDE_PATH: &str = "/api/internal/membership/decide";
/// How often a member checks its token and a secondary rereads the list.
const KEEPER_TICK: Duration = Duration::from_secs(30);
const MEMBERSHIP_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether this node is an admin of its own chain.
pub(crate) fn is_admin(state: &GatewayState) -> bool {
    crate::admin_trust::control_issuer_is_admin(
        &state.admin_trust.document(),
        &state.authority_principal,
    )
}

/// Whether this node is the chain's principal.
pub(crate) fn is_principal(state: &GatewayState) -> bool {
    is_admin(state) && state.admin_trust.current_primary() == state.authority_principal
}

fn refused(reason: &'static str) -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": reason, "reason": reason }))).into_response()
}

/// The admin who signed `auth`, when it is one of the chain's.
fn admin_caller(state: &GatewayState, auth: &InternalNodeAuth) -> bool {
    auth.principal().is_some_and(|issuer| {
        crate::admin_trust::control_issuer_is_admin(&state.admin_trust.document(), issuer)
    })
}

/// The role a member gets in its token: the one it was admitted with. A
/// legacy network open to anyone with the anchor has no admissions, and
/// keeps what the node announces.
async fn role_to_issue(
    state: &GatewayState,
    caller: &crate::admin_trust::AuthorityPrincipal,
) -> Option<crate::config::EnrollmentRole> {
    if let Some(role) = state.network_chat.admitted_role(&caller.node_id) {
        return Some(role);
    }
    if !state.auth.read().await.network.open_admission() {
        return None;
    }
    let peer = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == caller.node_id && peer.pubkey == caller.pubkey)?;
    Some(if peer.profile == crate::profile::NodeProfile::Visitor {
        crate::config::EnrollmentRole::Visitor
    } else {
        crate::config::EnrollmentRole::Worker
    })
}

/// `POST /api/internal/membership/token` - on an admin: a fresh token for
/// the member that signed the request, with the role it was admitted with.
/// An admin needs none, and a machine nobody admitted gets none.
pub(crate) async fn internal_token(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
) -> Response {
    let Some(caller) = auth.principal().cloned() else {
        return refused("signature_required");
    };
    if !is_admin(&state) {
        return refused("not_an_admin");
    }
    let document = state.admin_trust.document();
    if crate::admin_trust::control_issuer_is_admin(&document, &caller) {
        return refused("admins_need_no_token");
    }
    let Some(role) = role_to_issue(&state, &caller).await else {
        return refused("not_admitted");
    };
    match crate::membership::issue(
        &state.authority_signing_key,
        &state.authority_principal,
        &document,
        &caller.node_id,
        &caller.pubkey,
        role,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(token) => Json(json!({ "token": token })).into_response(),
        Err(error) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("could not issue the token: {error:#}") })),
        )
            .into_response(),
    }
}

/// `POST /api/internal/membership/roster` - on the principal: its admission
/// list, for a secondary admin.
pub(crate) async fn internal_roster(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
) -> Response {
    if !admin_caller(&state, &auth) {
        return refused("admin_required");
    }
    if !is_principal(&state) {
        return refused("not_the_principal");
    }
    Json(state.network_chat.admissions()).into_response()
}

/// `POST /api/internal/membership/decide` - on the principal: a secondary
/// admin's decision on one admission, applied as the principal's own.
pub(crate) async fn internal_decide(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Json(decision): Json<AdmissionDecision>,
) -> Response {
    if !admin_caller(&state, &auth) {
        return refused("admin_required");
    }
    if !is_principal(&state) {
        return refused("not_the_principal");
    }
    apply_decision(&state, decision).await
}

/// Applies one decision on this node's admission list - the principal's.
/// Forgetting a machine that was approved takes its membership away, and it
/// still holds the connection key: same treatment as a departure. An admin
/// is not removed this way: its role is the chain's, and only the principal
/// demotes.
pub(crate) async fn apply_decision(state: &GatewayState, decision: AdmissionDecision) -> Response {
    let status = match decision.decision.as_str() {
        "approve" => Some(AdmissionStatus::Approved),
        "reject" => Some(AdmissionStatus::Rejected),
        "forget" => None,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "decision must be approve, reject or forget" })),
            )
                .into_response()
        }
    };
    let node_id = decision.node_id.trim();
    let document = state.admin_trust.document();
    if std::iter::once(&document.primary)
        .chain(document.grants.iter().map(|grant| &grant.subject))
        .any(|admin| {
            admin.node_id == node_id
                && crate::admin_trust::control_issuer_is_admin(&document, admin)
        })
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "an admin is not removed through admissions",
                "reason": "admin_target",
            })),
        )
            .into_response();
    }
    let revoked = status.is_none()
        && state
            .network_chat
            .admission_of(node_id)
            .is_some_and(|record| record.status == AdmissionStatus::Approved);
    if revoked {
        state.network_chat.member_departed(node_id);
        state.network_chat.request_rotation(node_id);
    } else if !state.network_chat.decide_admission(node_id, status) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(state.network_chat.admissions()).into_response()
}

/// Reads the principal's admission list into the mirror. Returns it.
pub(crate) async fn refresh_roster(state: &GatewayState) -> anyhow::Result<Vec<AdmissionRecord>> {
    let peer = principal_peer(state).await?;
    let response = crate::admin::post_internal_json(
        state,
        &peer,
        ROSTER_PATH,
        &json!({}),
        MEMBERSHIP_TIMEOUT,
    )
    .await?;
    if !response.status().is_success() {
        anyhow::bail!("the principal answered {}", response.status());
    }
    let records: Vec<AdmissionRecord> = response.json().await?;
    state.network_chat.set_roster_mirror(records.clone());
    Ok(records)
}

/// Sends a secondary's decision to the principal and returns the list as
/// the principal has it afterwards.
pub(crate) async fn forward_decision(
    state: &GatewayState,
    decision: &AdmissionDecision,
) -> Response {
    let peer = match principal_peer(state).await {
        Ok(peer) => peer,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": format!("{error:#}"), "reason": "principal_unreachable" })),
            )
                .into_response()
        }
    };
    let sent = crate::admin::post_internal_json(
        state,
        &peer,
        DECIDE_PATH,
        decision,
        MEMBERSHIP_TIMEOUT,
    )
    .await;
    match sent {
        Ok(response) if response.status().is_success() => {
            match response.json::<Vec<AdmissionRecord>>().await {
                Ok(records) => {
                    state.network_chat.set_roster_mirror(records.clone());
                    Json(records).into_response()
                }
                Err(error) => (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": format!("{error:#}") })),
                )
                    .into_response(),
            }
        }
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            let body = response.json::<Value>().await.unwrap_or(Value::Null);
            (status, Json(body)).into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("{error:#}"), "reason": "principal_unreachable" })),
        )
            .into_response(),
    }
}

async fn principal_peer(state: &GatewayState) -> anyhow::Result<crate::network::PeerInfo> {
    let primary = state.admin_trust.current_primary();
    crate::admin::principal_peer_for(&state.network, &primary)
        .await
        .ok_or_else(|| anyhow::anyhow!("the principal is not reachable"))
}

/// Asks the admins in turn for a new token, until one gives it.
async fn request_token(state: &GatewayState) -> anyhow::Result<String> {
    let document = state.admin_trust.document();
    let mut admins: Vec<_> = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .filter(|peer| {
            peer.node_id != state.network.node_id
                && peer.gateway_port.is_some()
                && crate::admin_trust::is_admin_key(&document, &peer.node_id, &peer.pubkey)
        })
        .collect();
    // The principal first: it holds the list the others mirror.
    admins.sort_by_key(|peer| peer.node_id != document.primary.node_id);
    let mut last = anyhow::anyhow!("no admin of the network is reachable");
    for peer in admins {
        let sent = crate::admin::post_internal_json(
            state,
            &peer,
            TOKEN_PATH,
            &json!({}),
            MEMBERSHIP_TIMEOUT,
        )
        .await;
        match sent {
            Ok(response) if response.status().is_success() => {
                let token = response
                    .json::<Value>()
                    .await?
                    .get("token")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("the admin answered without a token"))?;
                // A token this node cannot verify itself would be refused
                // everywhere else as well.
                crate::membership::role_of(
                    Some(&token),
                    &state.network.node_id,
                    &state.network.pubkey,
                    &document,
                    chrono::Utc::now().timestamp(),
                )
                .ok_or_else(|| anyhow::anyhow!("the admin's token does not hold"))?;
                return Ok(token);
            }
            Ok(response) => last = anyhow::anyhow!("{} answered {}", peer.node_id, response.status()),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Keeps this node's token fresh and announced. An admin carries none. A
/// token that stopped holding - its issuer was demoted, the chain moved on -
/// is replaced at once; one that expires without a renewal is dropped, and
/// the node is then refused what only a worker may ask.
pub(crate) fn spawn_token_keeper(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(KEEPER_TICK);
        loop {
            ticker.tick().await;
            if state.network.is_detached() || is_admin(&state) {
                state.network.set_membership(None);
                continue;
            }
            let now = chrono::Utc::now().timestamp();
            let current = state.network.membership();
            let holds = crate::membership::role_of(
                current.as_deref(),
                &state.network.node_id,
                &state.network.pubkey,
                &state.admin_trust.document(),
                now,
            )
            .is_some();
            let due = current
                .as_deref()
                .and_then(crate::membership::expires_at)
                .is_none_or(|exp| {
                    now >= exp - crate::membership::TOKEN_TTL_SECS
                        + crate::membership::RENEW_AFTER_SECS
                });
            if holds && !due {
                continue;
            }
            match request_token(&state).await {
                Ok(token) => {
                    state.network.set_membership(Some(token));
                    debug!("membership token renewed");
                }
                Err(error) => {
                    if !holds {
                        state.network.set_membership(None);
                    }
                    debug!("could not renew the membership token: {error:#}");
                }
            }
        }
    });
}

/// On a secondary admin: keeps the mirror of the principal's admission
/// list. Off a secondary the mirror is empty.
pub(crate) fn spawn_roster_keeper(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(KEEPER_TICK);
        loop {
            ticker.tick().await;
            if !is_admin(&state) || is_principal(&state) {
                state.network_chat.set_roster_mirror(Vec::new());
                continue;
            }
            if let Err(error) = refresh_roster(&state).await {
                debug!("could not read the principal's admission list: {error:#}");
            }
        }
    });
}
