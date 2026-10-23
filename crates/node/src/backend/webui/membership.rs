//! Membership tokens at run time (`crate::membership`): admins issue them,
//! members keep theirs fresh and announce it, and the secondary admins keep
//! a copy of the principal's admission list so they can issue and decide
//! too.
//!
//! The admission list stays the principal's: the lobby is the principal's,
//! and so is rotating the connection key when a member leaves. A secondary
//! reads it (`/api/internal/membership/roster`) and sends its decisions to
//! the principal (`/api/internal/membership/decide`).
//!
//! A ban does not wait for the principal: the admin who removes a member
//! signs a revocation (`membership::Revocation`) and pushes it to every
//! peer, and every node trades its list with an admin each tick
//! (`/api/internal/membership/revocations`), so a node that missed the push
//! catches up.

use super::*;
use crate::auth::InternalNodeAuth;
use crate::network_lobby::{AdmissionRecord, AdmissionStatus};
use axum::extract::State;
use serde_json::Value;
use tracing::{debug, warn};

pub(crate) const TOKEN_PATH: &str = "/api/internal/membership/token";
pub(crate) const ROSTER_PATH: &str = "/api/internal/membership/roster";
pub(crate) const DECIDE_PATH: &str = "/api/internal/membership/decide";
pub(crate) const REVOCATIONS_PATH: &str = "/api/internal/membership/revocations";
/// How often a member checks its token and a secondary rereads the list.
const KEEPER_TICK: Duration = Duration::from_secs(30);
/// How soon a member with no token looks again: for an admin to show up, and
/// after a refused request, doubling up to `KEEPER_TICK`. Right after a join
/// or a boot no admin has been heard yet; waiting the whole tick there left a
/// visitor with no models for up to 30 s.
const TOKEN_RETRY_MIN: Duration = Duration::from_secs(2);
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
    if let Some(role) = state
        .network_chat
        .admitted_role(&caller.node_id, state.admin_trust.revoked_node_at(&caller.node_id)) {
        return Some(role);
    }
    if !state.auth.read().await.network.open_admission()
        || state.admin_trust.revoked_node_at(&caller.node_id).is_some()
    {
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
        if let Err(error) = ban(state, node_id).await {
            warn!("could not sign the revocation of {node_id}: {error:#}");
        }
        state.network_chat.member_departed(node_id);
        state.network_chat.request_rotation(node_id);
    } else if !state.network_chat.decide_admission(node_id, status) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(state.network_chat.admissions()).into_response()
}

/// Whether `node_id` is an approved member on this node's list or on its
/// mirror of the principal's.
pub(crate) fn is_approved_member(state: &GatewayState, node_id: &str) -> bool {
    state
        .network_chat
        .admissions()
        .into_iter()
        .chain(state.network_chat.roster_mirror())
        .any(|record| record.node_id == node_id && record.status == AdmissionStatus::Approved)
}

/// Bans `node_id`: signs its revocation with this admin's key, keeps it and
/// pushes it to every peer. Needs no other node, the principal included.
pub(crate) async fn ban(
    state: &GatewayState,
    node_id: &str,
) -> anyhow::Result<crate::membership::Revocation> {
    let revocation = crate::membership::revoke(
        &state.authority_signing_key,
        &state.authority_principal,
        &state.admin_trust.document(),
        node_id,
        chrono::Utc::now().timestamp(),
    )?;
    let added = state.admin_trust.merge_revocations(vec![revocation.clone()]);
    on_revoked(state, &added).await;
    let state = state.clone();
    tokio::spawn(async move { push_revocations(&state, added).await });
    Ok(revocation)
}

/// What a node does with revocations it just learned. Every node stops
/// counting the member at once, since roles are checked against the list.
/// The principal also takes the member off its admission list and rotates
/// the connection key, as when a member leaves.
async fn on_revoked(state: &GatewayState, added: &[crate::membership::Revocation]) {
    for revocation in added {
        warn!("member {} revoked by admin {}", revocation.sub, revocation.iss);
        state.network.drop_revoked(&revocation.sub).await;
        if is_principal(state)
            && state
                .network_chat
                .admission_of(&revocation.sub)
                .is_some_and(|record| record.status == AdmissionStatus::Approved)
        {
            state.network_chat.member_departed(&revocation.sub);
            state.network_chat.request_rotation(&revocation.sub);
        }
    }
}

/// Sends `entries` to every live peer that has a gateway.
async fn push_revocations(state: &GatewayState, entries: Vec<crate::membership::Revocation>) {
    if entries.is_empty() {
        return;
    }
    for peer in state.network.alive_peers().await {
        if peer.node_id == state.network.node_id || peer.gateway_port.is_none() {
            continue;
        }
        let sent = crate::admin::post_internal_json(
            state,
            &peer,
            REVOCATIONS_PATH,
            &entries,
            MEMBERSHIP_TIMEOUT,
        )
        .await;
        if let Err(error) = sent {
            debug!("could not push revocations to {}: {error:#}", peer.node_id);
        }
    }
}

/// `POST /api/internal/membership/revocations` - takes the caller's
/// revocations and answers with this node's. Each entry carries an admin's
/// signature and is checked on its own, so whoever sends them does not
/// matter.
pub(crate) async fn internal_revocations(
    _auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Json(entries): Json<Vec<crate::membership::Revocation>>,
) -> Response {
    let added = state.admin_trust.merge_revocations(entries);
    on_revoked(&state, &added).await;
    Json(state.admin_trust.revocations()).into_response()
}

/// Trades revocation lists with one admin per tick, a different one each
/// time, so a node that missed a push learns the ban within a tick or two.
pub(crate) fn spawn_revocation_keeper(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(KEEPER_TICK);
        let mut turn = 0usize;
        loop {
            ticker.tick().await;
            if state.network.is_detached() {
                continue;
            }
            let document = state.admin_trust.document();
            let admins: Vec<_> = state
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
            if admins.is_empty() {
                continue;
            }
            turn = turn.wrapping_add(1);
            let peer = &admins[turn % admins.len()];
            let sent = crate::admin::post_internal_json(
                &state,
                peer,
                REVOCATIONS_PATH,
                &state.admin_trust.revocations(),
                MEMBERSHIP_TIMEOUT,
            )
            .await;
            match sent {
                Ok(response) if response.status().is_success() => {
                    match response.json::<Vec<crate::membership::Revocation>>().await {
                        Ok(entries) => {
                            let added = state.admin_trust.merge_revocations(entries);
                            on_revoked(&state, &added).await;
                        }
                        Err(error) => debug!("unreadable revocation list: {error:#}"),
                    }
                }
                Ok(response) => debug!(
                    "{} answered {} to the revocation exchange",
                    peer.node_id,
                    response.status()
                ),
                Err(error) => debug!("revocation exchange failed: {error:#}"),
            }
        }
    });
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
                crate::membership::member_role(
                    &state.admin_trust,
                    Some(&token),
                    &state.network.node_id,
                    &state.network.pubkey,
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
        let mut wait = Duration::ZERO;
        let mut backoff = TOKEN_RETRY_MIN;
        loop {
            tokio::time::sleep(wait).await;
            wait = KEEPER_TICK;
            if state.network.is_detached() || is_admin(&state) {
                state.network.set_membership(None);
                note_token_error(None);
                refresh_connection_phase(&state).await;
                continue;
            }
            let now = chrono::Utc::now().timestamp();
            let current = state.network.membership();
            let holds = crate::membership::member_role(
                &state.admin_trust,
                current.as_deref(),
                &state.network.node_id,
                &state.network.pubkey,
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
                refresh_connection_phase(&state).await;
                continue;
            }
            // No admin heard yet: nobody to ask, so only look again soon.
            if !holds && !admin_in_sight(&state).await {
                note_token_error(None);
                refresh_connection_phase(&state).await;
                wait = TOKEN_RETRY_MIN;
                continue;
            }
            match request_token(&state).await {
                Ok(token) => {
                    state.network.set_membership(Some(token));
                    note_token_error(None);
                    backoff = TOKEN_RETRY_MIN;
                    debug!("membership token renewed");
                }
                Err(error) => {
                    if !holds {
                        state.network.set_membership(None);
                        note_token_error(Some(format!("{error:#}")));
                        wait = backoff;
                        backoff = (backoff * 2).min(KEEPER_TICK);
                    }
                    debug!("could not renew the membership token: {error:#}");
                }
            }
            refresh_connection_phase(&state).await;
        }
    });
}

/// Where this node stands on its way into the network, for the interface:
/// without it a visitor that had just joined saw no models and was told the
/// network serves none, while it was still looking for the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConnectionPhase {
    /// An admin of its own chain, or off the network: it needs no token.
    Standalone,
    /// A member that has heard no admin of its network yet.
    Discovering,
    /// An admin is in sight, but this node holds no valid token yet.
    Authenticating,
    /// A member with a valid membership token.
    Connected,
}

struct ConnectionTrack {
    phase: Option<ConnectionPhase>,
    since: i64,
    last_error: Option<String>,
}

fn connection_track() -> &'static std::sync::Mutex<ConnectionTrack> {
    static TRACK: std::sync::OnceLock<std::sync::Mutex<ConnectionTrack>> = std::sync::OnceLock::new();
    TRACK.get_or_init(|| {
        std::sync::Mutex::new(ConnectionTrack {
            phase: None,
            since: chrono::Utc::now().timestamp(),
            last_error: None,
        })
    })
}

/// The last refused token request, kept while this node holds no token.
fn note_token_error(error: Option<String>) {
    if let Ok(mut track) = connection_track().lock() {
        track.last_error = error;
    }
}

/// Whether an admin of this node's chain is among the live peers.
async fn admin_in_sight(state: &GatewayState) -> bool {
    let document = state.admin_trust.document();
    state.network.alive_peers().await.iter().any(|peer| {
        peer.node_id != state.network.node_id
            && peer.gateway_port.is_some()
            && crate::admin_trust::is_admin_key(&document, &peer.node_id, &peer.pubkey)
    })
}

/// Reads the phase from the node's state now, and starts its clock when it
/// changed. Called by the keeper every tick and by `GET /api/ui/connection`.
async fn refresh_connection_phase(state: &GatewayState) -> (ConnectionPhase, i64, Option<String>) {
    let phase = if state.network.is_detached() || is_admin(state) {
        ConnectionPhase::Standalone
    } else if crate::membership::member_role(
        &state.admin_trust,
        state.network.membership().as_deref(),
        &state.network.node_id,
        &state.network.pubkey,
    )
    .is_some()
    {
        ConnectionPhase::Connected
    } else if admin_in_sight(state).await {
        ConnectionPhase::Authenticating
    } else {
        ConnectionPhase::Discovering
    };
    let Ok(mut track) = connection_track().lock() else {
        return (phase, chrono::Utc::now().timestamp(), None);
    };
    if track.phase != Some(phase) {
        track.phase = Some(phase);
        track.since = chrono::Utc::now().timestamp();
    }
    (phase, track.since, track.last_error.clone())
}

/// `GET /api/ui/connection` - this node's phase on the way into its network
/// (`ConnectionPhase`), since when (unix seconds), the last refused token
/// request while it has none, and how much of the cluster it sees now: the
/// other live peers and whether an admin is among them.
pub(super) async fn connection_status(State(state): State<GatewayState>) -> Json<Value> {
    let (phase, since, last_error) = refresh_connection_phase(&state).await;
    let peers = state
        .network
        .alive_peers()
        .await
        .iter()
        .filter(|peer| peer.node_id != state.network.node_id)
        .count();
    Json(json!({
        "phase": phase,
        "since": since,
        "last_error": last_error,
        "peers": peers,
        "admin_in_sight": admin_in_sight(&state).await,
    }))
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

#[cfg(test)]
mod connection_tests {
    use super::*;
    use crate::admin_trust::AuthorityPrincipal;

    /// A visitor of a principal's network walks the three phases as it hears
    /// the admin and then holds a token; an admin needs none of them.
    #[tokio::test]
    async fn a_member_goes_from_discovering_to_connected() {
        let principal_key = ed25519_dalek::SigningKey::from_bytes(&[31; 32]);
        let principal =
            AuthorityPrincipal::from_signing_key("principal", "peer-principal", &principal_key);
        let trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "connection-lab",
            principal.clone(),
            &principal_key,
        )
        .unwrap();

        let admin = crate::gateway::testkit::state_with("peer-principal", vec![]).await;
        let mut admin = admin;
        admin.admin_trust = std::sync::Arc::new(
            crate::admin_trust::AdminTrust::in_memory(trust.document()).unwrap(),
        );
        admin.authority_principal = principal.clone();
        assert_eq!(refresh_connection_phase(&admin).await.0, ConnectionPhase::Standalone);

        let visitor_key = ed25519_dalek::SigningKey::from_bytes(&[32; 32]);
        let mut visitor = crate::gateway::testkit::state_with("peer-visitor", vec![]).await;
        visitor.admin_trust = std::sync::Arc::new(
            crate::admin_trust::AdminTrust::in_memory(trust.document()).unwrap(),
        );
        visitor.authority_principal =
            AuthorityPrincipal::from_signing_key("visitor", "peer-visitor", &visitor_key);
        assert_eq!(refresh_connection_phase(&visitor).await.0, ConnectionPhase::Discovering);

        let mut heard = crate::gateway::testkit::peer(&principal.node_id, "principal", 8090);
        heard.pubkey = principal.pubkey.clone();
        visitor
            .network
            .peers
            .write()
            .await
            .insert(heard.node_id.clone(), heard);
        assert_eq!(refresh_connection_phase(&visitor).await.0, ConnectionPhase::Authenticating);

        let token = crate::membership::issue(
            &principal_key,
            &principal,
            &trust.document(),
            &visitor.network.node_id,
            &visitor.network.pubkey,
            crate::config::EnrollmentRole::Visitor,
            chrono::Utc::now().timestamp(),
        )
        .unwrap();
        visitor.network.set_membership(Some(token));
        assert_eq!(refresh_connection_phase(&visitor).await.0, ConnectionPhase::Connected);
        let Json(status) = connection_status(State(visitor.clone())).await;
        assert_eq!(status["phase"], "connected");
        assert_eq!(status["peers"], 1);
        assert_eq!(status["admin_in_sight"], true);
    }
}
