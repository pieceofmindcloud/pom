//! The candidate's side of the lobby over HTTP (`network/network_lobby.rs`).
//!
//! These routes make **this** node talk to another network's principal, by
//! its `pk:` key, before joining it. They exist twice, over the same code: on
//! a configured node, behind the operator's session, and in setup mode, on the
//! loopback-only setup router - a machine that has not joined anything yet is
//! exactly the one that wants to ask first. Nothing here touches the local
//! cluster.

use super::*;
use crate::network_lobby::{self, LobbyCard, LobbyRequest};

#[derive(Debug, Deserialize)]
pub struct LobbyQuery {
    /// `pk:` key of the principal publishing the network.
    pub key: String,
}

#[derive(Debug, Deserialize)]
pub struct LobbyPost {
    pub key: String,
    pub message_id: String,
    pub text: String,
}

/// How long the sampled card is reused. A poll every few seconds must not
/// re-run the GPU probes behind `sysmon::sample`.
const CARD_TTL: Duration = Duration::from_secs(60);

async fn local_card() -> LobbyCard {
    static CARD: OnceLock<std::sync::Mutex<Option<(Instant, LobbyCard)>>> = OnceLock::new();
    let cache = CARD.get_or_init(|| std::sync::Mutex::new(None));
    if let Some((at, card)) = cache.lock().expect("lobby card lock").clone() {
        if at.elapsed() < CARD_TTL {
            return card;
        }
    }
    let card = LobbyCard::from_system(&sysmon::sample().await);
    *cache.lock().expect("lobby card lock") = Some((Instant::now(), card.clone()));
    card
}

/// What this node asks of a lobby in one exchange.
enum Ask {
    Fetch,
    Post { message_id: String, text: String },
    Admission {
        role: crate::config::EnrollmentRole,
    },
}

/// An admission request this node made to another network. `grant` is what an
/// approval brought; it stays in memory, never goes to the browser and is not
/// written to the database - the next poll brings it again.
#[derive(Clone)]
struct CandidateRequest {
    key: String,
    name: String,
    role: crate::config::EnrollmentRole,
    status: network_lobby::AdmissionStatus,
    grant: Option<network_lobby::AdmissionGrant>,
    /// Whether this node asked for the admission it now sees. `false` is a
    /// machine that was admitted before, left, and finds the network still
    /// holding its approval: nothing is waiting for an answer, so there is
    /// no notification, and the lobby offers to switch at once.
    asked: bool,
    /// When rejected: the Unix second from which this machine may ask again,
    /// on this node's own clock (now plus what the principal said remains).
    retry_at: Option<i64>,
}

impl CandidateRequest {
    /// An approval this node did not just ask for: credentials it already had.
    fn standing(&self) -> bool {
        self.status == network_lobby::AdmissionStatus::Approved && !self.asked
    }

    fn public_json(&self) -> serde_json::Value {
        json!({
            "key": self.key,
            "name": self.name,
            "role": self.role,
            "status": self.status,
            "asked": self.asked,
            "standing": self.standing(),
            "retry_at": self.retry_at,
        })
    }
}

#[derive(Default)]
struct CandidateState {
    requests: HashMap<String, CandidateRequest>,
    /// Round trip of the last exchange with each network, in milliseconds.
    rtt_ms: HashMap<String, u32>,
    /// Names typed or listed for each network, to label a request.
    loaded: bool,
}

fn candidate_state() -> &'static std::sync::Mutex<CandidateState> {
    static STATE: OnceLock<std::sync::Mutex<CandidateState>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

/// Loads the persisted requests once, and writes them back after a change.
fn sync_requests(database: &crate::db::PomDb, changed: bool) {
    let mut state = candidate_state().lock().expect("candidate state lock");
    if !state.loaded {
        state.loaded = true;
        for value in database.lobby_requests().unwrap_or_default() {
            let (Some(key), Ok(role), Ok(status)) = (
                value["key"].as_str(),
                serde_json::from_value(value["role"].clone()),
                serde_json::from_value(value["status"].clone()),
            ) else {
                continue;
            };
            state.requests.entry(key.to_string()).or_insert(CandidateRequest {
                key: key.to_string(),
                name: value["name"].as_str().unwrap_or_default().to_string(),
                role,
                status,
                grant: None,
                // A request kept before this field existed was asked for.
                asked: value["asked"].as_bool().unwrap_or(true),
                retry_at: value["retry_at"].as_i64(),
            });
        }
    }
    if changed {
        let listed: Vec<serde_json::Value> = state
            .requests
            .values()
            .map(CandidateRequest::public_json)
            .collect();
        if let Err(error) = database.save_lobby_requests(&listed) {
            warn!("lobby: could not save the admission requests: {error:#}");
        }
    }
}

/// Runs one exchange with the lobby of the network under `key`. `database` is
/// absent in setup mode, where nothing about admission is kept.
async fn exchange(
    key: &str,
    ask: Ask,
    name: Option<&str>,
    database: Option<&crate::db::PomDb>,
) -> Response {
    let key = key.trim();
    let client = match network_lobby::client().await {
        Ok(client) => client,
        Err(error) => {
            warn!("lobby: could not start the client: {error:#}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "lobby_unavailable" })),
            )
                .into_response();
        }
    };
    if let Some(database) = database {
        sync_requests(database, false);
    }
    let mut card = local_card().await;
    card.rtt_ms = candidate_state()
        .lock()
        .expect("candidate state lock")
        .rtt_ms
        .get(key)
        .copied();
    let identity = client.proof();
    let asking = matches!(ask, Ask::Admission { .. });
    let request = match ask {
        Ask::Post { message_id, text } => LobbyRequest::Post {
            identity,
            card: card.clone(),
            message_id,
            text,
        },
        Ask::Fetch => LobbyRequest::Fetch {
            identity,
            card: card.clone(),
        },
        Ask::Admission { role } => LobbyRequest::Admission {
            identity,
            card: card.clone(),
            role,
        },
    };
    let started = Instant::now();
    match client.request(key, request).await {
        // A refusal by the principal (`accepted: false`) is an answer, not a
        // transport failure: the screen reads `error` and says why.
        Ok(response) => {
            let elapsed = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
            let admission = {
                let mut state = candidate_state().lock().expect("candidate state lock");
                state.rtt_ms.insert(key.to_string(), elapsed);
                if response.accepted {
                    match &response.admission {
                        Some(view) => {
                            let entry = state.requests.entry(key.to_string()).or_insert_with(|| {
                                CandidateRequest {
                                    key: key.to_string(),
                                    name: String::new(),
                                    role: view.role,
                                    status: view.status,
                                    grant: None,
                                    asked: asking,
                                    retry_at: None,
                                }
                            });
                            if asking {
                                entry.asked = true;
                            }
                            if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
                                entry.name = name.trim().to_string();
                            }
                            entry.role = view.role;
                            entry.status = view.status;
                            entry.grant = view.grant.clone();
                            entry.retry_at = view
                                .retry_after_secs
                                .filter(|secs| *secs > 0)
                                .map(|secs| chrono::Utc::now().timestamp() + secs as i64);
                        }
                        // The principal no longer has the request.
                        None => {
                            state.requests.remove(key);
                        }
                    }
                }
                state.requests.get(key).map(CandidateRequest::public_json)
            };
            if let Some(database) = database {
                sync_requests(database, response.accepted);
            }
            card.rtt_ms = Some(elapsed);
            Json(json!({
                "accepted": response.accepted,
                "error": response.error,
                "node_id": if response.node_id.is_empty() {
                    client.node_id().to_string()
                } else {
                    response.node_id
                },
                "card": card,
                "messages": response.messages,
                "messages_closed": response.messages_closed,
                // Status and role only: the grant never reaches the browser.
                "admission": admission,
            }))
            .into_response()
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /api/ui/network-chat/lobby?key=pk:...` - recent lobby history of the
/// network published under `key`, as its principal answers it now, plus the
/// state of this node's admission request there.
pub(super) async fn lobby_fetch(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(query): Query<LobbyQuery>,
) -> Response {
    exchange(&query.key, Ask::Fetch, None, Some(&state.database)).await
}

/// `POST /api/ui/network-chat/lobby` - say something in that lobby.
pub(super) async fn lobby_post(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(post): Json<LobbyPost>,
) -> Response {
    exchange(
        &post.key,
        Ask::Post {
            message_id: post.message_id,
            text: post.text,
        },
        None,
        Some(&state.database),
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct AdmissionAsk {
    pub key: String,
    pub role: crate::config::EnrollmentRole,
    /// The network's name as the screen lists it, to label the request.
    #[serde(default)]
    pub name: String,
}

/// `POST /api/ui/network-chat/lobby/admission` - ask the network under `key`
/// to admit this machine as `role`. The answer is the request's state; an
/// admin there decides later.
pub(super) async fn lobby_admission(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(ask): Json<AdmissionAsk>,
) -> Response {
    exchange(
        &ask.key,
        Ask::Admission { role: ask.role },
        Some(&ask.name),
        Some(&state.database),
    )
    .await
}

/// `GET /api/ui/network-chat/lobby/requests` - the admission requests this
/// node made, with their last known state. Never the grant.
pub(super) async fn lobby_requests(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    sync_requests(&state.database, false);
    let mut listed: Vec<serde_json::Value> = candidate_state()
        .lock()
        .expect("candidate state lock")
        .requests
        .values()
        .map(CandidateRequest::public_json)
        .collect();
    listed.sort_by(|left, right| left["key"].as_str().cmp(&right["key"].as_str()));
    Json(listed).into_response()
}

/// `POST /api/ui/network-chat/lobby/requests/forget` - drop a request from
/// this node's list. It does not withdraw it on the other network.
pub(super) async fn lobby_request_forget(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(query): Json<LobbyQuery>,
) -> Response {
    sync_requests(&state.database, false);
    candidate_state()
        .lock()
        .expect("candidate state lock")
        .requests
        .remove(query.key.trim());
    sync_requests(&state.database, true);
    Json(json!({ "forgotten": true })).into_response()
}

/// Keeps this node's pending admission requests current while no chat window
/// is open: an approval may come hours after the request. One `Fetch` per
/// pending request every 30 s, which also keeps the machine listed there.
pub(crate) fn spawn_admission_poller(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            sync_requests(&state.database, false);
            let pending: Vec<String> = candidate_state()
                .lock()
                .expect("candidate state lock")
                .requests
                .values()
                .filter(|request| {
                    request.status == network_lobby::AdmissionStatus::Pending
                        || (request.status == network_lobby::AdmissionStatus::Approved
                            && request.grant.is_none())
                })
                .map(|request| request.key.clone())
                .collect();
            for key in pending {
                let _ = exchange(&key, Ask::Fetch, None, Some(&state.database)).await;
            }
        }
    });
}

/// `POST /api/ui/network/join` - join the network under `key`, whose admin
/// approved this machine. It replaces the network this node is in: the local
/// trust root is revoked, the granted one installed, the config rewritten as a
/// member with the granted connection key, and the node restarts. The
/// operator confirms this in the interface; nothing calls it unattended.
pub(super) async fn join_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(query): Json<LobbyQuery>,
) -> Response {
    let key = query.key.trim().to_string();
    let refuse = |status: StatusCode, reason: &str, error: String| {
        (status, Json(json!({ "reason": reason, "error": error }))).into_response()
    };
    // The grant is only kept in memory; after a restart, ask for it again.
    let mut request = candidate_state()
        .lock()
        .expect("candidate state lock")
        .requests
        .get(&key)
        .cloned();
    if request.as_ref().is_some_and(|request| request.grant.is_none()) || request.is_none() {
        let _ = exchange(&key, Ask::Fetch, None, Some(&state.database)).await;
        request = candidate_state()
            .lock()
            .expect("candidate state lock")
            .requests
            .get(&key)
            .cloned();
    }
    let Some(request) = request else {
        return refuse(
            StatusCode::CONFLICT,
            "no_request",
            "this node has no admission request for that network".into(),
        );
    };
    let Some(grant) = request.grant.clone() else {
        return refuse(
            StatusCode::CONFLICT,
            "not_approved",
            "the network has not approved this machine".into(),
        );
    };
    let document = match crate::admin_trust::parse_transported(&grant.trust_document) {
        Ok(document) => document,
        Err(error) => {
            return refuse(
                StatusCode::BAD_GATEWAY,
                "grant_invalid",
                format!("the granted trust document is invalid: {error:#}"),
            )
        }
    };
    // The grant must be for the network that was asked: the one whose
    // principal key is `key`.
    let granted_key =
        crate::pkarr_node::PkarrNode::public_key_uri_from_hex(&document.primary.pubkey);
    if granted_key.as_deref() != Some(key.as_str()) {
        return refuse(
            StatusCode::BAD_GATEWAY,
            "grant_invalid",
            "the granted trust document belongs to another network".into(),
        );
    }
    let metadata = document.setup.clone();
    // The settings in the grant are the network's current ones; the copy in
    // the trust document dates from when the network was created.
    let Some(mut network) = grant
        .network
        .clone()
        .or_else(|| metadata.as_ref().map(super::setup::network_from_trust_metadata))
    else {
        return refuse(
            StatusCode::BAD_GATEWAY,
            "grant_invalid",
            "the grant carries no network settings".into(),
        );
    };
    // A member of a network is never off the network, whatever was granted.
    if network.visibility == crate::config::NetworkVisibility::Local {
        network.visibility = crate::config::NetworkVisibility::Private;
    }
    if grant.connection_key.trim().is_empty() {
        return refuse(
            StatusCode::BAD_GATEWAY,
            "grant_invalid",
            "the grant carries no connection key".into(),
        );
    }

    // The node's own network is never dropped: it goes to standby and comes
    // back, as it was, when the node leaves the network it is joining now.
    {
        let current = state.auth.read().await.clone();
        if let Err(error) = super::standby::save_primary(
            &current,
            &state.admin_trust.document(),
            &state.authority_principal.pubkey,
        ) {
            return refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "standby_not_saved",
                format!("could not keep this machine's own network aside: {error:#}"),
            );
        }
    }
    state.runtime.stop().await;
    state.network.detach().await;
    let trust_path = crate::config::admin_trust_path();
    if let Err(error) = std::fs::remove_file(&trust_path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "trust_not_replaced",
                format!("could not remove the current trust root: {error}"),
            );
        }
    }
    if let Err(error) = crate::admin_trust::AdminTrust::install_document(trust_path, document) {
        return refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "trust_not_replaced",
            format!("could not install the granted trust root: {error:#}"),
        );
    }
    let mut config = match crate::config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            return refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "config_unreadable",
                format!("{error:#}"),
            )
        }
    };
    // How this machine's own interface is reached is the operator's choice
    // and has nothing to do with which network it is in.
    let exposure = config.admin_exposure;
    let access_token = config.admin_access_token.clone();
    config.prepare_for_setup();
    config.admin_exposure = exposure;
    config.admin_access_token = access_token;
    config.setup_pending = false;
    config.profile = match request.role {
        crate::config::EnrollmentRole::Node => crate::profile::NodeProfile::Node,
        crate::config::EnrollmentRole::Infer => crate::profile::NodeProfile::Infer,
    };
    config.connection_key = grant.connection_key.trim().to_string();
    config.network = network;
    let relay_hints = metadata.map(|metadata| metadata.relay_hints).unwrap_or_default();
    for relay in relay_hints.iter().chain(grant.relay_peers.iter()) {
        if !config.relay_peers.contains(relay) {
            config.relay_peers.push(relay.clone());
        }
    }
    if let Err(error) = config.save() {
        return refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "config_not_saved",
            format!("{error:#}"),
        );
    }
    if let Err(error) = state.database.save_network_settings(&config.network) {
        warn!("could not save the network details in the database: {error:#}");
    }
    if let Err(error) = state.database.clear_api_keys() {
        warn!("could not clear the local API keys: {error:#}");
    }
    candidate_state()
        .lock()
        .expect("candidate state lock")
        .requests
        .remove(&key);
    sync_requests(&state.database, true);
    info!(network = %config.network.public_name, "admitted: joining the network and restarting");
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        let error = crate::respawn::respawn_self();
        tracing::error!("respawn after joining the network failed, the node stays up: {error}");
    });
    Json(json!({ "joined": true, "restart": true })).into_response()
}

/// `GET /api/ui/network/admissions` - the admission requests this network
/// received, for its admin to decide. Each carries the candidate's declared
/// hardware, the latency it measured and whether it reaches the principal
/// directly or through a relay.
pub(super) async fn admissions(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    Json(state.network_chat.admissions()).into_response()
}

#[derive(Debug, Deserialize)]
pub struct AdmissionDecision {
    pub node_id: String,
    /// `approve`, `reject` or `forget`.
    pub decision: String,
}

/// `POST /api/ui/network/admissions` - an admin's decision on one request.
/// Approving does not send anything: the candidate collects the grant on its
/// next poll.
pub(super) async fn decide_admission(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(decision): Json<AdmissionDecision>,
) -> Response {
    let status = match decision.decision.as_str() {
        "approve" => Some(network_lobby::AdmissionStatus::Approved),
        "reject" => Some(network_lobby::AdmissionStatus::Rejected),
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
    // Forgetting a machine that was approved takes its membership away, and
    // it still holds the connection key: same treatment as a departure.
    let revoked = status.is_none()
        && state
            .network_chat
            .admission_of(node_id)
            .is_some_and(|record| record.status == network_lobby::AdmissionStatus::Approved);
    if revoked {
        state.network_chat.member_departed(node_id);
        state.network_chat.request_rotation(node_id);
    } else if !state.network_chat.decide_admission(node_id, status) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(state.network_chat.admissions()).into_response()
}

/// `GET /api/setup/lobby?key=pk:...` - the same, from the setup wizard.
pub async fn setup_lobby_fetch(Query(query): Query<LobbyQuery>) -> Response {
    exchange(&query.key, Ask::Fetch, None, None).await
}

/// `POST /api/setup/lobby`.
pub async fn setup_lobby_post(Json(post): Json<LobbyPost>) -> Response {
    exchange(
        &post.key,
        Ask::Post {
            message_id: post.message_id,
            text: post.text,
        },
        None,
        None,
    )
    .await
}

async fn info_exchange(key: &str) -> Response {
    let client = match network_lobby::client().await {
        Ok(client) => client,
        Err(error) => {
            warn!("lobby: could not start the client: {error:#}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "lobby_unavailable" })),
            )
                .into_response();
        }
    };
    match client.request(key.trim(), LobbyRequest::Info).await {
        Ok(response) => Json(json!({
            "accepted": response.accepted,
            "error": response.error,
            "info": response.info,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /api/ui/network-chat/lobby/info?key=pk:...` - models, node count and
/// VRAM the network published under `key` chose to show, asked from its
/// principal now. `info: null` with `accepted: true` means it is still being
/// computed there.
pub(super) async fn lobby_info(_auth: ConnectionAuth, Query(query): Query<LobbyQuery>) -> Response {
    info_exchange(&query.key).await
}

/// How long a measured latency is answered again without asking the network.
const LATENCY_TTL: Duration = Duration::from_secs(60);
/// How many measured networks are remembered.
const LATENCY_CACHE_MAX: usize = 1024;

/// `GET /api/ui/network-chat/lobby/latency?key=pk:...` - the round trip from
/// this node to the node that publishes the network under `key`, over the
/// same keyed connection the lobby uses (direct or through a relay).
///
/// The question asked is `Describe`, which every node answers and which
/// leaves no presence in the other network's lobby. It is asked twice: the
/// first exchange pays for resolving the key and opening the connection, so
/// the smaller of the two is the round trip. Any answer counts, a refusal
/// included; only silence is `502`.
pub(super) async fn lobby_latency(_auth: ConnectionAuth, Query(query): Query<LobbyQuery>) -> Response {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<String, (Instant, u32)>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = query.key.trim().to_string();
    if let Some((at, rtt_ms)) = cache.lock().expect("latency cache lock").get(&key).copied() {
        if at.elapsed() < LATENCY_TTL {
            return Json(json!({ "rtt_ms": rtt_ms })).into_response();
        }
    }
    let client = match network_lobby::client().await {
        Ok(client) => client,
        Err(error) => {
            warn!("lobby: could not start the client: {error:#}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "lobby_unavailable" })),
            )
                .into_response();
        }
    };
    let mut best: Option<u32> = None;
    let mut failure = String::new();
    for _ in 0..2 {
        let started = Instant::now();
        match client.request(&key, LobbyRequest::Describe).await {
            Ok(_) => {
                let elapsed = started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
                best = Some(best.map_or(elapsed, |known| known.min(elapsed)));
            }
            Err(error) => {
                failure = error.to_string();
                break;
            }
        }
    }
    let Some(rtt_ms) = best else {
        return (StatusCode::BAD_GATEWAY, Json(json!({ "error": failure }))).into_response();
    };
    let mut cache = cache.lock().expect("latency cache lock");
    if cache.len() >= LATENCY_CACHE_MAX {
        cache.retain(|_, (at, _)| at.elapsed() < LATENCY_TTL);
    }
    cache.insert(key, (Instant::now(), rtt_ms));
    Json(json!({ "rtt_ms": rtt_ms })).into_response()
}

/// `GET /api/setup/lobby/info?key=pk:...` - the same, from the setup wizard.
pub async fn setup_lobby_info(Query(query): Query<LobbyQuery>) -> Response {
    info_exchange(&query.key).await
}

/// The three facts of a cluster view that a network publishes, computed the
/// way the Public networks screen computes them for the local network
/// (`apps/frontend/src/lib/networkStats.ts`), so both rows agree.
pub(crate) fn network_info_from_cluster(cluster: &serde_json::Value) -> network_lobby::NetworkInfo {
    let peers = cluster["peers"].as_array().cloned().unwrap_or_default();
    let mut models = std::collections::BTreeSet::new();
    let mut add = |value: &serde_json::Value| {
        if let Some(model) = value.as_str().filter(|model| !model.is_empty()) {
            models.insert(model.to_string());
        }
    };
    for entry in cluster["model_inventory"].as_array().into_iter().flatten() {
        add(&entry["model_id"]);
    }
    for peer in &peers {
        add(&peer["model"]);
    }
    add(&cluster["active_model"]);
    let mut vram_total_mb = None;
    for peer in &peers {
        for gpu in peer["node"]["system"]["gpus"].as_array().into_iter().flatten() {
            if let Some(total) = gpu["vram_total_mb"].as_f64().filter(|mb| *mb > 0.0) {
                *vram_total_mb.get_or_insert(0_u64) += total as u64;
            }
        }
    }
    network_lobby::NetworkInfo {
        models: models.into_iter().collect(),
        node_count: peers.len() as u32,
        vram_total_mb,
    }
}

/// Fills `PublicStats` on demand. The swarm loop cannot compute the stats
/// (they need the cluster view, which reads every peer over HTTP), so it wakes
/// this task when a request finds them stale; at most one cluster read per
/// [`network_lobby::INFO_REFRESH`], and none while nobody asks.
pub(crate) fn spawn_public_stats_publisher(state: GatewayState) {
    tokio::spawn(async move {
        loop {
            state.network_chat.public_stats().wanted().await;
            if !state.network_chat.public_stats_enabled().await {
                state.network_chat.public_stats().clear();
                continue;
            }
            let response = cluster(HeaderMap::new(), State(state.clone())).await;
            let view = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
                Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).ok(),
                Err(_) => None,
            };
            if let Some(view) = view.filter(|view| view["peers"].is_array()) {
                state
                    .network_chat
                    .public_stats()
                    .publish(network_info_from_cluster(&view));
            }
            tokio::time::sleep(network_lobby::INFO_REFRESH).await;
        }
    });
}

#[derive(Debug, Default, Deserialize)]
pub struct SetupPublicNetworksQuery {
    /// Directory ingresses typed in the wizard, comma separated. They add to
    /// the ones already configured for this installation.
    #[serde(default)]
    pub replicas: String,
}

/// `GET /api/setup/public-networks` - the public networks a machine in setup
/// mode can learn about. It has no cluster and no pkarr discovery, so it asks
/// the addresses it was given, each in both ways: as a directory replica
/// (signed records, merged with the local cache) and as a node (the networks
/// that node lists, accepted only when they certify themselves). With no
/// address there is nobody to ask and the list is empty.
pub async fn setup_public_networks(
    State(state): State<SetupState>,
    Query(query): Query<SetupPublicNetworksQuery>,
) -> Response {
    let mut replicas = state
        .database
        .public_directory_settings()
        .ok()
        .flatten()
        .unwrap_or_default()
        .replicas;
    // The same field takes relay keys (`pk:...`) and addresses. Keys are the
    // normal case: a relay is found by its key, like every other node.
    let mut relay_keys = network_lobby::default_directory_relays();
    relay_keys.extend(state.config.read().await.relay_peers.iter().cloned());
    for entry in query
        .replicas
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if entry.starts_with("pk:") {
            relay_keys.push(entry.to_string());
        } else {
            replicas.push(entry.to_string());
        }
    }
    relay_keys.sort();
    relay_keys.dedup();
    let replicas = match crate::public_directory::PublicDirectorySettings::from_urls(replicas.iter())
        .and_then(|settings| settings.effective_with_env())
    {
        Ok(settings) => settings.replicas,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    };
    // A node with cluster TLS presents a certificate derived from its own
    // key, which no public authority signs. Accepting it here is safe for this
    // read only: every entry is checked against its own trust document.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_default();
    let mut networks =
        crate::gateway::directory_public_networks(&state.database, &client, replicas.clone())
            .await;
    for network in crate::gateway::listed_public_networks(&client, replicas).await {
        if !networks
            .iter()
            .any(|known| known.fingerprint == network.fingerprint)
        {
            networks.push(network);
        }
    }
    let from_relays =
        futures::future::join_all(relay_keys.iter().map(|key| crate::gateway::relay_directory(key))).await;
    for network in from_relays.into_iter().flatten() {
        if !networks
            .iter()
            .any(|known| known.fingerprint == network.fingerprint)
        {
            networks.push(network);
        }
    }
    crate::gateway::resolve_places(&mut networks);
    crate::webui::remember_public_networks(&state.database, &networks);
    Json(networks).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_approval_this_node_did_not_ask_for_is_standing_not_news() {
        let request = |status, asked| CandidateRequest {
            key: "pk:x".into(),
            name: "Rede".into(),
            role: crate::config::EnrollmentRole::Node,
            status,
            grant: None,
            asked,
            retry_at: None,
        };
        use network_lobby::AdmissionStatus::{Approved, Pending, Rejected};
        // Admitted before, left, came back: the approval is still there.
        assert_eq!(request(Approved, false).public_json()["standing"], true);
        // Asked and then approved: that is an answer, and it is announced.
        assert_eq!(request(Approved, true).public_json()["standing"], false);
        assert_eq!(request(Pending, true).public_json()["standing"], false);
        assert_eq!(request(Rejected, false).public_json()["standing"], false);
    }

    #[test]
    fn published_stats_match_what_the_local_row_shows() {
        let cluster = json!({
            "active_model": "qwen3",
            "model_inventory": [{ "model_id": "qwen3" }, { "model_id": "gemma" }],
            "peers": [
                { "model": "qwen3", "node": { "system": { "gpus": [
                    { "vram_total_mb": 16000.0 }, { "vram_total_mb": 8000.5 }
                ] } } },
                // Unified memory reports no dedicated VRAM, and is not a zero.
                { "model": "", "node": { "system": { "gpus": [{ "vram_total_mb": null }] } } },
                // A peer that could not be read still counts as a node.
                { "model": "llama", "node": null },
            ],
        });
        let info = network_info_from_cluster(&cluster);
        assert_eq!(info.models, vec!["gemma", "llama", "qwen3"]);
        assert_eq!(info.node_count, 3);
        assert_eq!(info.vram_total_mb, Some(24_000));

        let empty = network_info_from_cluster(&json!({ "peers": [] }));
        assert_eq!(empty.node_count, 0);
        assert_eq!(empty.vram_total_mb, None);
        assert!(empty.models.is_empty());
    }
}
