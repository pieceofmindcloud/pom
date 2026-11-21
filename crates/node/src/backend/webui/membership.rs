//! Membership at run time (`crate::membership`): the secondary admins keep
//! a copy of the principal's admission list so they can decide and vouch
//! too, every admin signs the admitted roles as **vouches** and the keepers
//! carry them to every node, and the members keep their relay passes fresh
//! by asking an admin. The membership token was retired 2026-11-15
//! (phase C3 of the admin-hub routing, decision 5: no compatibility
//! bridge): the roster and its vouches are what prove a member.
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

pub(crate) const RELAYS_PATH: &str = "/api/internal/membership/relays";
pub(crate) const ROSTER_PATH: &str = "/api/internal/membership/roster";
pub(crate) const DECIDE_PATH: &str = "/api/internal/membership/decide";
pub(crate) const REVOCATIONS_PATH: &str = "/api/internal/membership/revocations";
pub(crate) const VOUCHES_PATH: &str = "/api/internal/membership/vouches";
/// How often a member renews its relay passes and a secondary rereads the
/// list.
const KEEPER_TICK: Duration = Duration::from_secs(30);
/// How soon a member with no passes looks again: for an admin to show up,
/// and after a refused request, doubling up to `KEEPER_TICK`. Right after a
/// join or a boot no admin has been heard yet; waiting the whole tick there
/// left a visitor with no models for up to 30 s.
const RELAY_RETRY_MIN: Duration = Duration::from_secs(2);
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
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": reason, "reason": reason })),
    )
        .into_response()
}

/// The admin who signed `auth`, when it is one of the chain's.
fn admin_caller(state: &GatewayState, auth: &InternalNodeAuth) -> bool {
    auth.principal().is_some_and(|issuer| {
        crate::admin_trust::control_issuer_is_admin(&state.admin_trust.document(), issuer)
    })
}

/// The role this admin can pass for `caller` with: the one its admission
/// gave it. A network open to anyone with the anchor has no admissions, and
/// keeps what the node announces.
async fn role_to_admit(
    state: &GatewayState,
    caller: &crate::admin_trust::AuthorityPrincipal,
) -> Option<crate::config::EnrollmentRole> {
    if let Some(role) = state.network_chat.admitted_role(
        &caller.node_id,
        state.admin_trust.revoked_node_at(&caller.node_id),
    ) {
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

/// `POST /api/internal/membership/relays` - on an admin: the relay passes it
/// signs for the member that signed the request, for this network's visitor
/// relays. An admin signs its own passes and asks for none; a machine nobody
/// admitted gets none. The membership token this route used to answer with
/// was retired 2026-11-15 (phase C3): the vouches prove membership now.
pub(crate) async fn internal_relays(
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
        return refused("admins_sign_their_own_passes");
    }
    if role_to_admit(&state, &caller).await.is_none() {
        return refused("not_admitted");
    }
    Json(json!({
        "relays": relay_passes(&state, &caller.node_id).await,
    }))
    .into_response()
}

/// The visitors' anonymous relays this admin sees, each with a fresh pass,
/// for `member` (its own relay left out). On an admin only: passes are
/// signed with the admin's key.
pub(crate) async fn relay_passes(
    state: &GatewayState,
    member: &str,
) -> Vec<crate::network::AnonymousRelay> {
    // A direct-only member (`--no-iroh`) holds no ephemeral endpoint and dials
    // no visitor relay: passes for it would go nowhere (spec 2026-11-15).
    if member == state.network.node_id && state.network.is_direct_only()
        || state.network.peer_direct_only(member).await
    {
        return Vec::new();
    }
    let now = chrono::Utc::now().timestamp();
    let document = state.admin_trust.document();
    let mut relays = Vec::new();
    for peer in state.network.alive_peers().await {
        if peer.node_id == member || !crate::gateway::peer_is_visitor(state, &peer) {
            continue;
        }
        let Some(url) = peer
            .relay
            .as_ref()
            .filter(|offer| offer.anonymous)
            .and_then(|offer| offer.url.clone())
        else {
            continue;
        };
        // The URL is the visitor's own claim: a pass goes only to a relay on a
        // public host the peer is reached at, never one it points elsewhere.
        if !relay_url_is_peers(&url, &peer) {
            debug!(
                "no relay pass for {url}: not a public address of {}",
                peer.node_id
            );
            continue;
        }
        match crate::relay_pass::issue(
            &state.authority_signing_key,
            &state.authority_principal,
            &document,
            &url,
            now,
        ) {
            Ok(pass) => relays.push(crate::network::AnonymousRelay { url, pass }),
            Err(error) => debug!("no relay pass for {url}: {error:#}"),
        }
    }
    relays
}

/// Host of `http(s)://host:port/`, without IPv6 brackets.
fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let authority = rest.split('/').next()?;
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next()?,
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    };
    (!host.is_empty()).then_some(host)
}

/// Whether `url` names a relay on a public host that is one of `peer`'s own
/// addresses (announced, or the source its packets came from).
fn relay_url_is_peers(url: &str, peer: &crate::network::PeerInfo) -> bool {
    let Some(host) = url_host(url) else {
        return false;
    };
    if crate::addrs::is_private_or_local(host) {
        return false;
    }
    std::iter::once(&peer.addr)
        .chain(peer.addr_candidates.iter())
        .filter_map(|addr| addr.parse::<std::net::SocketAddr>().ok())
        .any(|addr| addr.ip().to_string() == host)
}

#[cfg(test)]
mod relay_url_tests {
    use super::*;

    fn peer(addrs: &[&str]) -> crate::network::PeerInfo {
        let mut peer = crate::gateway::testkit::peer("visitor", "visitor", 8080);
        peer.addr = addrs[0].to_string();
        peer.addr_candidates = addrs.iter().map(|a| a.to_string()).collect();
        peer
    }

    #[test]
    fn a_pass_goes_only_to_a_relay_on_the_peers_own_public_address() {
        let visitor = peer(&["203.0.113.5:42777", "[2001:db8::5]:42777"]);
        assert!(relay_url_is_peers("http://203.0.113.5:52000/", &visitor));
        assert!(relay_url_is_peers("http://[2001:db8::5]:52000/", &visitor));
        // Another host, a private one, or no URL at all.
        assert!(!relay_url_is_peers("http://198.51.100.9:52000/", &visitor));
        assert!(!relay_url_is_peers(
            "http://10.0.0.5:52000/",
            &peer(&["10.0.0.5:42777"])
        ));
        assert!(!relay_url_is_peers("relay.example", &visitor));
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
    if status == Some(AdmissionStatus::Approved) {
        let account = state
            .network_chat
            .admission_of(node_id)
            .and_then(|r| r.account);
        if let Some(account) = account {
            crate::payments::runtime::open_account(
                &account,
                decision.grant_micro.unwrap_or(0),
                node_id,
            );
        }
        // Phase C of the admin-hub routing: the approval is vouched to the
        // network at once, beside the ban's push of the removal.
        let publisher = state.clone();
        tokio::spawn(async move {
            publish_vouches(&publisher).await;
        });
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
    let added = state
        .admin_trust
        .merge_revocations(vec![revocation.clone()]);
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
        warn!(
            "member {} revoked by admin {}",
            revocation.sub, revocation.iss
        );
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

/// Signs this admin's vouches for every approved admission it sees - its
/// own list and the mirror of the principal's (phase C of the admin-hub
/// routing, spec 10.1). A vouch repeats the approval: same member, same
/// role, same decision time, this admin's signature. Decisions older than
/// the member's last ban are not vouched: the ban already undoes them.
pub(crate) async fn sign_vouches(state: &GatewayState) -> Vec<crate::membership::Vouch> {
    if !is_admin(&state) {
        return Vec::new();
    }
    let document = state.admin_trust.document();
    let now = chrono::Utc::now().timestamp();
    let mut newest: std::collections::HashMap<String, (i64, crate::config::EnrollmentRole)> =
        std::collections::HashMap::new();
    for record in state
        .network_chat
        .admissions()
        .into_iter()
        .chain(state.network_chat.roster_mirror())
    {
        if record.status != AdmissionStatus::Approved {
            continue;
        }
        let Some(admitted_at) = record
            .decided_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.timestamp())
        else {
            continue;
        };
        if state
            .admin_trust
            .revoked_node_at(&record.node_id)
            .is_some_and(|revoked| admitted_at <= revoked)
        {
            continue;
        }
        match newest.get(&record.node_id) {
            Some((known_at, _)) if *known_at >= admitted_at => {}
            _ => {
                newest.insert(record.node_id.clone(), (admitted_at, record.role));
            }
        }
    }
    // A network open to anyone with the anchor admits by the key, not by a
    // list: the admin vouches what a live peer announces, the way it used to
    // issue those peers tokens. A ban is final for such a peer, as it was in
    // the token answer: an admin signs a peer the chain holds any revocation
    // for no vouch, and a peer that goes quiet stops being signed, so its
    // vouch ages out within the vouch window.
    if state.auth.read().await.network.open_admission() {
        for peer in state.network.alive_peers().await {
            if peer.node_id == state.network.node_id || newest.contains_key(&peer.node_id) {
                continue;
            }
            if crate::admin_trust::is_admin_key(&document, &peer.node_id, &peer.pubkey) {
                continue;
            }
            if state.admin_trust.revoked_node_at(&peer.node_id).is_some() {
                continue;
            }
            let role = if peer.profile == crate::profile::NodeProfile::Visitor {
                crate::config::EnrollmentRole::Visitor
            } else {
                crate::config::EnrollmentRole::Worker
            };
            newest.insert(peer.node_id.clone(), (now, role));
        }
    }
    // An instance of a member (`crate::instances`): the host signed that the
    // node is its own, so it joins with the host's role - an admin host's
    // instance as a worker - for as long as the host is a member. A host
    // with a ban gets no vouch for its instances, which then age out.
    for peer in state.network.alive_peers().await {
        let Some(cert) = peer.instance.as_ref() else {
            continue;
        };
        if newest.contains_key(&peer.node_id)
            || !cert.verifies_for(&peer.node_id)
            || state.admin_trust.revoked_node_at(&peer.node_id).is_some()
            || state.admin_trust.revoked_node_at(&cert.host_node_id).is_some()
        {
            continue;
        }
        let host_role = if crate::admin_trust::is_admin_key(&document, &cert.host_node_id, &cert.host_pubkey) {
            Some(crate::config::EnrollmentRole::Worker)
        } else {
            newest.get(&cert.host_node_id).map(|(_, role)| *role)
        };
        if let Some(role) = host_role {
            newest.insert(peer.node_id.clone(), (now, role));
        }
    }
    let mut out = Vec::new();
    for (node_id, (admitted_at, role)) in newest {
        match crate::membership::sign_vouch(
            &state.authority_signing_key,
            &state.authority_principal,
            &document,
            &node_id,
            role,
            admitted_at,
            now,
        ) {
            Ok(vouch) => out.push(vouch),
            Err(error) => debug!("could not vouch {node_id}: {error:#}"),
        }
    }
    out
}

/// Re-signs this admin's vouches into its ledger and carries them to every
/// live peer with a gateway, so an approval reaches the whole network at
/// once - the push beside `push_revocations`, same trust: every entry is
/// checked on signature wherever it lands. Non-admins keep their copy fresh
/// through the keeper exchange instead (`spawn_vouch_keeper`).
pub(crate) async fn publish_vouches(state: &GatewayState) {
    let signed = sign_vouches(state).await;
    state.admin_trust.merge_vouches(signed);
    let entries = state.admin_trust.vouches();
    if entries.is_empty() {
        return;
    }
    for peer in state.network.alive_peers().await {
        if peer.node_id == state.network.node_id || peer.gateway_port.is_none() {
            continue;
        }
        if let Err(error) = crate::admin::post_internal_json(
            state,
            &peer,
            VOUCHES_PATH,
            &entries,
            MEMBERSHIP_TIMEOUT,
        )
        .await
        {
            debug!("could not push vouches to {}: {error:#}", peer.node_id);
        }
    }
}

/// `POST /api/internal/membership/vouches` - takes the caller's vouches and
/// answers with this node's. Each entry carries an admin's signature and is
/// checked on its own, so whoever sends them does not matter; this is how a
/// node that never met the principal still holds the roster's word for its
/// own decisions (phase C).
pub(crate) async fn internal_vouches(
    _auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Json(entries): Json<Vec<crate::membership::Vouch>>,
) -> Response {
    state.admin_trust.merge_vouches(entries);
    Json(state.admin_trust.vouches()).into_response()
}

/// Keeps the roster vouches fresh on every node (phase C). An admin
/// re-signs the vouches of its admissions every tick - the renewal is what
/// lets a member dropped from the roster age out when its ban never
/// arrived. Every node then trades lists with one admin per tick, a
/// different one each time, so a node that missed a push holds the
/// roster's word within a tick or two.
pub(crate) fn spawn_vouch_keeper(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(KEEPER_TICK);
        let mut turn = 0usize;
        loop {
            ticker.tick().await;
            if state.network.is_detached() {
                continue;
            }
            if is_admin(&state) {
                let signed = sign_vouches(&state).await;
                if !signed.is_empty() {
                    state.admin_trust.merge_vouches(signed);
                }
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
                VOUCHES_PATH,
                &state.admin_trust.vouches(),
                MEMBERSHIP_TIMEOUT,
            )
            .await;
            match sent {
                Ok(response) if response.status().is_success() => {
                    match response.json::<Vec<crate::membership::Vouch>>().await {
                        Ok(entries) => {
                            state.admin_trust.merge_vouches(entries);
                        }
                        Err(error) => debug!("unreadable roster vouches: {error:#}"),
                    }
                }
                Ok(response) => debug!(
                    "{} answered {} to the vouch exchange",
                    peer.node_id,
                    response.status()
                ),
                Err(error) => debug!("vouch exchange failed: {error:#}"),
            }
        }
    });
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
    let response =
        crate::admin::post_internal_json(state, &peer, ROSTER_PATH, &json!({}), MEMBERSHIP_TIMEOUT)
            .await?;
    if !response.status().is_success() {
        anyhow::bail!("the principal answered {}", response.status());
    }
    let records: Vec<AdmissionRecord> = response.json().await?;
    state.network_chat.set_roster_mirror(records.clone());
    persist_roster_mirror(state, &records);
    Ok(records)
}

/// `local_settings` key of the secondary's persisted mirror.
const ROSTER_SETTING: &str = "roster_mirror";

/// Writes the mirror last read from the principal, so a secondary admin that
/// restarts while the principal is offline still knows who the network
/// admitted (admin-hub design, con 3:
/// `docs/superpowers/specs/2026-11-14-admin-hub-routing-design.md`). A failure
/// is only logged: the in-memory mirror already serves this turn.
fn persist_roster_mirror(state: &GatewayState, records: &[AdmissionRecord]) {
    if let Err(error) = state.database.save_local_setting(ROSTER_SETTING, records) {
        debug!("could not persist the roster mirror: {error:#}");
    }
}

/// Brings back the last mirror persisted on this machine. The revocation guard
/// still applies on read: `admitted_role` ignores an approval not newer than
/// the node's last ban, and revocations are durable in the trust chain.
fn restore_roster_mirror(state: &GatewayState) {
    let Ok(Some(raw)) = state.database.local_setting_json(ROSTER_SETTING) else {
        return;
    };
    match serde_json::from_str::<Vec<AdmissionRecord>>(&raw) {
        Ok(records) if !records.is_empty() => {
            state.network_chat.set_roster_mirror(records);
        }
        Ok(_) => {}
        Err(error) => {
            debug!("the persisted roster mirror is unreadable: {error:#}");
            let _ = state.database.delete_local_setting(ROSTER_SETTING);
        }
    }
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
    let sent =
        crate::admin::post_internal_json(state, &peer, DECIDE_PATH, decision, MEMBERSHIP_TIMEOUT)
            .await;
    match sent {
        Ok(response) if response.status().is_success() => {
            match response.json::<Vec<AdmissionRecord>>().await {
                Ok(records) => {
                    state.network_chat.set_roster_mirror(records.clone());
                    persist_roster_mirror(state, &records);
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
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
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

/// Ask admins in order until one hands over the relay passes it signs
/// for this node.
async fn request_membership(
    state: &GatewayState,
) -> anyhow::Result<Vec<crate::network::AnonymousRelay>> {
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
            RELAYS_PATH,
            &json!({}),
            MEMBERSHIP_TIMEOUT,
        )
        .await;
        match sent {
            Ok(response) if response.status().is_success() => {
                let answer = response.json::<Value>().await?;
                let relays = answer
                    .get("relays")
                    .cloned()
                    .and_then(|relays| serde_json::from_value(relays).ok())
                    .unwrap_or_default();
                return Ok(relays);
            }
            Ok(response) => {
                last = anyhow::anyhow!("{} answered {}", peer.node_id, response.status())
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Keeps this node's relay passes fresh, and its standing in the network
/// visible to the interface. An admin signs its own passes every tick. A
/// member asks an admin for them every tick; the passes are what let it dial
/// the network's visitor relays through the anonymous tunnel. Membership
/// itself is no longer carried by the node: the roster vouches, pushed and
/// exchanged by the other keepers, decide it on every node.
pub(crate) fn spawn_membership_keeper(state: GatewayState) {
    tokio::spawn(async move {
        let mut wait = Duration::ZERO;
        let mut backoff = RELAY_RETRY_MIN;
        loop {
            tokio::time::sleep(wait).await;
            wait = KEEPER_TICK;
            if state.network.is_detached() || is_admin(&state) {
                if !state.network.is_detached() {
                    // An admin signs its own passes, renewed every tick.
                    let relays = relay_passes(&state, &state.network.node_id).await;
                    state.network.set_anonymous_relays(relays);
                }
                note_membership_error(None);
                refresh_connection_phase(&state).await;
                continue;
            }
            // No admin heard yet: nobody to ask, so only look again soon.
            if !admin_in_sight(&state).await {
                note_membership_error(None);
                refresh_connection_phase(&state).await;
                wait = RELAY_RETRY_MIN;
                continue;
            }
            match request_membership(&state).await {
                Ok(relays) => {
                    state.network.set_anonymous_relays(relays);
                    note_membership_error(None);
                    backoff = RELAY_RETRY_MIN;
                    debug!("relay passes renewed");
                }
                Err(error) => {
                    note_membership_error(Some(format!("{error:#}")));
                    wait = backoff;
                    backoff = (backoff * 2).min(KEEPER_TICK);
                    debug!("could not renew the relay passes: {error:#}");
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
    /// An admin of its own chain, or off the network: it needs nothing.
    Standalone,
    /// A member that has heard no admin of its network yet.
    Discovering,
    /// An admin is in sight, but no roster vouch names this node yet.
    Authenticating,
    /// A member the roster vouches.
    Connected,
}

struct ConnectionTrack {
    phase: Option<ConnectionPhase>,
    since: i64,
    last_error: Option<String>,
}

fn connection_track() -> &'static std::sync::Mutex<ConnectionTrack> {
    static TRACK: std::sync::OnceLock<std::sync::Mutex<ConnectionTrack>> =
        std::sync::OnceLock::new();
    TRACK.get_or_init(|| {
        std::sync::Mutex::new(ConnectionTrack {
            phase: None,
            since: chrono::Utc::now().timestamp(),
            last_error: None,
        })
    })
}

/// The last refused membership request, kept to explain a wait.
fn note_membership_error(error: Option<String>) {
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
    } else if state
        .admin_trust
        .vouched_role(&state.network.node_id, chrono::Utc::now().timestamp())
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
/// (`ConnectionPhase`), since when (unix seconds), the last refused
/// membership request, and how much of the cluster it sees now: the other
/// live peers and whether an admin is among them.
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
/// list. Off a secondary the mirror is empty. The mirror is restored from
/// `pom.db` at the start and written back on every read, so a secondary that
/// restarts while the principal is offline keeps knowing the member list.
pub(crate) fn spawn_roster_keeper(state: GatewayState) {
    tokio::spawn(async move {
        if is_admin(&state) && !is_principal(&state) {
            restore_roster_mirror(&state);
        }
        let mut ticker = tokio::time::interval(KEEPER_TICK);
        loop {
            ticker.tick().await;
            if !is_admin(&state) || is_principal(&state) {
                state.network_chat.set_roster_mirror(Vec::new());
                let _ = state.database.delete_local_setting(ROSTER_SETTING);
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
    /// the admin and then holds its vouch; an admin needs none of them.
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
        assert_eq!(
            refresh_connection_phase(&admin).await.0,
            ConnectionPhase::Standalone
        );

        let visitor_key = ed25519_dalek::SigningKey::from_bytes(&[32; 32]);
        let mut visitor = crate::gateway::testkit::state_with("peer-visitor", vec![]).await;
        visitor.admin_trust = std::sync::Arc::new(
            crate::admin_trust::AdminTrust::in_memory(trust.document()).unwrap(),
        );
        visitor.authority_principal =
            AuthorityPrincipal::from_signing_key("visitor", "peer-visitor", &visitor_key);
        assert_eq!(
            refresh_connection_phase(&visitor).await.0,
            ConnectionPhase::Discovering
        );

        let mut heard = crate::gateway::testkit::peer(&principal.node_id, "principal", 8090);
        heard.pubkey = principal.pubkey.clone();
        visitor
            .network
            .peers
            .write()
            .await
            .insert(heard.node_id.clone(), heard);
        assert_eq!(
            refresh_connection_phase(&visitor).await.0,
            ConnectionPhase::Authenticating
        );

        // The vouch keeper has carried the admin's signature of this node's
        // approval: it is a member of the roster now.
        let now = chrono::Utc::now().timestamp();
        let vouch = crate::membership::sign_vouch(
            &principal_key,
            &principal,
            &trust.document(),
            &visitor.network.node_id,
            crate::config::EnrollmentRole::Visitor,
            now - 5,
            now,
        )
        .unwrap();
        visitor.admin_trust.merge_vouches(vec![vouch]);
        assert_eq!(
            refresh_connection_phase(&visitor).await.0,
            ConnectionPhase::Connected
        );
        let Json(status) = connection_status(State(visitor.clone())).await;
        assert_eq!(status["phase"], "connected");
        assert_eq!(status["peers"], 1);
        assert_eq!(status["admin_in_sight"], true);
    }
}

#[cfg(test)]
mod relay_pass_tests {
    use super::*;
    use crate::admin_trust::AuthorityPrincipal;
    use crate::profile::NodeProfile;

    /// An admin hands out a pass for each visitor relay that takes passes,
    /// and none for a worker's relay, one that does not, or the asker's own.
    #[tokio::test]
    async fn an_admin_hands_out_passes_for_visitor_relays_only() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[41; 32]);
        let principal = AuthorityPrincipal::from_signing_key("principal", "peer-principal", &key);
        let trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "pass-lab",
            principal.clone(),
            &key,
        )
        .unwrap();
        let offer = |url: &str, anonymous: bool| {
            Some(crate::network::RelayOffer {
                transfers: false,
                url: Some(url.to_string()),
                anonymous,
            })
        };
        let with = |id: &str, profile: NodeProfile, relay| {
            let mut peer = crate::gateway::testkit::peer(id, id, 8090);
            peer.profile = profile;
            // Each relays from its own address, but "liar" points elsewhere.
            let n = id.len() % 200;
            peer.addr = format!("198.51.100.{n}:42777");
            peer.addr_candidates = vec![peer.addr.clone()];
            peer.relay = relay;
            peer
        };
        let peers = vec![
            with(
                "visitor-relay",
                NodeProfile::Visitor,
                offer("http://198.51.100.13:52000/", true),
            ),
            with(
                "visitor-plain",
                NodeProfile::Visitor,
                offer("http://198.51.100.13:52000/", false),
            ),
            with(
                "worker-relay",
                NodeProfile::Worker,
                offer("http://198.51.100.12:52000/", true),
            ),
            with(
                "asker",
                NodeProfile::Visitor,
                offer("http://198.51.100.5:52000/", true),
            ),
            with(
                "liar",
                NodeProfile::Visitor,
                offer("http://203.0.113.9:52000/", true),
            ),
        ];
        let mut admin = crate::gateway::testkit::state_with("peer-principal", peers).await;
        admin.admin_trust = std::sync::Arc::new(
            crate::admin_trust::AdminTrust::in_memory(trust.document()).unwrap(),
        );
        admin.authority_principal = principal;
        admin.authority_signing_key = std::sync::Arc::new(key);

        let relays = relay_passes(&admin, "asker").await;
        assert_eq!(
            relays
                .iter()
                .map(|relay| relay.url.as_str())
                .collect::<Vec<_>>(),
            vec!["http://198.51.100.13:52000/"]
        );
        let now = chrono::Utc::now().timestamp();
        assert!(crate::relay_pass::verify(
            &relays[0].pass,
            &trust.document(),
            "http://198.51.100.13:52000/",
            now
        )
        .is_ok());

        // A peer whose announce said it runs with iroh off (`--no-iroh`) is
        // handed no passes: it holds no ephemeral endpoint and dials no
        // visitor relay (spec 2026-11-15).
        admin
            .network
            .peers
            .write()
            .await
            .get_mut("asker")
            .expect("asker is on the table")
            .direct_only = true;
        assert!(relay_passes(&admin, "asker").await.is_empty());

        // The same node asking for itself while direct-only: the same answer.
        let mut self_direct = crate::gateway::testkit::state_with("self-direct", vec![]).await;
        self_direct.admin_trust = std::sync::Arc::new(
            crate::admin_trust::AdminTrust::in_memory(trust.document()).unwrap(),
        );
        self_direct.network.set_direct_only(true);
        let own = self_direct.network.node_id.clone();
        assert!(relay_passes(&self_direct, &own).await.is_empty());
    }
}

#[cfg(test)]
mod roster_mirror_tests {
    use super::*;

    /// The mirror survives the process: a secondary admin that restarts while
    /// the principal is offline reads the member list back from `pom.db`
    /// (admin-hub design, con 3:
    /// `docs/superpowers/specs/2026-11-14-admin-hub-routing-design.md`).
    #[tokio::test]
    async fn the_roster_mirror_comes_back_from_the_database() {
        let state = crate::gateway::testkit::state_with_secondary_admin("secondary", vec![]).await;
        let records = vec![AdmissionRecord {
            node_id: "worker-1".into(),
            card: Default::default(),
            role: crate::config::EnrollmentRole::Worker,
            status: AdmissionStatus::Approved,
            requested_at: "2026-11-14T00:00:00Z".into(),
            decided_at: Some("2026-11-14T00:01:00Z".into()),
            account: None,
            validator: false,
        }];
        persist_roster_mirror(&state, &records);

        // A restart empties the in-memory mirror before the first read of a
        // principal that is not reachable yet.
        state.network_chat.set_roster_mirror(Vec::new());
        assert!(state.network_chat.roster_mirror().is_empty());

        restore_roster_mirror(&state);
        let mirror = state.network_chat.roster_mirror();
        assert_eq!(mirror.len(), 1);
        assert_eq!(mirror[0].node_id, "worker-1");
        // The restored mirror is the roster a serving gate consults.
        assert_eq!(
            state.network_chat.admitted_role("worker-1", None),
            Some(crate::config::EnrollmentRole::Worker)
        );
    }

    /// An unreadable persisted mirror is erased, not trusted: the next read of
    /// the principal replaces it.
    #[tokio::test]
    async fn an_unreadable_mirror_is_erased_and_not_restored() {
        let state = crate::gateway::testkit::state_with_secondary_admin("secondary", vec![]).await;
        state
            .database
            .save_local_setting(ROSTER_SETTING, &"not a list")
            .unwrap();
        restore_roster_mirror(&state);
        assert!(state.network_chat.roster_mirror().is_empty());
        assert!(state
            .database
            .local_setting_json(ROSTER_SETTING)
            .unwrap()
            .is_none());
    }
}

#[cfg(test)]
mod vouch_tests {
    use super::*;
    use crate::gateway::testkit;

    /// Phase C of the admin-hub routing (spec 10.1): the approval on the
    /// admin's list is signed as a vouch of the admitted role; a request
    /// that was not decided and an approval a ban cut are not signed.
    #[tokio::test]
    async fn an_approval_is_signed_as_a_vouch_on_the_admin_that_holds_it() {
        let state = testkit::state_with("principal", vec![]).await;
        state
            .network_chat
            .request_admission(
                "member",
                Default::default(),
                crate::config::EnrollmentRole::Worker,
            )
            .unwrap();
        assert!(state
            .network_chat
            .decide_admission("member", Some(AdmissionStatus::Approved)));
        let vouches = sign_vouches(&state).await;
        assert_eq!(vouches.len(), 1);
        assert_eq!(vouches[0].sub, "member");
        assert_eq!(vouches[0].role, crate::config::EnrollmentRole::Worker);
        crate::membership::verify_vouch(
            &vouches[0],
            &state.admin_trust.document(),
            chrono::Utc::now().timestamp(),
        )
        .unwrap();

        // A request no one decided is not an admission, and is not vouched.
        state
            .network_chat
            .request_admission(
                "candidate",
                Default::default(),
                crate::config::EnrollmentRole::Visitor,
            )
            .unwrap();
        assert!(sign_vouches(&state)
            .await
            .iter()
            .all(|v| v.sub != "candidate"));

        // A ban cuts the approval: the admin stops vouching the member.
        let revocation = crate::membership::revoke(
            &state.authority_signing_key,
            &state.authority_principal,
            &state.admin_trust.document(),
            "member",
            chrono::Utc::now().timestamp(),
        )
        .unwrap();
        state.admin_trust.merge_revocations(vec![revocation]);
        assert!(sign_vouches(&state).await.iter().all(|v| v.sub != "member"));
    }

    /// The exchange merges by signature, never by sender: what no admin
    /// signed holds nothing, whoever carried it.
    #[tokio::test]
    async fn the_vouch_exchange_merges_by_signature_not_by_sender() {
        let state = testkit::state_with("entry", vec![]).await;
        let now = chrono::Utc::now().timestamp();
        let real = crate::membership::sign_vouch(
            &state.authority_signing_key,
            &state.authority_principal,
            &state.admin_trust.document(),
            "member",
            crate::config::EnrollmentRole::Worker,
            now - 10,
            now,
        )
        .unwrap();
        let mut forged = real.clone();
        forged.sub = "impostor".into();
        forged.signature = "00".repeat(64);
        let response = internal_vouches(
            InternalNodeAuth { principal: None },
            State(state.clone()),
            Json(vec![forged, real]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            state.admin_trust.vouched_role("member", now),
            Some(crate::config::EnrollmentRole::Worker)
        );
        assert_eq!(state.admin_trust.vouched_role("impostor", now), None);
    }

    /// The approval route vouches the network at once, the way a ban pushes
    /// its revocation: the principal's own ledger holds the new member.
    #[tokio::test]
    async fn deciding_an_approval_publishes_the_vouch_into_the_admin_ledger() {
        let state = testkit::state_with("principal", vec![]).await;
        state
            .network_chat
            .request_admission(
                "member",
                Default::default(),
                crate::config::EnrollmentRole::Worker,
            )
            .unwrap();
        let response = apply_decision(
            &state,
            AdmissionDecision {
                node_id: "member".into(),
                decision: "approve".into(),
                grant_micro: None,
            },
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // The publish runs on its own task, as the ban's push does; wait for
        // its effect on this node's ledger.
        for _ in 0..50 {
            if state
                .admin_trust
                .vouched_role("member", chrono::Utc::now().timestamp())
                .is_some()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the approval did not vouch the member");
    }

    /// `POST /api/internal/membership/relays` - the route that carries the
    /// passes a member needs to dial the network's visitor relays. It
    /// answers only a member the admin's roster admits, an admin signs its
    /// own passes, and a machine nobody admitted gets nothing.
    #[tokio::test]
    async fn the_relay_route_answers_only_members_the_roster_admits() {
        let state = testkit::state_with("principal", vec![]).await;
        let member = crate::admin_trust::AuthorityPrincipal {
            node_id: "member-1".into(),
            peer_id: String::new(),
            pubkey: "ab".repeat(32),
        };

        // Not admitted: nothing.
        let response = internal_relays(
            InternalNodeAuth {
                principal: Some(member.clone()),
            },
            State(state.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(testkit::body_of(response).await["reason"], "not_admitted");

        // Approved on the list: the passes it signs for it, with it. No
        // visitor relays are in sight here, so the list is empty - what a
        // member behind NAT reaches through the tunnel's allow list
        // (`infer_tunnel`).
        state
            .network_chat
            .request_admission(
                "member-1",
                Default::default(),
                crate::config::EnrollmentRole::Worker,
            )
            .unwrap();
        assert!(state
            .network_chat
            .decide_admission("member-1", Some(AdmissionStatus::Approved)));
        let response = internal_relays(
            InternalNodeAuth {
                principal: Some(member.clone()),
            },
            State(state.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(testkit::body_of(response).await["relays"], json!([]));

        // A ban is final for this answer, as it was in the token route it
        // replaces: the admin asks for none and gets none.
        let revocation = crate::membership::revoke(
            &state.authority_signing_key,
            &state.authority_principal,
            &state.admin_trust.document(),
            "member-1",
            chrono::Utc::now().timestamp(),
        )
        .unwrap();
        state.admin_trust.merge_revocations(vec![revocation]);
        let response = internal_relays(
            InternalNodeAuth {
                principal: Some(member),
            },
            State(state.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(testkit::body_of(response).await["reason"], "not_admitted");

        // An admin signs its own passes every keeper tick: it asks for none.
        let response = internal_relays(
            InternalNodeAuth {
                principal: Some(state.authority_principal.clone()),
            },
            State(state.clone()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            testkit::body_of(response).await["reason"],
            "admins_sign_their_own_passes"
        );
    }
}
