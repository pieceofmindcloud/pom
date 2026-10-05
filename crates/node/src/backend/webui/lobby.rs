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

async fn exchange(key: &str, post: Option<(String, String)>) -> Response {
    let key = key.trim();
    let client = match network_lobby::client().await {
        Ok(client) => client,
        Err(error) => {
            warn!("lobby: não foi possível iniciar o cliente: {error:#}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "lobby_unavailable" })),
            )
                .into_response();
        }
    };
    let card = local_card().await;
    let request = match post {
        Some((message_id, text)) => LobbyRequest::Post {
            card,
            message_id,
            text,
        },
        None => LobbyRequest::Fetch { card },
    };
    match client.request(key, request).await {
        // A refusal by the principal (`accepted: false`) is an answer, not a
        // transport failure: the screen reads `error` and says why.
        Ok(response) => Json(json!({
            "accepted": response.accepted,
            "error": response.error,
            "node_id": if response.node_id.is_empty() {
                client.node_id().to_string()
            } else {
                response.node_id
            },
            "card": local_card().await,
            "messages": response.messages,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

/// `GET /api/ui/network-chat/lobby?key=pk:...` - recent lobby history of the
/// network published under `key`, as its principal answers it now.
pub(super) async fn lobby_fetch(_auth: ConnectionAuth, Query(query): Query<LobbyQuery>) -> Response {
    exchange(&query.key, None).await
}

/// `POST /api/ui/network-chat/lobby` - say something in that lobby.
pub(super) async fn lobby_post(_auth: ConnectionAuth, Json(post): Json<LobbyPost>) -> Response {
    exchange(&post.key, Some((post.message_id, post.text))).await
}

/// `GET /api/setup/lobby?key=pk:...` - the same, from the setup wizard.
pub async fn setup_lobby_fetch(Query(query): Query<LobbyQuery>) -> Response {
    exchange(&query.key, None).await
}

/// `POST /api/setup/lobby`.
pub async fn setup_lobby_post(Json(post): Json<LobbyPost>) -> Response {
    exchange(&post.key, Some((post.message_id, post.text))).await
}

async fn info_exchange(key: &str) -> Response {
    let client = match network_lobby::client().await {
        Ok(client) => client,
        Err(error) => {
            warn!("lobby: não foi possível iniciar o cliente: {error:#}");
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
    replicas.extend(
        query
            .replicas
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );
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
    Json(networks).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

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
