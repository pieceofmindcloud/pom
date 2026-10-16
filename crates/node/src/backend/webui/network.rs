//! Shared network information and administrator editing.
//!
//! The network block is deliberately a first-class control-plane document. A
//! node stores it locally for an always-available read view, while an admin
//! edit is signed and fanned out to the other peers. Public-directory
//! publication is a separate side effect owned by the gateway state.

use super::*;
use crate::auth::{ConnectionAuth, InternalNodeAuth};
use crate::config::{self, NetworkSettings, NetworkVisibility};
use axum::extract::State;

const NETWORK_SETTINGS_PATH: &str = "/api/internal/network/settings";
pub(crate) const NETWORK_SETTINGS_POLICY_PATH: &str = "/api/cluster/network-settings";

#[derive(Debug, Serialize)]
pub struct NetworkInfoView {
    pub network: NetworkSettings,
    pub can_edit: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct NetworkSettingsEnvelope {
    pub network: NetworkSettings,
    /// Only the shared secret for private networks follows the signed control
    /// envelope. A public node keeps a local admin credential in its config,
    /// but that credential is never sent to peers or shown in this card.
    pub connection_key: String,
    /// When `connection_key` replaces another: the Unix second from which
    /// every node sends the new key (`auth::begin_key_transition`). Absent
    /// when the key is not being rotated.
    #[serde(default)]
    pub key_effective_at: Option<i64>,
}

/// Read-only for node and infer, with the same response used by the admin
/// editor. The capability bit is derived from the signed authority chain, not
/// from the profile string alone.
pub(crate) async fn network_info(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    Json(network_info_view(&state).await).into_response()
}

/// Return the authoritative network document to a node that joined after the
/// last admin edit. Public joins intentionally carry only directory metadata;
/// this signed control read supplies the rest of the read-only UI state.
pub(crate) async fn network_settings_policy(
    _auth: InternalNodeAuth,
    State(state): State<GatewayState>,
) -> Response {
    let configured_network = state.auth.read().await.network.clone();
    let network = state
        .database
        .network_settings()
        .ok()
        .flatten()
        .unwrap_or(configured_network);
    // The key never leaves through this read: it is answered to any
    // well-formed signed envelope, and signing one only takes the trust
    // document. A member that needs the current key claims it from the
    // principal, which checks the member list (`network_lobby::ClaimKey`).
    let connection_key = String::new();
    Json(NetworkSettingsEnvelope {
        network,
        connection_key,
        key_effective_at: None,
    })
    .into_response()
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct GeoCitiesQuery {
    country: String,
    #[serde(default)]
    q: String,
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct GeoNearestQuery {
    latitude: f64,
    longitude: f64,
    limit: Option<usize>,
}

fn geo_unavailable(error: String) -> Response {
    network_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "geo_database_failed",
        &error,
    )
}

pub(super) fn geo_countries_response() -> Response {
    match crate::geo::database() {
        Ok(database) => Json(json!({
            "countries": database.countries(),
            "release": crate::geo::RELEASE,
        }))
        .into_response(),
        Err(error) => geo_unavailable(error),
    }
}

pub(super) fn geo_cities_response(query: GeoCitiesQuery) -> Response {
    match crate::geo::database() {
        Ok(database) => Json(json!({
            "cities": database.search(&query.country, &query.q, query.limit),
        }))
        .into_response(),
        Err(error) => geo_unavailable(error),
    }
}

pub(super) fn geo_nearest_response(query: GeoNearestQuery) -> Response {
    match crate::geo::database() {
        Ok(database) => Json(json!({
            "cities": database.nearest(query.latitude, query.longitude, query.limit),
        }))
        .into_response(),
        Err(error) => geo_unavailable(error),
    }
}

/// Reads of the offline city database (`geo.rs`) for the location editor.
/// They answer from the binary: nothing is looked up on the internet and
/// nothing is stored.
pub(crate) async fn geo_countries(_auth: ConnectionAuth) -> Response {
    geo_countries_response()
}

/// Cities of one country matching the typed text.
pub(crate) async fn geo_cities(
    _auth: ConnectionAuth,
    axum::extract::Query(query): axum::extract::Query<GeoCitiesQuery>,
) -> Response {
    geo_cities_response(query)
}

/// The cities closest to a coordinate, which is how a pin becomes a city.
pub(crate) async fn geo_nearest(
    _auth: ConnectionAuth,
    axum::extract::Query(query): axum::extract::Query<GeoNearestQuery>,
) -> Response {
    geo_nearest_response(query)
}

pub(crate) async fn save_network_settings(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(settings): Json<NetworkSettings>,
) -> Response {
    if !super::local_is_primary(&state).await {
        return network_error(
            StatusCode::FORBIDDEN,
            "admin_authority_required",
            "only an authorized administrator can edit the network",
        );
    }

    match apply_network_settings_with_key(&state, settings, true, None, None).await {
        Ok(synced_peers) => {
            let view = network_info_view(&state).await;
            Json(json!({
                "network": view.network,
                "can_edit": view.can_edit,
                "synced_peers": synced_peers,
            }))
            .into_response()
        }
        Err(error) => network_error(StatusCode::BAD_REQUEST, "network_settings_invalid", &error),
    }
}

async fn apply_network_settings_with_key(
    state: &GatewayState,
    mut settings: NetworkSettings,
    fanout: bool,
    incoming_connection_key: Option<&str>,
    key_effective_at: Option<i64>,
) -> Result<usize, String> {
    settings.public_name = settings.public_name.trim().to_string();
    settings.organization = settings.organization.trim().to_string();
    settings.description = settings.description.trim().to_string();
    settings.rules = settings.rules.trim().to_string();
    settings.channels = settings.channels.trim().to_string();
    crate::geo::apply_location(&mut settings)?;
    settings.accepted_enrollments = settings.accepted_enrollments.normalized();
    let discovery_before = state.auth.read().await.network.visibility != NetworkVisibility::Local;
    let discovery_after = settings.visibility != NetworkVisibility::Local;
    if settings.visibility == NetworkVisibility::Public {
        settings.geo_label = config::geo_network_label(
            &settings.country_code,
            &settings.city,
            &state.network.node_id,
        );
    } else {
        settings.geo_label.clear();
    }
    config::validate_network_settings(&settings).map_err(|error| error.to_string())?;

    let connection_key = {
        let mut cfg = state.auth.write().await;
        cfg.network = settings.clone();
        if let Some(incoming) = incoming_connection_key {
            let incoming = incoming.trim();
            if !incoming.is_empty() && incoming != cfg.connection_key {
                // A rotation: keep the old key in use until the instant the
                // principal named, so nothing in flight is refused.
                if let Some(effective_at) = key_effective_at {
                    crate::auth::begin_key_transition(&cfg.connection_key, incoming, effective_at);
                }
                cfg.connection_key = incoming.to_string();
            }
        }
        if cfg.connection_key.trim().is_empty() {
            cfg.connection_key = crate::webui::fresh_connection_key();
        }
        cfg.save()
            .map_err(|error| format!("save network configuration: {error:#}"))?;
        cfg.connection_key.clone()
    };
    state
        .database
        .save_network_settings(&settings)
        .map_err(|error| format!("save network details: {error:#}"))?;

    // Discovery is the difference between `local` and the other two: off, the
    // node announces nothing and no key lookup finds it; on, it goes looking
    // for the network again.
    if discovery_before && !discovery_after {
        state.network.detach().await;
    } else if !discovery_before && discovery_after {
        let assignment = state.local_assignment.read().await.clone();
        if let Err(error) = state.network.rejoin(assignment.as_ref()).await {
            warn!("could not resume announcing the network: {error:#}");
        }
    }

    if let Some(publisher) = &state.pkarr_publisher {
        if let Err(error) = publisher.set_geo_label(&settings.geo_label).await {
            warn!("could not republish the geo label on pkarr: {error:#}");
        }
    }

    if !fanout {
        return Ok(0);
    }
    Ok(fanout_network_settings(state, &settings, &connection_key, None, None).await)
}

async fn fanout_network_settings(
    state: &GatewayState,
    settings: &NetworkSettings,
    connection_key: &str,
    exclude: Option<&str>,
    key_effective_at: Option<i64>,
) -> usize {
    let mut synced = 0;
    for peer in state.all_peers().await {
        if peer.node_id == state.network.node_id || Some(peer.node_id.as_str()) == exclude {
            continue;
        }
        let Ok(client) = state.peer_http.for_peer(&peer.node_id) else {
            warn!(peer = %peer.node_id, "could not create a client to sync the network details");
            continue;
        };
        let mut updated = false;
        for base in state.gateway_bases(&peer) {
            let request = match state
                .control_json_request(
                    client
                        .post(format!("{base}{NETWORK_SETTINGS_PATH}"))
                        .timeout(crate::webui::PEER_TIMEOUT),
                    NETWORK_SETTINGS_PATH,
                    &NetworkSettingsEnvelope {
                        network: settings.clone(),
                        connection_key: if !settings.open_admission() {
                            connection_key.to_string()
                        } else {
                            String::new()
                        },
                        key_effective_at,
                    },
                )
                .await
            {
                Ok(request) => request,
                Err(error) => {
                    warn!(peer = %peer.node_id, "could not sign the network details: {error:#}");
                    break;
                }
            };
            if request
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                updated = true;
                break;
            }
        }
        if updated {
            synced += 1;
        } else {
            warn!(peer = %peer.node_id, "peer did not confirm the updated network details");
        }
    }
    synced
}

pub(crate) const MEMBER_LEFT_PATH: &str = "/api/cluster/member-left";

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct MemberLeft {
    pub node_id: String,
}

/// Tells the principal of the network this node is in that it is leaving.
/// Signed with this node's own key, so a node only ever announces its own
/// departure. Best effort, with a short deadline.
pub(crate) async fn notify_principal_of_departure(state: &GatewayState) {
    let primary = state.admin_trust.current_primary();
    if primary.pubkey == state.authority_principal.pubkey {
        return;
    }
    // The cluster's own control channel first, when this node sees the
    // principal as a peer.
    let peer = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == primary.node_id && peer.pubkey == primary.pubkey);
    if let Some(peer) = peer {
        let body = MemberLeft {
            node_id: state.network.node_id.clone(),
        };
        match state
            .control_post_json(&peer, MEMBER_LEFT_PATH, &body, Duration::from_secs(5))
            .await
        {
            Ok(response) if response.status().is_success() => return,
            Ok(response) => warn!(
                "leaving the network: the principal answered {} to the notice",
                response.status()
            ),
            Err(error) => {
                warn!("leaving the network: the principal did not receive the notice: {error:#}")
            }
        }
    }
    // Otherwise by the principal's key, over the lobby connection. That one
    // needs neither side to have admitted the other, which is exactly the
    // case of a node that joined and was never seen as a peer: without it
    // the principal kept the approval and the node could walk back in.
    let Some(key) = crate::pkarr_node::PkarrNode::public_key_uri_from_hex(&primary.pubkey) else {
        return;
    };
    let Ok(client) = crate::network_lobby::client().await else {
        return;
    };
    let request = crate::network_lobby::LobbyRequest::Leave {
        identity: client.proof(),
    };
    match tokio::time::timeout(Duration::from_secs(10), client.request(&key, request)).await {
        Ok(Ok(response)) if response.accepted => {}
        Ok(Ok(response)) => warn!(
            "leaving the network: the principal refused the notice: {:?}",
            response.error
        ),
        Ok(Err(error)) => warn!("leaving the network: the principal is unreachable: {error:#}"),
        Err(_) => warn!("leaving the network: the notice to the principal exceeded the deadline"),
    }
}

/// Rotates the connection key for departures the swarm loop heard on the
/// lobby connection (`LobbyRequest::Leave`): it can record them, but pushing
/// a new key is the gateway's work.
pub(crate) fn spawn_departure_worker(state: GatewayState) {
    tokio::spawn(async move {
        loop {
            for node_id in state.network_chat.rotations_wanted().await {
                match rotate_connection_key(&state, &node_id).await {
                    Ok(Some(synced)) => {
                        info!(node = %node_id, synced, "member left: connection key renewed")
                    }
                    Ok(None) => {}
                    Err(error) => warn!("could not renew the connection key: {error}"),
                }
            }
        }
    });
}

/// `POST /api/cluster/member-left` - a member says it is leaving. Only the
/// principal acts on it, and only for the member that signed the request:
/// the issuer must be the node named in the body and a peer this node
/// currently admits. Its admission is forgotten and, unless the network is in
/// the open mode, the connection key is replaced and pushed to everyone
/// else, so the departed machine has to ask to be admitted again.
pub(crate) async fn member_left(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Json(body): Json<MemberLeft>,
) -> Response {
    if state.admin_trust.current_primary().pubkey != state.authority_principal.pubkey {
        return network_error(
            StatusCode::CONFLICT,
            "not_principal",
            "only the principal handles a departure",
        );
    }
    let Some(issuer) = auth.principal() else {
        return network_error(
            StatusCode::FORBIDDEN,
            "signature_required",
            "a departure must be signed by the node that is leaving",
        );
    };
    // The issuer's `node_id` has to be the one its key gives, or the member
    // list (keyed by `node_id`) could be matched under a borrowed name.
    let named_by_key = crate::session::node_id_matches_pubkey(&issuer.node_id, &issuer.pubkey);
    let is_member = state.network_chat.admission_of(&body.node_id).is_some()
        || state
            .network
            .alive_peers()
            .await
            .iter()
            .any(|peer| peer.node_id == body.node_id && peer.pubkey == issuer.pubkey);
    if issuer.node_id != body.node_id || !named_by_key || !is_member {
        return network_error(
            StatusCode::FORBIDDEN,
            "not_the_member",
            "a node can only announce its own departure from a network it is in",
        );
    }
    state.network_chat.member_departed(&body.node_id);
    match rotate_connection_key(&state, &body.node_id).await {
        Ok(Some(synced)) => {
            info!(node = %body.node_id, synced, "member left: connection key renewed");
            Json(json!({ "rotated": true, "synced_peers": synced })).into_response()
        }
        Ok(None) => Json(json!({ "rotated": false })).into_response(),
        Err(error) => network_error(StatusCode::INTERNAL_SERVER_ERROR, "rotation_failed", &error),
    }
}

/// Replaces the network's connection key and pushes it, in the admin-signed
/// settings envelope, to every admitted peer except `departed`. `None` when
/// the network admits without a key and there is nothing to rotate.
async fn rotate_connection_key(
    state: &GatewayState,
    departed: &str,
) -> Result<Option<usize>, String> {
    let effective_at = chrono::Utc::now().timestamp() + crate::auth::KEY_TRANSITION_LEAD_SECS;
    let (settings, connection_key) = {
        let mut config = state.auth.write().await;
        if config.network.open_admission() {
            return Ok(None);
        }
        let previous = std::mem::replace(
            &mut config.connection_key,
            crate::webui::fresh_connection_key(),
        );
        // Everyone, this node included, keeps sending the old key until
        // `effective_at` and accepts both a while longer: the rotation cuts
        // nothing that is running.
        crate::auth::begin_key_transition(&previous, &config.connection_key, effective_at);
        config
            .save()
            .map_err(|error| format!("save the new connection key: {error:#}"))?;
        (config.network.clone(), config.connection_key.clone())
    };
    let settings = state
        .database
        .network_settings()
        .ok()
        .flatten()
        .unwrap_or(settings);
    Ok(Some(
        fanout_network_settings(
            state,
            &settings,
            &connection_key,
            Some(departed),
            Some(effective_at),
        )
        .await,
    ))
}

/// Keeps the principal's member list complete: every peer it currently
/// admits has an approved record, so it can claim the key after a rotation.
/// This is what puts members that joined before admission requests on the
/// list. Only the principal of a key-gated network keeps one.
pub(crate) fn spawn_member_list_keeper(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            if state.admin_trust.current_primary().pubkey != state.authority_principal.pubkey
                || state.auth.read().await.network.open_admission()
            {
                continue;
            }
            for peer in state.network.alive_peers().await {
                let role = if peer.profile == crate::profile::NodeProfile::Infer {
                    crate::config::EnrollmentRole::Infer
                } else {
                    crate::config::EnrollmentRole::Node
                };
                if state
                    .network_chat
                    .record_member(&peer.node_id, &peer.hostname, role)
                {
                    info!(node = %peer.node_id, "member recorded in the network list");
                }
            }
        }
    });
}

/// On a member: when the principal is not among the admitted peers - the
/// usual sign that this node's connection key is no longer the network's -
/// asks the principal for the current key and adopts it. A member that was
/// offline during a rotation gets back in by itself this way; one that was
/// removed is answered without a grant and stays out.
pub(crate) fn spawn_key_reclaimer(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            if state.network.is_detached() {
                continue;
            }
            let primary = state.admin_trust.current_primary();
            if primary.pubkey == state.authority_principal.pubkey {
                continue;
            }
            if state
                .network
                .alive_peers()
                .await
                .iter()
                .any(|peer| peer.node_id == primary.node_id)
            {
                continue;
            }
            let Some(key) = crate::pkarr_node::PkarrNode::public_key_uri_from_hex(&primary.pubkey)
            else {
                continue;
            };
            let Ok(client) = crate::network_lobby::client().await else {
                continue;
            };
            let request = crate::network_lobby::LobbyRequest::ClaimKey {
                identity: client.proof(),
            };
            let granted = match client.request(&key, request).await {
                Ok(response) => response.admission.and_then(|view| view.grant),
                Err(error) => {
                    tracing::debug!("network key: the principal did not answer: {error:#}");
                    continue;
                }
            };
            let Some(grant) = granted else {
                tracing::debug!("network key: this machine is not in the member list");
                continue;
            };
            let new_key = grant.connection_key.trim();
            let mut config = state.auth.write().await;
            if new_key.is_empty() || config.connection_key == new_key {
                continue;
            }
            config.connection_key = new_key.to_string();
            match config.save() {
                Ok(()) => info!("network connection key renewed from the principal"),
                Err(error) => warn!("could not save the new connection key: {error:#}"),
            }
        }
    });
}

pub(crate) async fn internal_network_settings(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Json(envelope): Json<NetworkSettingsEnvelope>,
) -> Response {
    // Network settings come from the principal alone.
    let is_admin = auth
        .principal()
        .is_some_and(|issuer| crate::gateway::issuer_is_principal(&state, issuer));
    if !is_admin {
        return network_error(
            StatusCode::FORBIDDEN,
            "admin_authority_required",
            "the network change must be signed by an administrative authority",
        );
    }
    match apply_network_settings_with_key(
        &state,
        envelope.network,
        false,
        Some(&envelope.connection_key),
        envelope.key_effective_at,
    )
    .await
    {
        Ok(_) => Json(json!({ "saved": true })).into_response(),
        Err(error) => network_error(StatusCode::BAD_REQUEST, "network_settings_invalid", &error),
    }
}

/// Ask the current principal for the complete network document after a node
/// first sees its authenticated Announce. The task is deliberately attached
/// to `GatewayState` so it can persist both Config and the local DB snapshot,
/// exactly like the admin fan-out path.
pub(crate) fn spawn_network_settings_ratchet(state: GatewayState) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
    state.network.set_network_settings_probe(tx);
    tokio::spawn(async move {
        let mut last_attempt: Option<(String, Instant)> = None;
        // Consecutive failures: the trigger arrives on every Announce (~5 s)
        // until the sync succeeds, and only the 1st, 2nd, 4th, 8th... becomes
        // a `warn`.
        let mut failures: u32 = 0;
        while let Some((principal, announced_tls)) = rx.recv().await {
            if last_attempt
                .as_ref()
                .is_some_and(|(previous, at)| previous == &principal && at.elapsed() < PEER_TIMEOUT)
            {
                continue;
            }
            last_attempt = Some((principal.clone(), Instant::now()));

            let Some(peer) = state
                .network
                .alive_peers()
                .await
                .into_iter()
                .find(|peer| peer.node_id == principal)
            else {
                continue;
            };
            let probe = crate::peer_client::PeerClients::new(PEER_TIMEOUT);
            probe.set_tls(announced_tls);
            let Ok(client) = probe.for_peer(&principal) else {
                warn!(principal = %principal, "could not create a client to sync the network");
                continue;
            };
            let mut fetched = None;
            for base in state.network.order_gateway_bases(
                &peer.node_id,
                peer.gateway_candidates_with_tls(announced_tls),
            ) {
                let Ok(request) = state
                    .control_get_request(
                        client.get(format!("{base}{NETWORK_SETTINGS_POLICY_PATH}")),
                        NETWORK_SETTINGS_POLICY_PATH,
                    )
                    .await
                else {
                    continue;
                };
                let response = tokio::time::timeout(PEER_TIMEOUT, request.send()).await;
                let Ok(Ok(response)) = response else {
                    continue;
                };
                if !response.status().is_success() {
                    continue;
                }
                if let Ok(envelope) = response.json::<NetworkSettingsEnvelope>().await {
                    fetched = Some(envelope);
                    break;
                }
            }
            // No direct address answered: the libp2p tunnel, like the other
            // policy ratchets. A node that only reaches the principal through
            // the relay (Docker bridge, NAT) has no other path.
            let mut tunnel_error = None;
            if fetched.is_none() {
                match state
                    .control_get_via_tunnel(&peer, NETWORK_SETTINGS_POLICY_PATH, PEER_TIMEOUT)
                    .await
                {
                    Ok(response) if response.status().is_success() => {
                        match response.json::<NetworkSettingsEnvelope>().await {
                            Ok(envelope) => fetched = Some(envelope),
                            Err(error) => tunnel_error = Some(format!("invalid response: {error}")),
                        }
                    }
                    Ok(response) => tunnel_error = Some(format!("HTTP {}", response.status())),
                    Err(error) => tunnel_error = Some(format!("{error:#}")),
                }
            }

            let Some(envelope) = fetched else {
                failures = failures.saturating_add(1);
                let reason = tunnel_error.unwrap_or_else(|| "no route".to_string());
                if failures.is_power_of_two() {
                    warn!(
                        principal = %principal,
                        "could not fetch the network details from the principal (consecutive failure #{failures}): {reason}"
                    );
                } else {
                    tracing::debug!(
                        principal = %principal,
                        "could not fetch the network details from the principal (consecutive failure #{failures}): {reason}"
                    );
                }
                continue;
            };
            failures = 0;
            match apply_network_settings_with_key(
                &state,
                envelope.network,
                false,
                Some(&envelope.connection_key),
                envelope.key_effective_at,
            )
            .await
            {
                Ok(_) => {
                    state.network.mark_network_settings_synced();
                    info!(principal = %principal, "complete network details adopted from the principal");
                }
                Err(error) => {
                    warn!(principal = %principal, "network details received but invalid: {error}");
                }
            }
        }
    });
}

async fn network_info_view(state: &GatewayState) -> NetworkInfoView {
    let config = state.auth.read().await;
    let network = state
        .database
        .network_settings()
        .ok()
        .flatten()
        .unwrap_or_else(|| config.network.clone());
    let can_edit = state
        .admin_trust
        .effective_authority(&state.authority_principal, config.profile)
        == Some(crate::admin_trust::EffectiveAuthority::Primary);
    NetworkInfoView { network, can_edit }
}

fn network_error(status: StatusCode, reason: &'static str, message: &str) -> Response {
    (status, Json(json!({ "error": message, "reason": reason }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::testkit::{body_of, state_with};

    #[tokio::test]
    async fn network_info_reads_the_database_snapshot_for_every_profile() {
        let state = state_with("eu", vec![]).await;
        let settings = NetworkSettings {
            public_name: "Rede Curitiba".into(),
            organization: "POM".into(),
            description: "Rede local de teste".into(),
            country_code: "BR".into(),
            country_name: "Brasil".into(),
            city: "Curitiba".into(),
            ..NetworkSettings::default()
        };
        state.database.save_network_settings(&settings).unwrap();

        let body = body_of(network_info(ConnectionAuth, State(state)).await).await;
        assert_eq!(body["network"]["public_name"], "Rede Curitiba");
        assert_eq!(body["network"]["city"], "Curitiba");
        assert_eq!(body["can_edit"], false);
    }

    #[tokio::test]
    async fn policy_read_returns_full_public_metadata_without_the_connection_key() {
        let state = state_with("admin", vec![]).await;
        let settings = NetworkSettings {
            public_name: "Rede Curitiba".into(),
            organization: "POM".into(),
            description: "Inferência distribuída".into(),
            rules: "Respeite a rede".into(),
            channels: "Discord: pom".into(),
            country_code: "BR".into(),
            country_name: "Brasil".into(),
            city: "Curitiba".into(),
            visibility: NetworkVisibility::Public,
            ..NetworkSettings::default()
        };
        state.database.save_network_settings(&settings).unwrap();

        let response = network_settings_policy(
            InternalNodeAuth {
                principal: Some(state.authority_principal.clone()),
            },
            State(state),
        )
        .await;
        let body = body_of(response).await;

        assert_eq!(body["network"]["public_name"], "Rede Curitiba");
        assert_eq!(body["network"]["organization"], "POM");
        assert_eq!(body["network"]["description"], "Inferência distribuída");
        assert_eq!(body["network"]["rules"], "Respeite a rede");
        assert_eq!(body["network"]["channels"], "Discord: pom");
        assert_eq!(body["network"]["visibility"], "public");
        assert_eq!(body["network"]["country_name"], "Brasil");
        assert_eq!(body["network"]["city"], "Curitiba");
        assert_eq!(body["connection_key"], "");
    }

    #[tokio::test]
    async fn a_departure_is_only_accepted_from_the_member_that_is_leaving() {
        let state = state_with("admin", vec![]).await;
        let key_before = state.auth.read().await.connection_key.clone();
        let leaving = || {
            Json(MemberLeft {
                node_id: "quem-sai".into(),
            })
        };

        // Not signed: the shared key alone cannot say who is leaving.
        let unsigned = member_left(
            InternalNodeAuth { principal: None },
            State(state.clone()),
            leaving(),
        )
        .await;
        assert_eq!(unsigned.status(), StatusCode::FORBIDDEN);

        // Signed by someone else, about a node that is not a member here.
        let stranger = member_left(
            InternalNodeAuth {
                principal: Some(state.authority_principal.clone()),
            },
            State(state.clone()),
            leaving(),
        )
        .await;
        assert_eq!(stranger.status(), StatusCode::FORBIDDEN);
        // Neither attempt cost the network its key.
        assert_eq!(state.auth.read().await.connection_key, key_before);
    }

    #[tokio::test]
    async fn saving_a_public_network_requires_the_geo_fields() {
        let state = state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        let response = save_network_settings(
            ConnectionAuth,
            State(state),
            Json(NetworkSettings {
                public_name: "Rede sem cidade".into(),
                visibility: NetworkVisibility::Public,
                ..NetworkSettings::default()
            }),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_pin_is_answered_with_the_cities_around_it() {
        let response = geo_nearest_response(GeoNearestQuery {
            latitude: -25.43,
            longitude: -49.27,
            limit: Some(2),
        });
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let cities = body["cities"].as_array().unwrap();
        assert_eq!(cities.len(), 2);
        assert_eq!(cities[0]["name"], "Curitiba");
        assert_eq!(cities[0]["country_code"], "BR");
        assert!(cities[0]["distance_km"].as_f64().unwrap() < 5.0);

        let response = geo_cities_response(GeoCitiesQuery {
            country: "br".into(),
            q: "curitib".into(),
            limit: None,
        });
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["cities"][0]["name"], "Curitiba");
    }

    #[tokio::test]
    async fn changing_visibility_preserves_the_internal_credential() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = state_with("admin", vec![]).await;
        struct RestoreConfig(Option<std::ffi::OsString>);
        impl Drop for RestoreConfig {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("POM_CONFIG", value),
                    None => std::env::remove_var("POM_CONFIG"),
                }
            }
        }
        let _restore_config = RestoreConfig(std::env::var_os("POM_CONFIG"));
        let config_path = std::env::temp_dir().join(format!(
            "pom-network-visibility-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::env::set_var("POM_CONFIG", &config_path);

        let private = NetworkSettings {
            public_name: "Rede de teste".into(),
            visibility: NetworkVisibility::Private,
            ..NetworkSettings::default()
        };
        apply_network_settings_with_key(&state, private.clone(), false, None, None)
            .await
            .expect("rede privada inicial");
        assert_eq!(state.auth.read().await.connection_key, "chave");

        let public = NetworkSettings {
            city_id: Some(crate::geo::test_city_id()),
            visibility: NetworkVisibility::Public,
            ..private.clone()
        };
        apply_network_settings_with_key(&state, public, false, None, None)
            .await
            .expect("transition to a public network");
        let public_config = state.auth.read().await;
        assert_eq!(public_config.connection_key, "chave");
        assert_eq!(
            crate::auth::internal_node_token(&public_config).as_deref(),
            Some("chave")
        );
        drop(public_config);

        apply_network_settings_with_key(&state, private, false, None, None)
            .await
            .expect("return to a private network");
        let private_config = state.auth.read().await;
        assert_eq!(private_config.connection_key, "chave");
        assert_eq!(
            crate::auth::internal_node_token(&private_config).as_deref(),
            Some("chave")
        );
        drop(private_config);

        std::fs::remove_file(config_path).ok();
    }
}
