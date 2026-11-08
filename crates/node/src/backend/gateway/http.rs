use super::*;

// ── Server startup ────────────────────────────────────────────────────────────

/// Carries the port the gateway bound, which the native window loads.
pub(crate) type GatewayReadySender = std::sync::mpsc::SyncSender<u16>;

pub(crate) async fn start_gateway_with_ready(
    state: GatewayState,
    port: u16,
    ready: Option<GatewayReadySender>,
) -> Result<()> {
    crate::config::validate_gateway_port(port)?;
    let warm = state.clone();
    let tls_server = state.tls_server.clone();
    let quic_server = state.quic_server.clone();
    let quic_port = state.quic_port;
    crate::user_session::announce_operator_path();
    let _ = crate::user_session::demo_mode();
    let admin = admin::admin_routes().with_state(state.clone());
    crate::gpu_arbiter::install(state.clone());
    let app = Router::new()
        .route("/api/generate", post(handle_generate))
        .route("/generate", post(handle_generate)) // alias
        .route("/api/access", get(admin_access_status))
        .route(
            "/auth/*rest",
            axum::routing::any(crate::user_session::auth_http),
        )
        .route("/api/public/network", get(public_network))
        .route("/api/public/networks", get(public_networks))
        .route("/api/public/networks/locate", get(locate_public_network))
        .route(
            "/api/public/directory/records",
            get(public_directory_records)
                .post(accept_public_directory_record)
                .layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/peers", get(handle_peers))
        .route("/health", get(handle_health))
        .route(crate::quic_policy::QUIC_POLICY_PATH, get(cluster_quic))
        .route(
            crate::cluster_logs_policy::CLUSTER_LOGS_POLICY_PATH,
            get(cluster_logs_policy),
        )
        .route(
            crate::network_chat_policy::NETWORK_CHAT_POLICY_PATH,
            get(network_chat_policy),
        )
        .route(
            crate::webui::NETWORK_SETTINGS_POLICY_PATH,
            get(crate::webui::network_settings_policy),
        )
        .route(
            crate::webui::MEMBER_LEFT_PATH,
            post(crate::webui::member_left),
        )
        .route("/handshake", post(session::handle_handshake))
        // Where the tail of a pipeline delivers the result. The route is internal
        // to the pipeline: the Ticket is the per-frame credential.
        .route(
            crate::direct::RELAY_RESULT_PATH,
            post(handle_relay_result).layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)),
        )
        .route(
            crate::direct::REMOTE_STREAM_RESULT_PATH,
            post(handle_remote_stream_result)
                .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)),
        )
        .route(
            crate::link::LINK_PATH,
            post(handle_remote_stream_link).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .merge(crate::openai::routes())
        .merge(crate::chat_tools::routes())
        .merge(crate::messages::routes())
        .merge(crate::webui::routes())
        .merge(crate::transfer::routes())
        .merge(admin)
        // Genuinely last: this mounts the SPA fallback after all concrete API
        // routes, so an unknown mutating path cannot turn a real handler into
        // a method-not-allowed response.
        .merge(crate::webapp::routes())
        .with_state(state.clone())
        // JSON responses and the static UI bundle compress well; the default
        // predicate already skips `text/event-stream`, so the SSE token
        // stream in openai::routes() is untouched — compressing it would
        // trade first-token latency for bytes on exactly the path that
        // cares most about the former. Peer-to-peer tensor frames never pass
        // through this router (see inference/infer_server.rs), so there is
        // no risk of compressing binary inference frames here.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_admin_for_surface,
        ))
        .layer(middleware::from_fn_with_state(
            state,
            verify_control_request,
        ))
        // A streamed NDJSON answer (the Chat's image generation) is left
        // uncompressed like SSE: compressed, its last line never reached the
        // browser, which then failed the whole stream with "network error".
        .layer(compression())
        // Every response says which node it came from; see `peer_client::answered_by_other_node`.
        .layer(middleware::map_response(crate::peer_client::stamp_node_id));

    // Both startup paths (with a model and gateway-only) come through here, so
    // this is the one place that warms every node's catalog exactly once.
    crate::webui::warm_catalog(warm);

    let listener = crate::infer_server::bind_dual_stack(port).await?;
    if let Some(ready) = ready {
        let _ = ready.send(port);
    }
    info!(
        "🌐 Interface e API em http://localhost:{port}  \
         — UI /  ·  OpenAI /v1/chat/completions  ·  Anthropic /v1/messages  ·  \
         POST /api/generate  ·  GET /peers /health"
    );
    // Nagle delays a small write until the ACK of the previous one. In SSE each
    // token is a small write, and the interaction with the other side's
    // delayed-ACK adds tens of ms to what the client feels. `reqwest` already
    // turns this off on the client side by default; `axum::serve` left
    // `tcp_nodelay` at `None`, so the server side kept Nagle on in every
    // response. `tcp_nodelay` moved, it did not go away: `serve` applies it to
    // each accepted connection, along with the decision between TLS and clear text.
    crate::serve::serve_with_quic(
        listener,
        app,
        tls_server,
        quic_server,
        quic_port,
        std::future::pending::<()>(),
    )
    .await?;
    Ok(())
}

pub(super) async fn cluster_logs_policy(
    _auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "logs_enabled": state.network.cluster_logs_enabled(),
    }))
}

pub(super) async fn network_chat_policy(
    _auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "network_chat_enabled": state.network.network_chat_enabled(),
    }))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PublicNetworkView {
    pub cluster_id: String,
    pub name: String,
    pub fingerprint: String,
    /// `pk:` key of the principal publishing the network. The network is found
    /// by it, not by an IP: see `locate_public_network`.
    #[serde(default)]
    pub publisher_key: String,
    /// `pk:` keys of the network's dedicated relays.
    #[serde(default)]
    pub relay_hints: Vec<String>,
    /// The signed chain is public network metadata, not a shared secret. It
    /// lets the public directory hand the join flow everything it needs.
    pub trust_document: String,
    pub organization: String,
    pub description: String,
    pub rules: String,
    pub channels: String,
    pub country_code: String,
    pub country_name: String,
    /// State or region of the city. Only filled by `resolve_places`.
    #[serde(default)]
    pub state: String,
    pub city: String,
    pub geo_label: String,
    pub access_policy: crate::config::NetworkAccessPolicy,
    /// Enrollment roles the network advertises as open; empty means it is not
    /// accepting new members and the directory disables its join button.
    #[serde(default)]
    pub accepted_enrollments: crate::config::AcceptedEnrollments,
    /// The network publishes models, node count and VRAM to non-members,
    /// asked live from its principal.
    #[serde(default)]
    pub public_stats: bool,
    /// The network's Chat is on, so its lobby carries a conversation with
    /// non-members; off, the lobby only takes admission requests.
    #[serde(default)]
    pub lobby_chat: bool,
    pub location: Option<crate::config::NetworkLocation>,
    pub discovered_via: String,
    #[serde(default)]
    pub directory_replicas: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PublicNetworksQuery {
    /// Search only cached records whose canonical label starts with this
    /// hierarchy, for example `com.br.curitiba`.
    pub geo_prefix: Option<String>,
    /// Search only cached records whose canonical label ends with this node or
    /// network suffix. Both filters are conjunctive when supplied together.
    pub geo_suffix: Option<String>,
}

fn matches_geo_label(label: &str, query: &PublicNetworksQuery) -> bool {
    let label = label.trim();
    query
        .geo_prefix
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none_or(|prefix| label.starts_with(prefix))
        && query
            .geo_suffix
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none_or(|suffix| label.ends_with(suffix))
}

fn allow_public_p2p_discovery(visibility: crate::config::NetworkVisibility) -> bool {
    visibility == crate::config::NetworkVisibility::Public
}

fn directory_settings(state: &GatewayState) -> crate::public_directory::PublicDirectorySettings {
    state
        .database
        .public_directory_settings()
        .ok()
        .flatten()
        .unwrap_or_default()
        .effective_with_env()
        .unwrap_or_default()
}

fn directory_network_from_view(
    view: &PublicNetworkView,
) -> crate::public_directory::PublicDirectoryNetwork {
    crate::public_directory::PublicDirectoryNetwork {
        cluster_id: view.cluster_id.clone(),
        name: view.name.clone(),
        fingerprint: view.fingerprint.clone(),
        trust_document: view.trust_document.clone(),
        organization: view.organization.clone(),
        description: view.description.clone(),
        rules: view.rules.clone(),
        channels: view.channels.clone(),
        country_code: view.country_code.clone(),
        country_name: view.country_name.clone(),
        city: view.city.clone(),
        geo_label: view.geo_label.clone(),
        access_policy: view.access_policy,
        accepted_enrollments: view.accepted_enrollments.clone(),
        public_stats: view.public_stats,
        lobby_chat: view.lobby_chat,
        location: view.location.clone(),
        relay_hints: view.relay_hints.clone(),
        directory_replicas: view.directory_replicas.clone(),
    }
}

fn view_from_directory_record(
    record: crate::public_directory::SignedPublicDirectoryRecord,
) -> PublicNetworkView {
    let publisher_key =
        crate::pkarr_node::PkarrNode::public_key_uri_from_hex(&record.publisher_pubkey)
            .unwrap_or_default();
    let network = record.network;
    PublicNetworkView {
        cluster_id: network.cluster_id,
        name: network.name,
        fingerprint: network.fingerprint,
        publisher_key,
        relay_hints: network.relay_hints,
        trust_document: network.trust_document,
        organization: network.organization,
        description: network.description,
        rules: network.rules,
        channels: network.channels,
        country_code: network.country_code,
        country_name: network.country_name,
        state: String::new(),
        city: network.city,
        geo_label: network.geo_label,
        access_policy: network.access_policy,
        accepted_enrollments: network.accepted_enrollments,
        public_stats: network.public_stats,
        lobby_chat: network.lobby_chat,
        location: network.location,
        discovered_via: "directory".to_string(),
        directory_replicas: network.directory_replicas,
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PublicDirectoryRecordsResponse {
    version: u32,
    records: Vec<crate::public_directory::SignedPublicDirectoryRecord>,
}

fn directory_records_url(base: &str) -> String {
    format!(
        "{}/api/public/directory/records",
        base.trim_end_matches('/')
    )
}

fn validate_public_directory_semantics(
    record: &crate::public_directory::SignedPublicDirectoryRecord,
) -> Result<()> {
    let document = crate::admin_trust::parse_transported(&record.network.trust_document)
        .map_err(|error| anyhow::anyhow!("invalid public chain: {error:#}"))?;
    if document.anchor.cluster_id != record.network.cluster_id
        || crate::admin_trust::anchor_fingerprint(&document) != record.network.fingerprint
    {
        return Err(anyhow::anyhow!(
            "the record does not match the announced trust root"
        ));
    }
    if document.primary.pubkey != record.publisher_pubkey {
        return Err(anyhow::anyhow!(
            "only the principal of the network can publish its record"
        ));
    }
    Ok(())
}

async fn read_directory_records(
    state: &GatewayState,
) -> Vec<crate::public_directory::SignedPublicDirectoryRecord> {
    let (records, learned) = merge_directory_replicas(
        &state.database,
        &state.peer_http.plain(),
        directory_settings(state).replicas,
    )
    .await;
    replicate_directory_records(state, learned).await;
    records
}

/// The public networks a node can list from the federated directory alone:
/// the local cache merged with what `replicas` answer now. It needs no
/// cluster, which is what lets the setup wizard show the directory.
pub(crate) async fn directory_public_networks(
    database: &crate::db::PomDb,
    client: &reqwest::Client,
    replicas: Vec<String>,
) -> Vec<PublicNetworkView> {
    let (records, _) = merge_directory_replicas(database, client, replicas).await;
    let mut networks: Vec<PublicNetworkView> = Vec::new();
    for record in records {
        let network = view_from_directory_record(record);
        if !networks
            .iter()
            .any(|known| known.fingerprint == network.fingerprint)
        {
            networks.push(network);
        }
    }
    networks
}

/// Entries accepted from one node's listing.
const LISTED_NETWORKS_MAX: usize = 256;

/// Accepts a network another node listed only if it certifies itself: the
/// trust document must be the one the fingerprint and the `cluster_id` name.
/// The publisher key is derived from that document, never taken from the
/// listing, so a node cannot point someone else's network at its own key.
/// The descriptive fields stay as listed; they are not signed on this path.
pub(crate) fn validated_listed_network(mut view: PublicNetworkView) -> Option<PublicNetworkView> {
    let document = crate::admin_trust::parse_transported(&view.trust_document).ok()?;
    if document.anchor.cluster_id != view.cluster_id
        || crate::admin_trust::anchor_fingerprint(&document) != view.fingerprint
    {
        return None;
    }
    view.publisher_key =
        crate::pkarr_node::PkarrNode::public_key_uri_from_hex(&document.primary.pubkey)?;
    view.discovered_via = "node".to_string();
    Some(view)
}

/// Accepts a network published to this relay only from its own principal:
/// on top of certifying itself, the trust document's primary key must be
/// `publisher_pubkey`, the node key that signed for the connection.
pub(crate) fn validated_publication(
    view: PublicNetworkView,
    publisher_pubkey: &str,
) -> Option<PublicNetworkView> {
    let document = crate::admin_trust::parse_transported(&view.trust_document).ok()?;
    if document.primary.pubkey != publisher_pubkey {
        return None;
    }
    let mut view = validated_listed_network(view)?;
    view.discovered_via = "published".to_string();
    Some(view)
}

/// The public networks the nodes at `bases` list right now
/// (`GET <base>/api/public/networks`). A configured node learns networks from
/// its cluster and pkarr; a node in setup mode has neither, so it asks one
/// that does.
pub(crate) async fn listed_public_networks(
    client: &reqwest::Client,
    bases: Vec<String>,
) -> Vec<PublicNetworkView> {
    let answers = futures::future::join_all(bases.into_iter().map(|base| {
        let client = client.clone();
        async move {
            let response = client
                .get(format!(
                    "{}/api/public/networks",
                    base.trim_end_matches('/')
                ))
                .timeout(Duration::from_secs(8))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            response.json::<Vec<PublicNetworkView>>().await.ok()
        }
    }))
    .await;
    answers
        .into_iter()
        .flatten()
        .flat_map(|listed| listed.into_iter().take(LISTED_NETWORKS_MAX))
        .filter_map(validated_listed_network)
        .collect()
}

/// Fetches the replicas, validates and merges into the cached records, and
/// returns the cache plus the records learned in this pass.
async fn merge_directory_replicas(
    database: &crate::db::PomDb,
    client: &reqwest::Client,
    replicas: Vec<String>,
) -> (
    Vec<crate::public_directory::SignedPublicDirectoryRecord>,
    Vec<crate::public_directory::SignedPublicDirectoryRecord>,
) {
    let responses = futures::future::join_all(replicas.into_iter().map(|base| {
        let client = client.clone();
        async move {
            let response = client
                .get(directory_records_url(&base))
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            response
                .json::<PublicDirectoryRecordsResponse>()
                .await
                .ok()
                .filter(|payload| {
                    payload.version == crate::public_directory::PUBLIC_DIRECTORY_VERSION
                })
                .map(|payload| payload.records)
        }
    }))
    .await;

    let now = crate::public_directory::current_time_secs()
        .unwrap_or_else(|_| chrono::Utc::now().timestamp());
    let mut records = database.public_directory_records().unwrap_or_default();
    let mut changed = false;
    let mut learned = Vec::new();
    for incoming in responses.into_iter().flatten().flatten() {
        if let Err(error) = validate_public_directory_semantics(&incoming) {
            warn!("federated directory record refused: {error:#}");
            continue;
        }
        let learned_record = incoming.clone();
        match crate::public_directory::merge_record(&mut records, incoming, now) {
            Ok(crate::public_directory::MergeOutcome::Inserted)
            | Ok(crate::public_directory::MergeOutcome::Replaced) => {
                changed = true;
                learned.push(learned_record);
            }
            Ok(_) => {}
            Err(error) => warn!("federated directory record refused: {error:#}"),
        }
    }
    let before = records.len();
    crate::public_directory::prune_records(&mut records, now);
    records.retain(|record| {
        if record.validate(now).is_err() || validate_public_directory_semantics(record).is_err() {
            changed = true;
            false
        } else {
            true
        }
    });
    changed |= before != records.len();
    if changed {
        if let Err(error) = database.save_public_directory_records(&records) {
            warn!("could not save the federated directory cache: {error:#}");
        }
    }
    (records, learned)
}

async fn replicate_directory_records(
    state: &GatewayState,
    records: Vec<crate::public_directory::SignedPublicDirectoryRecord>,
) {
    if records.is_empty() {
        return;
    }
    let settings = directory_settings(state);
    let client = state.peer_http.plain();
    let sends = futures::future::join_all(settings.replicas.into_iter().flat_map(|base| {
        let client = client.clone();
        records.iter().cloned().map(move |record| {
            let client = client.clone();
            let url = directory_records_url(&base);
            async move {
                client
                    .post(url)
                    .json(&record)
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
            }
        })
    }))
    .await;
    let failures = sends.iter().filter(|result| result.is_err()).count();
    if failures > 0 {
        warn!(
            "{} anti-entropy publication(s) of the federated directory failed",
            failures
        );
    }
}

async fn publish_local_directory_record(state: &GatewayState) -> Result<()> {
    let settings = directory_settings(state);
    if settings.replicas.is_empty() {
        return Ok(());
    }
    if !state.network.is_principal() {
        return Ok(());
    }
    let Some(local) = local_public_network(state).await else {
        return Ok(());
    };
    let now = crate::public_directory::current_time_secs()?;
    let mut records = state.database.public_directory_records()?;
    let desired = directory_network_from_view(&local);
    let publisher = hex::encode(state.authority_signing_key.verifying_key().as_bytes());
    if records.iter().any(|record| {
        record.publisher_pubkey == publisher
            && record.network == desired
            && record.expires_at
                > now + crate::public_directory::PUBLIC_DIRECTORY_MAX_RECORD_TTL_SECS / 3
            && record.validate(now).is_ok()
    }) {
        return Ok(());
    }
    let sequence = records
        .iter()
        .filter(|record| record.publisher_pubkey == publisher)
        .map(|record| record.sequence)
        .max()
        .unwrap_or(0)
        .max(crate::public_directory::next_sequence()?.saturating_sub(1))
        .saturating_add(1);
    let record = crate::public_directory::SignedPublicDirectoryRecord::sign(
        &state.authority_signing_key,
        sequence,
        now,
        desired,
    )?;
    let _ = crate::public_directory::merge_record(&mut records, record.clone(), now)?;
    state.database.save_public_directory_records(&records)?;

    let client = state.peer_http.plain();
    let sends = futures::future::join_all(settings.replicas.into_iter().map(|base| {
        let client = client.clone();
        let record = record.clone();
        async move {
            client
                .post(directory_records_url(&base))
                .json(&record)
                .timeout(Duration::from_secs(3))
                .send()
                .await
        }
    }))
    .await;
    for result in sends {
        if let Err(error) = result {
            warn!("could not publish to the federated directory ingress: {error}");
        }
    }
    Ok(())
}

pub(crate) fn spawn_public_directory_republisher(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30 * 60));
        ticker.tick().await;
        loop {
            if let Err(error) = publish_local_directory_record(&state).await {
                warn!("publication to the federated directory failed: {error:#}");
            }
            ticker.tick().await;
        }
    });
}

async fn public_directory_records(State(state): State<GatewayState>) -> impl IntoResponse {
    let now = crate::public_directory::current_time_secs()
        .unwrap_or_else(|_| chrono::Utc::now().timestamp());
    let mut records = state
        .database
        .public_directory_records()
        .unwrap_or_default();
    crate::public_directory::prune_records(&mut records, now);
    records.retain(|record| {
        record.validate(now).is_ok() && validate_public_directory_semantics(record).is_ok()
    });
    if let Err(error) = state.database.save_public_directory_records(&records) {
        warn!("could not prune the federated directory cache: {error:#}");
    }
    Json(PublicDirectoryRecordsResponse {
        version: crate::public_directory::PUBLIC_DIRECTORY_VERSION,
        records,
    })
}

pub(super) async fn accept_public_directory_record(
    State(state): State<GatewayState>,
    Json(record): Json<crate::public_directory::SignedPublicDirectoryRecord>,
) -> Response {
    let now = crate::public_directory::current_time_secs()
        .unwrap_or_else(|_| chrono::Utc::now().timestamp());
    if let Err(error) = validate_public_directory_semantics(&record) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response();
    }
    let mut records = state
        .database
        .public_directory_records()
        .unwrap_or_default();
    match crate::public_directory::merge_record(&mut records, record, now) {
        Ok(outcome) => {
            if let Err(error) = state.database.save_public_directory_records(&records) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("could not persist the record: {error:#}") }))).into_response();
            }
            let status = match outcome {
                crate::public_directory::MergeOutcome::Inserted => StatusCode::CREATED,
                _ => StatusCode::OK,
            };
            (
                status,
                Json(serde_json::json!({ "accepted": true, "outcome": format!("{outcome:?}") })),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "accepted": false, "error": error.to_string() })),
        )
            .into_response(),
    }
}

async fn local_public_network(state: &GatewayState) -> Option<PublicNetworkView> {
    let visibility = state.auth.read().await.network.visibility;
    if visibility != crate::config::NetworkVisibility::Public {
        return None;
    }
    local_network_view(state).await
}

/// The network this node is in, whatever its visibility, in the shape the
/// listings use. Only the operator's own screen may show it: what goes out to
/// other machines is [`local_public_network`].
pub(crate) async fn local_network_view(state: &GatewayState) -> Option<PublicNetworkView> {
    let configured_network = state.auth.read().await.network.clone();
    if !state.admin_trust.is_anchored() {
        return None;
    }
    let network = state
        .database
        .network_settings()
        .ok()
        .flatten()
        .unwrap_or(configured_network);
    let document = state.admin_trust.document();
    let fingerprint = crate::admin_trust::anchor_fingerprint(&document);
    let trust_document = serde_json::to_string_pretty(&document).ok()?;
    let directory_replicas = directory_settings(state).replicas;
    let relay_hints = crate::webui::relay_hint_keys(&state.auth.read().await.relay_peers);
    Some(PublicNetworkView {
        cluster_id: document.anchor.cluster_id.clone(),
        name: network.public_name.clone(),
        fingerprint,
        publisher_key: crate::pkarr_node::PkarrNode::public_key_uri_from_hex(
            &document.primary.pubkey,
        )
        .unwrap_or_default(),
        relay_hints,
        trust_document,
        organization: network.organization.clone(),
        description: network.description.clone(),
        rules: network.rules.clone(),
        channels: network.channels.clone(),
        country_code: network.country_code.clone(),
        country_name: network.country_name.clone(),
        state: network.state.clone(),
        city: network.city.clone(),
        geo_label: network.geo_label.clone(),
        access_policy: network.access_policy,
        accepted_enrollments: network.accepted_enrollments.clone(),
        public_stats: network.public_stats,
        lobby_chat: state.network_chat.enabled(),
        // Already reduced to the precision the admin chose; `accuracy_m` is
        // what tells a viewer the point is approximate.
        location: network.location.clone(),
        discovered_via: "pkarr".to_string(),
        directory_replicas,
    })
}

/// Host of an IP endpoint address (brackets for IPv6), to build the gateway URL at
/// dial time.
/// The host of a direct endpoint address (`ip:port`), ready for a URL.
fn endpoint_addr_host(addr: &str) -> Option<String> {
    let socket = addr.parse::<std::net::SocketAddr>().ok()?;
    Some(match socket.ip() {
        std::net::IpAddr::V4(ip) => ip.to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct LocateQuery {
    pub key: String,
}

/// How a key's network can be reached right now, according to its pkarr
/// record: no address at all in the answer, only the kind of path.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct NetworkLocator {
    /// `direct` (a public address is published), `relayed` (only a circuit
    /// through a relay) or `unresolved` (the key did not answer in the DHT).
    pub reachability: &'static str,
    pub relays: usize,
}

const LOCATE_CACHE_TTL: Duration = Duration::from_secs(30);

fn locate_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, (Instant, NetworkLocator)>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, (Instant, NetworkLocator)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// How long a publisher stays locatable after the networks list showed it.
const LISTED_PUBLISHER_TTL: Duration = Duration::from_secs(30 * 60);
const LISTED_PUBLISHERS_MAX: usize = 1024;

fn listed_publishers() -> &'static std::sync::Mutex<std::collections::HashMap<String, Instant>> {
    static LISTED: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Instant>>,
    > = std::sync::OnceLock::new();
    LISTED.get_or_init(Default::default)
}

/// Remembers the publishers of the networks this node has just listed, so the
/// locate route answers for every row of the screen. A node on its own local
/// network hears no announce: its list comes from the relays and is stored
/// nowhere else.
pub(crate) fn remember_listed_publishers(networks: &[PublicNetworkView]) {
    let now = Instant::now();
    let mut listed = listed_publishers().lock().expect("listed publishers lock");
    listed.retain(|_, seen| now.duration_since(*seen) < LISTED_PUBLISHER_TTL);
    for network in networks {
        let Ok(key) = pkarr::PublicKey::try_from(network.publisher_key.trim()) else {
            continue;
        };
        let key = key.to_uri_string();
        if listed.len() < LISTED_PUBLISHERS_MAX || listed.contains_key(&key) {
            listed.insert(key, now);
        }
    }
}

pub(crate) fn locator_from(record: Option<&crate::pkarr_node::PkarrRecord>) -> NetworkLocator {
    match record {
        None => NetworkLocator {
            reachability: "unresolved",
            relays: 0,
        },
        Some(record) => NetworkLocator {
            reachability: if !record.addrs.is_empty() {
                "direct"
            } else if !record.relays.is_empty() {
                "relayed"
            } else {
                "unresolved"
            },
            relays: record.relays.len(),
        },
    }
}

/// Keys the locate route accepts: only publishers this node already lists. The
/// route is public; without that limit it would become a DHT resolver for any
/// key from anyone.
async fn locatable_keys(state: &GatewayState) -> std::collections::HashSet<String> {
    let mut keys: std::collections::HashSet<String> = state
        .database
        .public_directory_records()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| {
            crate::pkarr_node::PkarrNode::public_key_uri_from_hex(&record.publisher_pubkey)
        })
        .collect();
    if state.admin_trust.is_anchored() {
        keys.extend(crate::pkarr_node::PkarrNode::public_key_uri_from_hex(
            &state.admin_trust.current_primary().pubkey,
        ));
    }
    let peers = state
        .network
        .alive_peers()
        .await
        .into_iter()
        .chain(state.network.public_discovered_peers().await);
    keys.extend(
        peers
            .filter_map(|peer| crate::pkarr_node::PkarrNode::public_key_uri_from_hex(&peer.pubkey)),
    );
    keys.extend(
        listed_publishers()
            .lock()
            .expect("listed publishers lock")
            .iter()
            .filter(|(_, seen)| seen.elapsed() < LISTED_PUBLISHER_TTL)
            .map(|(key, _)| key.clone()),
    );
    keys
}

/// `GET /api/public/networks/locate?key=pk:...` - resolves the publisher's key
/// through pkarr and says whether the network has a direct path, only a relay
/// path, or no record. The node does the resolving: the browser does not speak
/// p2p and must not receive anyone's IP.
pub(super) async fn locate_public_network(
    State(state): State<GatewayState>,
    Query(query): Query<LocateQuery>,
) -> Response {
    let Ok(key) = pkarr::PublicKey::try_from(query.key.trim()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let key = key.to_uri_string();
    if let Some((at, locator)) = locate_cache()
        .lock()
        .expect("locate cache lock")
        .get(&key)
        .cloned()
    {
        if at.elapsed() < LOCATE_CACHE_TTL {
            return Json(locator).into_response();
        }
    }
    if !locatable_keys(&state).await.contains(&key) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(publisher) = state.pkarr_publisher.as_ref() else {
        return Json(locator_from(None)).into_response();
    };
    let resolved = tokio::time::timeout(Duration::from_secs(8), publisher.node().resolve(&key))
        .await
        .ok()
        .flatten();
    let locator = locator_from(resolved.as_ref().map(|peer| &peer.record));
    locate_cache()
        .lock()
        .expect("locate cache lock")
        .insert(key, (Instant::now(), locator.clone()));
    Json(locator).into_response()
}

/// Public metadata for a network. The trust document is signed public data,
/// not a password, so the directory can pass it to the no-secret join flow.
pub(super) async fn public_network(State(state): State<GatewayState>) -> impl IntoResponse {
    match local_public_network(&state).await {
        Some(network) => (StatusCode::OK, Json(network)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Return local, federated-directory, and live pkarr-backed public networks.
/// The directory is an independent locator: a node can list a signed public
/// record even when the advertised IPv6 address cannot be dialed directly.
pub(super) async fn public_networks(
    State(state): State<GatewayState>,
    Query(query): Query<PublicNetworksQuery>,
) -> impl IntoResponse {
    // What this node sees for itself and what its relays know, asked at the
    // same time. The relays matter most for a node with discovery off, which
    // hears no announce at all. A relay's own board does not go through here
    // (`spawn_directory_board_publisher` calls the collector), so relays never
    // chain questions to each other.
    let relay_keys = directory_relay_keys(&state).await;
    let (mut networks, from_relays) = tokio::join!(
        collect_public_networks(&state, &query),
        futures::future::join_all(relay_keys.iter().map(|key| relay_directory(key))),
    );
    for network in from_relays.into_iter().flatten() {
        if matches_geo_label(&network.geo_label, &query)
            && !networks
                .iter()
                .any(|known| known.fingerprint == network.fingerprint)
        {
            networks.push(network);
        }
    }
    remember_listed_publishers(&networks);
    resolve_places(&mut networks);
    crate::webui::remember_public_networks(&state.database, &networks);
    Json(networks)
}

/// Rewrites the country, state and city of each listed network from the
/// coordinates it publishes and this node's offline city database.
///
/// The names a network sends are text from another machine. What a viewer
/// sees and filters by is what the local database says is at that point, so
/// two nodes listing the same network name the place the same way and a
/// network cannot claim one city while pointing at another. A network that
/// publishes no coordinates is listed without a place.
pub(crate) fn resolve_places(networks: &mut [PublicNetworkView]) {
    for network in networks {
        let place = network
            .location
            .as_ref()
            .and_then(|location| crate::geo::resolve(location.latitude, location.longitude));
        match place {
            Some(city) => {
                network.country_code = city.country_code;
                network.country_name = city.country_name;
                network.state = city.state;
                network.city = city.name;
            }
            None => {
                network.country_code.clear();
                network.country_name.clear();
                network.state.clear();
                network.city.clear();
            }
        }
    }
}

/// The public networks the relay under `key` knows, each checked against its
/// own trust document. A relay that is still filling its board is asked once
/// more; one that does not answer contributes nothing.
pub(crate) async fn relay_directory(key: &str) -> Vec<PublicNetworkView> {
    let Ok(client) = crate::network_lobby::client().await else {
        return Vec::new();
    };
    let mut response = match client
        .request(key, crate::network_lobby::LobbyRequest::Directory)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            debug!("lobby: relay {key} did not answer the directory: {error:#}");
            return Vec::new();
        }
    };
    if response.accepted && response.pending {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Ok(again) = client
            .request(key, crate::network_lobby::LobbyRequest::Directory)
            .await
        {
            response = again;
        }
    }
    response
        .networks
        .into_iter()
        .take(crate::network_lobby::LobbyLimits::DIRECTORY_NETWORKS)
        .filter_map(validated_listed_network)
        .collect()
}

/// The relay keys this node asks for the networks they know: the built-in and
/// environment list plus the relays in its config, never its own key.
async fn directory_relay_keys(state: &GatewayState) -> Vec<String> {
    let own = state
        .pkarr_publisher
        .as_ref()
        .map(|publisher| publisher.node().public_key_uri());
    let mut keys = crate::network_lobby::default_directory_relays();
    keys.extend(state.auth.read().await.relay_peers.iter().cloned());
    keys.retain(|key| Some(key) != own.as_ref());
    keys.sort();
    keys.dedup();
    keys
}

/// Whether this node gathers the announces of other clusters into a list of
/// public networks. A node in a public network does, and so does a relay
/// whatever its own network: every cluster that uses it meets there, and that
/// is what makes it able to answer a machine that has joined nothing.
async fn gathers_public_networks(state: &GatewayState) -> bool {
    allow_public_p2p_discovery(state.auth.read().await.network.visibility)
        || state
            .pkarr_publisher
            .as_ref()
            .is_some_and(|publisher| publisher.desired_record().is_relay)
}

/// Asks a candidate for its network over the keyed lobby connection. The
/// fallback for a candidate whose gateway cannot be reached over HTTP, which
/// is every node behind NAT.
async fn describe_by_key(pubkey_hex: &str) -> Option<PublicNetworkView> {
    let key = crate::pkarr_node::PkarrNode::public_key_uri_from_hex(pubkey_hex)?;
    let client = crate::network_lobby::client().await.ok()?;
    let ask = || client.request(&key, crate::network_lobby::LobbyRequest::Describe);
    let mut response = tokio::time::timeout(Duration::from_secs(6), ask())
        .await
        .ok()?
        .ok()?;
    if response.accepted && response.pending {
        // The other node fills its board on demand: one short second chance.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        response = tokio::time::timeout(Duration::from_secs(6), ask())
            .await
            .ok()?
            .ok()?;
    }
    response
        .networks
        .into_iter()
        .next()
        .and_then(validated_listed_network)
}

async fn collect_public_networks(
    state: &GatewayState,
    query: &PublicNetworksQuery,
) -> Vec<PublicNetworkView> {
    let mut networks = Vec::new();
    let has_geo_filter = query
        .geo_prefix
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || query
            .geo_suffix
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    if let Some(local) = local_public_network(state).await {
        if !has_geo_filter || matches_geo_label(&local.geo_label, query) {
            networks.push(local);
        }
    }

    if let Err(error) = publish_local_directory_record(state).await {
        warn!("could not update the local record of the federated directory: {error:#}");
    }
    let directory_records = read_directory_records(state).await;
    for record in directory_records {
        let network = view_from_directory_record(record);
        if has_geo_filter && !matches_geo_label(&network.geo_label, query) {
            continue;
        }
        if !networks
            .iter()
            .any(|known: &PublicNetworkView| known.fingerprint == network.fingerprint)
        {
            networks.push(network);
        }
    }

    let mut geo_cache = state
        .database
        .public_network_geo_cache()
        .unwrap_or_default();
    let gathers = gathers_public_networks(state).await;
    let mut candidates = if gathers {
        state.network.alive_peers().await
    } else {
        Vec::new()
    };
    // Admitted peers are this node's own cluster: asking one of them by key
    // can only describe the network this node is already in.
    let members: std::collections::HashSet<String> =
        candidates.iter().map(|peer| peer.node_id.clone()).collect();
    if gathers {
        for peer in state.network.public_discovered_peers().await {
            if !candidates.iter().any(|known| known.node_id == peer.node_id) {
                candidates.push(peer);
            }
        }
    }
    let pkarr = state.pkarr_publisher.as_ref();
    let public_client = if state.peer_http.tls_enabled() {
        state.peer_http.probe().ok()
    } else {
        Some(state.peer_http.plain())
    };

    for peer in candidates {
        if peer.node_id == state.network.node_id || peer.gateway_port.is_none() {
            continue;
        }

        // The announce is only a candidate locator. Resolve the signed pkarr
        // record before touching the public HTTP endpoint. This keeps public
        // discovery open without turning an unadmitted peer into a cluster
        // member, and makes the geo filter apply to the DHT record itself.
        let Some(pkarr) = pkarr else {
            continue;
        };
        let Some(pkarr_record) = pkarr.resolve_public_peer(&peer.pubkey).await else {
            continue;
        };
        let pkarr_record = pkarr_record.record;
        if has_geo_filter
            && !pkarr_record
                .geo_label
                .as_deref()
                .is_some_and(|label| matches_geo_label(label, query))
        {
            continue;
        }
        let Some(client) = public_client.clone() else {
            continue;
        };
        // The address comes from the record resolved now, at dial time; it is
        // neither stored nor returned in the listing.
        let mut bases = Vec::new();
        if let Some(port) = pkarr_record.gateway_port.or(peer.gateway_port) {
            for host in pkarr_record
                .addrs
                .iter()
                .filter_map(|a| endpoint_addr_host(a))
            {
                bases.push(crate::peer_client::peer_base_url(
                    &host,
                    port,
                    state.peer_http.tls_enabled(),
                ));
            }
        }
        bases.extend(peer.gateway_candidates_with_tls(state.peer_http.tls_enabled()));
        let mut listed = false;
        for base in bases {
            let response = client
                .get(format!("{base}/api/public/network"))
                .timeout(Duration::from_millis(500))
                .send()
                .await;
            let Ok(response) = response else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            let Ok(network) = response.json::<PublicNetworkView>().await else {
                continue;
            };
            if has_geo_filter && !matches_geo_label(&network.geo_label, query) {
                continue;
            }
            geo_cache.retain(|entry| entry.node_id != peer.node_id);
            geo_cache.push(crate::db::PublicNetworkGeoCacheEntry {
                node_id: peer.node_id.clone(),
                geo_label: network.geo_label.clone(),
                fingerprint: network.fingerprint.clone(),
                observed_at: chrono::Utc::now().to_rfc3339(),
            });
            if !networks
                .iter()
                .any(|known: &PublicNetworkView| known.fingerprint == network.fingerprint)
            {
                networks.push(network);
            }
            listed = true;
            break;
        }
        // No gateway answered. A network already listed needs no second
        // answer from another of its members; otherwise ask by key.
        let known_cluster = peer.cluster_id.as_deref().is_some_and(|cluster| {
            networks
                .iter()
                .any(|known: &PublicNetworkView| known.cluster_id == cluster)
        });
        if !listed && !known_cluster && !members.contains(&peer.node_id) {
            if let Some(network) = describe_by_key(&peer.pubkey).await {
                if (!has_geo_filter || matches_geo_label(&network.geo_label, query))
                    && !networks
                        .iter()
                        .any(|known: &PublicNetworkView| known.fingerprint == network.fingerprint)
                {
                    networks.push(network);
                }
            }
        }
    }

    if let Err(error) = state.database.save_public_network_geo_cache(&geo_cache) {
        warn!("could not update the geo index of public networks: {error:#}");
    }

    networks
}

/// Tells this node's relays about its network, while it is the principal of a
/// listed one. The relay is dialed directly, so the network is listed even
/// when none of its nodes can be reached from outside - the relay never has
/// to come back for it. Sent every `PUBLICATION_INTERVAL`; a relay drops a
/// network it has not heard for `PUBLICATION_TTL`, which is also how turning
/// discovery off takes a network out of the list.
pub(crate) fn spawn_directory_publisher(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(crate::network_lobby::PUBLICATION_INTERVAL);
        loop {
            ticker.tick().await;
            if state.network.is_detached() {
                continue;
            }
            let Some(network) = local_public_network(&state).await else {
                continue;
            };
            // Only the principal publishes: a relay accepts nobody else.
            let own = state
                .pkarr_publisher
                .as_ref()
                .map(|publisher| publisher.node().public_key_uri());
            if own.as_deref() != Some(network.publisher_key.as_str()) {
                continue;
            }
            let Ok(client) = crate::network_lobby::client().await else {
                continue;
            };
            for key in directory_relay_keys(&state).await {
                let request = crate::network_lobby::LobbyRequest::Publish {
                    identity: client.proof(),
                    network: network.clone(),
                };
                match client.request(&key, request).await {
                    Ok(response) if response.accepted => {}
                    Ok(response) => debug!(
                        "lobby: relay {key} did not list the network: {:?}",
                        response.error
                    ),
                    Err(error) => {
                        debug!("lobby: relay {key} unreachable for publishing: {error:#}")
                    }
                }
            }
        }
    });
}

/// Fills `DirectoryBoard` on demand: this node's own public network and, when
/// it gathers them, the public networks it knows. The transport task answers
/// `Describe` and `Directory` from the board and wakes this task when it is
/// stale; one listing per `network_lobby::INFO_REFRESH` at most, none while
/// nobody asks.
pub(crate) fn spawn_directory_board_publisher(state: GatewayState) {
    /// The scan of announcing nodes asks each one in turn and can take
    /// minutes on a relay that hears many; an answer must not wait for it.
    const GATHER_TIMEOUT: Duration = Duration::from_secs(25);
    tokio::spawn(async move {
        let board = state.network_chat.directory_board();
        let mut gathered: Vec<PublicNetworkView> = Vec::new();
        // Once at startup, without being asked: a relay must be able to take
        // a principal's publication, and answer with it, from the first
        // minute.
        let mut first = true;
        loop {
            if !std::mem::take(&mut first) {
                board.wanted().await;
            }
            let own: Vec<PublicNetworkView> =
                local_public_network(&state).await.into_iter().collect();
            let gathers = gathers_public_networks(&state).await;
            // Put the cheap part up first, with whatever was gathered last
            // time, so requests are answered while the scan runs.
            board.publish(own.clone(), gathers.then(|| gathered.clone()));
            if gathers {
                let query = PublicNetworksQuery::default();
                let scan = collect_public_networks(&state, &query);
                match tokio::time::timeout(GATHER_TIMEOUT, scan).await {
                    Ok(found) => {
                        let mut known = found.clone();
                        resolve_places(&mut known);
                        crate::webui::remember_public_networks(&state.database, &known);
                        gathered = found;
                        board.publish(own, Some(gathered.clone()));
                    }
                    Err(_) => debug!("lobby: the public networks scan exceeded the deadline"),
                }
            }
            tokio::time::sleep(crate::network_lobby::INFO_REFRESH).await;
        }
    });
}

/// Probe the same IPv6 dual-stack bind used by the gateway before any model
/// loading starts. This makes invalid or occupied configured ports fail at
/// process startup instead of leaving a half-running node without its control
/// plane. `current` is used by the live preference endpoint, where the current
/// listener is expected to occupy the unchanged port.
pub(crate) async fn ensure_gateway_port_available(port: u16, current: Option<u16>) -> Result<()> {
    crate::config::validate_gateway_port(port)?;
    if current == Some(port) {
        return Ok(());
    }
    crate::infer_server::listener_dual_stack(port)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("the interface port {port} is not available: {error}"))
}

// ── Route handlers ────────────────────────────────────────────────────────────

pub(super) async fn cluster_quic(
    _auth: crate::auth::ConnectionAuth,
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "quic_enabled": state.auth.read().await.quic_enabled,
    }))
}

pub(super) async fn handle_relay_result(
    State(state): State<GatewayState>,
    body: Bytes,
) -> impl IntoResponse {
    crate::direct::handle_relay_result(&state.reply_inbox, body)
}

pub(super) async fn handle_remote_stream_result(
    State(state): State<GatewayState>,
    body: Bytes,
) -> impl IntoResponse {
    crate::direct::handle_remote_stream_result(&state.reply_inbox, body).await
}

/// Serves the `remote-stream` return over the same framed persistent channel
/// as the transport between slices. The credential is rechecked for each
/// frame, because the connection can outlive a change of the cluster key.
pub(super) async fn handle_remote_stream_link(
    State(state): State<GatewayState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let headers = parts.headers;
    let extensions = parts.extensions;
    let inbox = state.reply_inbox.clone();
    let auth = state.auth.clone();
    crate::link::serve(body, move |path, bytes| {
        let inbox = inbox.clone();
        let auth = auth.clone();
        let headers = headers.clone();
        let extensions = extensions.clone();
        async move {
            crate::auth::validate_internal_node_headers(&auth, &headers, &extensions, &path)
                .await
                .map_err(|_| anyhow::anyhow!("invalid peer credential"))?;
            if path != crate::direct::REMOTE_STREAM_RESULT_PATH {
                return Err(anyhow::anyhow!("unknown remote return route: {path}"));
            }
            Ok(crate::direct::handle_remote_stream_link(&inbox, bytes).await)
        }
    })
}

/// Accept raw bytes so callers don't need `Content-Type: application/json`.
pub(super) async fn handle_generate(
    auth: crate::auth::ApiKeyAuth,
    State(state): State<GatewayState>,
    body: Bytes,
) -> impl IntoResponse {
    let inference_token = auth.token().to_string();
    let caller = auth.into_caller();
    let req: GenerateRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(GenerateResponse {
                    response: format!("Invalid JSON: {}", e),
                    source: "error".to_string(),
                    tokens_generated: 0,
                    tokens_per_sec: 0.0,
                    duration_ms: 0,
                    timings: None,
                    stop_sequence: None,
                    finish_reason: None,
                    partial: false,
                    trace: None,
                }),
            )
                .into_response();
        }
    };
    let request_details = serde_json::to_value(&req).unwrap_or_else(|_| serde_json::json!({}));

    let model = match req.model.as_deref() {
        Some(m) => m.to_string(),
        None => state.active_model().await,
    };
    let requested_max_tokens = match resolve_request_max_tokens(req.max_tokens) {
        Ok(value) => value,
        Err(error) => {
            let error_for_audit = error.clone();
            let prompt_for_audit = req.prompt.clone();
            record_rejected(
                &state,
                "/api/generate",
                caller.clone(),
                model.clone(),
                prompt_for_audit,
                request_details.clone(),
                error_for_audit,
            );
            return (
                StatusCode::BAD_REQUEST,
                Json(GenerateResponse {
                    response: error,
                    source: "error".to_string(),
                    tokens_generated: 0,
                    tokens_per_sec: 0.0,
                    duration_ms: 0,
                    timings: None,
                    stop_sequence: None,
                    finish_reason: None,
                    partial: false,
                    trace: None,
                }),
            )
                .into_response();
        }
    };
    let stops = req.stop.map(StopInput::into_vec).unwrap_or_default();
    let output_limit = state
        .request_output_limit_for_model(Some(&model), requested_max_tokens, &req.prompt)
        .await;
    log_output_limit("/api/generate", output_limit);
    if let Some(error) = output_limit.rejection_reason() {
        record_rejected(
            &state,
            "/api/generate",
            caller.clone(),
            model.clone(),
            req.prompt.clone(),
            request_details.clone(),
            error.clone(),
        );
        return (
            if output_limit.capacity_pending() {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::PAYLOAD_TOO_LARGE
            },
            Json(GenerateResponse {
                response: error,
                source: "error".to_string(),
                tokens_generated: 0,
                tokens_per_sec: 0.0,
                duration_ms: 0,
                timings: None,
                stop_sequence: None,
                finish_reason: None,
                partial: false,
                trace: None,
            }),
        )
            .into_response();
    }
    if let Some(denial) = crate::quota::admit(
        &state,
        &caller,
        &req.prompt,
        crate::openai::approx_tokens(&req.prompt),
        &request_details,
    )
    .await
    {
        record_rejected(
            &state,
            "/api/generate",
            caller.clone(),
            model.clone(),
            req.prompt.clone(),
            request_details.clone(),
            denial.message.clone(),
        );
        return denial.decorate(
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(GenerateResponse {
                    response: denial.message.clone(),
                    source: "error".to_string(),
                    tokens_generated: 0,
                    tokens_per_sec: 0.0,
                    duration_ms: 0,
                    timings: None,
                    stop_sequence: None,
                    finish_reason: None,
                    partial: false,
                    trace: None,
                }),
            )
                .into_response(),
        );
    }

    info!(
        "📨 /api/generate model={} prompt_len={}",
        model,
        req.prompt.len()
    );

    match generate_blocking_with_stops(
        "/api/generate",
        caller,
        GenerationInput {
            shared_prefix: Default::default(),
            probe: None,
            trace_id: Some(Uuid::new_v4().simple().to_string()),
            state,
            inference_token,
            model,
            prompt: req.prompt,
            images: Vec::new(),
            chat_template: true,
            thinking_enabled: true,
            budget: output_limit.budget(),
            stops,
            request_details,
            // `/api/generate` receives a prompt, not a conversation: the prefix
            // is the text itself.
            cache_key: None,
            preferred_head: None,
        },
    )
    .await
    {
        Ok((text, summary)) => {
            (
                StatusCode::OK,
                Json(GenerateResponse {
                    response: text,
                    source: summary.source,
                    tokens_generated: summary.tokens_generated,
                    tokens_per_sec: summary.tokens_per_sec,
                    duration_ms: summary.duration_ms,
                    timings: Some(summary.timings),
                    stop_sequence: summary.stop_sequence,
                    // `/api/generate` keeps its established shape: ordinary
                    // stops and transport-partial responses carry neither.
                    finish_reason: None,
                    partial: false,
                    trace: summary.trace,
                }),
            )
        }
        .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(GenerateResponse {
                response: format!("Inference failed. Error: {e}."),
                source: "none".to_string(),
                tokens_generated: 0,
                tokens_per_sec: 0.0,
                duration_ms: 0,
                timings: None,
                stop_sequence: None,
                finish_reason: None,
                partial: false,
                trace: None,
            }),
        )
            .into_response(),
    }
}

pub(super) async fn handle_peers(State(state): State<GatewayState>) -> Json<PeersResponse> {
    let peers = state.all_peers().await;

    let mut summaries = Vec::new();
    for p in &peers {
        let load = state.loads.load(&p.node_id).await;
        summaries.push(PeerSummary {
            node_id: p.node_id[..p.node_id.len().min(12)].to_string(),
            addr: p.addr.clone(),
            model: p.model.clone(),
            layers: p.layer_range(),
            inference_url: p.inference_url(),
            load,
        });
    }
    let count = summaries.len();
    Json(PeersResponse {
        peers: summaries,
        count,
    })
}

pub(super) async fn handle_health(State(state): State<GatewayState>) -> Json<HealthResponse> {
    let ollama_available = state.ollama.is_available().await;
    let peers = state.all_peers().await;

    let peer_count = peers.len();
    let pipeline_peers = peers.iter().filter(|p| p.inference_port.is_some()).count();
    Json(HealthResponse {
        status: "ok",
        ollama_available,
        peer_count,
        pipeline_peers,
        default_model: state.active_model().await,
        runtime_capacity: state.network.runtime_capacity(),
    })
}

#[cfg(test)]
mod public_directory_isolation_tests {
    use super::allow_public_p2p_discovery;

    #[test]
    fn private_and_local_nodes_never_use_their_p2p_swarm_as_public_directory() {
        assert!(!allow_public_p2p_discovery(
            crate::config::NetworkVisibility::Private
        ));
        assert!(!allow_public_p2p_discovery(
            crate::config::NetworkVisibility::Local
        ));
        assert!(allow_public_p2p_discovery(
            crate::config::NetworkVisibility::Public
        ));
    }
}

/// The gateway's response compression: tower-http's defaults (never SSE,
/// images or gRPC) and never a streamed NDJSON answer either.
pub(crate) fn compression() -> tower_http::compression::CompressionLayer<
    tower_http::compression::predicate::And<
        tower_http::compression::predicate::DefaultPredicate,
        tower_http::compression::predicate::NotForContentType,
    >,
> {
    tower_http::compression::CompressionLayer::new().compress_when(
        tower_http::compression::Predicate::and(
            tower_http::compression::predicate::DefaultPredicate::new(),
            tower_http::compression::predicate::NotForContentType::const_new(
                "application/x-ndjson",
            ),
        ),
    )
}

#[cfg(test)]
mod compression_tests {
    use tower::ServiceExt;

    async fn encoding_of(content_type: &'static str) -> Option<String> {
        let app = axum::Router::new()
            .route(
                "/",
                axum::routing::get(move || async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, content_type)],
                        "x".repeat(4096),
                    )
                }),
            )
            .layer(super::compression());
        let response = app
            .oneshot(
                axum::http::Request::get("/")
                    .header("accept-encoding", "zstd, gzip")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        response
            .headers()
            .get(axum::http::header::CONTENT_ENCODING)
            .map(|v| v.to_str().unwrap().to_owned())
    }

    #[tokio::test]
    async fn a_streamed_ndjson_answer_is_never_compressed() {
        assert_eq!(encoding_of("application/x-ndjson").await, None);
        assert_eq!(encoding_of("text/event-stream").await, None);
        assert!(
            encoding_of("application/json").await.is_some(),
            "JSON still is"
        );
    }
}
