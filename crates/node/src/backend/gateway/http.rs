use super::*;

// ── Server startup ────────────────────────────────────────────────────────────

pub(crate) type GatewayReadySender = std::sync::mpsc::SyncSender<()>;

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
    let admin = admin::admin_routes().with_state(state.clone());
    let app = Router::new()
        .route("/api/generate", post(handle_generate))
        .route("/generate", post(handle_generate)) // alias
        .route("/api/access", get(admin_access_status))
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
        .route(crate::tls_policy::TLS_POLICY_PATH, get(cluster_tls))
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
        .route("/handshake", post(session::handle_handshake))
        // Onde a cauda de um pipeline entrega o resultado. A rota é interna ao
        // pipeline: o Ticket é a credencial por quadro.
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
        .layer(tower_http::compression::CompressionLayer::new())
        // Toda resposta diz de qual nó veio; ver `peer_client::answered_by_other_node`.
        .layer(middleware::map_response(crate::peer_client::stamp_node_id));

    // Both startup paths (with a model and gateway-only) come through here, so
    // this is the one place that warms every node's catalog exactly once.
    crate::webui::warm_catalog(warm);

    let listener = crate::infer_server::bind_dual_stack(port).await?;
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    info!(
        "🌐 Interface e API em http://localhost:{port}  \
         — UI /  ·  OpenAI /v1/chat/completions  ·  Anthropic /v1/messages  ·  \
         POST /api/generate  ·  GET /peers /health"
    );
    // Nagle atrasa uma escrita pequena até o ACK da anterior. No SSE cada token
    // é uma escrita pequena, e a interação com o delayed-ACK do outro lado soma
    // dezenas de ms ao que o cliente sente. O `reqwest` já desliga isso no lado
    // cliente por padrão; o `axum::serve` deixava `tcp_nodelay` em `None`, então
    // o lado servidor ficava com Nagle ligado em todas as respostas. O
    // `tcp_nodelay` mudou de lugar, não sumiu: `serve` o aplica em cada conexão
    // aceita, junto com a decisão entre TLS e texto claro.
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
    pub city: String,
    pub geo_label: String,
    pub access_policy: crate::config::NetworkAccessPolicy,
    /// Enrollment roles the network advertises as open; empty means it is not
    /// accepting new members and the directory disables its join button.
    #[serde(default)]
    pub accepted_enrollments: crate::config::AcceptedEnrollments,
    /// The network answers lobby chat from non-members right now (the
    /// operator's switch and the network Chat are both on).
    #[serde(default)]
    pub lobby_chat: bool,
    /// The network publishes models, node count and VRAM to non-members,
    /// asked live from its principal.
    #[serde(default)]
    pub public_stats: bool,
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
        lobby_chat: view.lobby_chat,
        public_stats: view.public_stats,
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
        city: network.city,
        geo_label: network.geo_label,
        access_policy: network.access_policy,
        accepted_enrollments: network.accepted_enrollments,
        lobby_chat: network.lobby_chat,
        public_stats: network.public_stats,
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
        .map_err(|error| anyhow::anyhow!("cadeia pública inválida: {error:#}"))?;
    if document.anchor.cluster_id != record.network.cluster_id
        || crate::admin_trust::anchor_fingerprint(&document) != record.network.fingerprint
    {
        return Err(anyhow::anyhow!(
            "o registro não corresponde à raiz de confiança anunciada"
        ));
    }
    if document.primary.pubkey != record.publisher_pubkey {
        return Err(anyhow::anyhow!(
            "somente o principal da rede pode publicar seu registro"
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
                .get(format!("{}/api/public/networks", base.trim_end_matches('/')))
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
            warn!("registro do diretório federado recusado: {error:#}");
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
            Err(error) => warn!("registro do diretório federado recusado: {error:#}"),
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
            warn!("não foi possível salvar o cache do diretório federado: {error:#}");
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
            "{} publicação(ões) de anti-entropia do diretório federado falharam",
            failures
        );
    }
}

async fn publish_local_directory_record(state: &GatewayState) -> Result<()> {
    let settings = directory_settings(state);
    if settings.replicas.is_empty() {
        return Ok(());
    }
    if state.auth.read().await.profile != crate::profile::NodeProfile::Admin {
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
            warn!("não foi possível publicar no ingresso do diretório federado: {error}");
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
                warn!("publicação no diretório federado falhou: {error:#}");
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
        warn!("não foi possível podar o cache do diretório federado: {error:#}");
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
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("não foi possível persistir o registro: {error:#}") }))).into_response();
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
    let (visibility, configured_network) = {
        let config = state.auth.read().await;
        (config.network.visibility, config.network.clone())
    };
    if visibility != crate::config::NetworkVisibility::Public || !state.admin_trust.is_anchored() {
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
        city: network.city.clone(),
        geo_label: network.geo_label.clone(),
        access_policy: network.access_policy,
        accepted_enrollments: network.accepted_enrollments.clone(),
        lobby_chat: network.lobby_chat && state.network_chat.enabled(),
        public_stats: network.public_stats,
        location: network
            .location
            .clone()
            .map(|location| crate::config::NetworkLocation {
                latitude: location.latitude,
                longitude: location.longitude,
                accuracy_m: None,
            }),
        discovered_via: "pkarr".to_string(),
        directory_replicas,
    })
}

/// Host of an IP multiaddress (brackets for IPv6), to build the gateway URL at
/// dial time.
fn multiaddr_host(addr: &str) -> Option<String> {
    use libp2p::multiaddr::Protocol;
    addr.parse::<libp2p::Multiaddr>()
        .ok()?
        .iter()
        .find_map(|protocol| match protocol {
            Protocol::Ip4(ip) => Some(ip.to_string()),
            Protocol::Ip6(ip) => Some(format!("[{ip}]")),
            Protocol::Dns(host) | Protocol::Dns4(host) | Protocol::Dns6(host) => {
                Some(host.to_string())
            }
            _ => None,
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

pub(crate) fn locator_from(record: Option<&crate::pkarr_node::PkarrRecord>) -> NetworkLocator {
    match record {
        None => NetworkLocator {
            reachability: "unresolved",
            relays: 0,
        },
        Some(record) => NetworkLocator {
            reachability: if !record.multiaddrs.is_empty() {
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
    keys
}

/// `GET /api/public/networks/locate?key=pk:...` - resolves the publisher's key
/// through pkarr and says whether the network has a direct path, only a relay
/// path, or no record. The node does the resolving: the browser does not speak
/// libp2p and must not receive anyone's IP.
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
    Json(networks)
}

/// The public networks the relay under `key` knows, each checked against its
/// own trust document. A relay that is still filling its board is asked once
/// more; one that does not answer contributes nothing.
pub(crate) async fn relay_directory(key: &str) -> Vec<PublicNetworkView> {
    let Ok(client) = crate::network_lobby::client().await else {
        return Vec::new();
    };
    let mut response = match client.request(key, crate::network_lobby::LobbyRequest::Directory).await {
        Ok(response) => response,
        Err(error) => {
            debug!("lobby: relé {key} não respondeu ao diretório: {error:#}");
            return Vec::new();
        }
    };
    if response.accepted && response.pending {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Ok(again) = client.request(key, crate::network_lobby::LobbyRequest::Directory).await {
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
        warn!("não foi possível atualizar o registro local do diretório federado: {error:#}");
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
    let members: std::collections::HashSet<String> = candidates
        .iter()
        .map(|peer| peer.node_id.clone())
        .collect();
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
                .multiaddrs
                .iter()
                .filter_map(|a| multiaddr_host(a))
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
        warn!("não foi possível atualizar o índice geo de redes públicas: {error:#}");
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
                        "lobby: relé {key} não listou a rede: {:?}",
                        response.error
                    ),
                    Err(error) => debug!("lobby: relé {key} inalcançável para publicar: {error:#}"),
                }
            }
        }
    });
}

/// Fills `DirectoryBoard` on demand: this node's own public network and, when
/// it gathers them, the public networks it knows. The swarm loop answers
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
                        gathered = found;
                        board.publish(own, Some(gathered.clone()));
                    }
                    Err(_) => debug!("lobby: a varredura de redes públicas passou do prazo"),
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
        .map_err(|error| {
            anyhow::anyhow!("a porta da interface {port} não está disponível: {error}")
        })
}

// ── Route handlers ────────────────────────────────────────────────────────────

/// O que o principal responde à catraca dos outros nós.
///
/// Autenticada pelo envelope assinado da cadeia de confiança. Em rede privada
/// a chave compartilhada pode acompanhar a assinatura como guarda de transporte;
/// isso impede um intruso de responder pelo principal e rebaixar o cluster para
/// texto claro. Ver `tls_policy`.
pub(super) async fn cluster_tls(
    _auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "tls_required": state.auth.read().await.tls_required,
    }))
}

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

/// Serve o retorno do `remote-stream` pelo mesmo canal persistente enquadrado
/// que o transporte entre as fatias. A credencial é reconferida para cada
/// frame, porque a conexão pode sobreviver à troca da chave do cluster.
pub(super) async fn handle_remote_stream_link(
    State(state): State<GatewayState>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> axum::response::Response {
    let inbox = state.reply_inbox.clone();
    let auth = state.auth.clone();
    crate::link::serve(body, move |path, bytes| {
        let inbox = inbox.clone();
        let auth = auth.clone();
        let headers = headers.clone();
        async move {
            crate::auth::validate_internal_node_headers(&auth, &headers)
                .await
                .map_err(|_| anyhow::anyhow!("credencial de peer inválida"))?;
            if path != crate::direct::REMOTE_STREAM_RESULT_PATH {
                return Err(anyhow::anyhow!(
                    "rota de retorno remoto desconhecida: {path}"
                ));
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

    info!(
        "📨 /api/generate model={} prompt_len={}",
        model,
        req.prompt.len()
    );

    match generate_blocking_with_stops(
        "/api/generate",
        caller,
        GenerationInput {
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
            // `/api/generate` recebe um prompt, não uma conversa: o prefixo é
            // o próprio texto.
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
            ollama_url: p.ollama_url(),
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
