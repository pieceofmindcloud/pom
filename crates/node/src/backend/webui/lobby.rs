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

#[derive(Debug, Default, Deserialize)]
pub struct SetupPublicNetworksQuery {
    /// Directory ingresses typed in the wizard, comma separated. They add to
    /// the ones already configured for this installation.
    #[serde(default)]
    pub replicas: String,
}

/// `GET /api/setup/public-networks` - the public networks the federated
/// directory lists. Setup mode has no cluster and no pkarr discovery, so this
/// is the directory alone: the cached records merged with what the replicas
/// answer now.
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
    let client = reqwest::Client::new();
    Json(crate::gateway::directory_public_networks(&state.database, &client, replicas).await)
        .into_response()
}
