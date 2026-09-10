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

/// A referência geográfica do setup também é necessária ao editor autenticado
/// da rede, que usa a mesma tabela SQLite para evitar nomes livres divergentes.
pub(crate) async fn geo_reference(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    match state.database.geo_reference() {
        Ok(countries) => Json(json!({ "countries": countries })).into_response(),
        Err(error) => network_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "geo_reference_failed",
            &format!("ler referência geográfica: {error:#}"),
        ),
    }
}

pub(crate) async fn save_network_settings(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(settings): Json<NetworkSettings>,
) -> Response {
    let profile = state.auth.read().await.profile;
    if state
        .admin_trust
        .effective_authority(&state.authority_principal, profile)
        .is_none()
    {
        return network_error(
            StatusCode::FORBIDDEN,
            "admin_authority_required",
            "somente um administrador autorizado pode editar a rede",
        );
    }

    match apply_network_settings_with_key(&state, settings, true, None).await {
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
) -> Result<usize, String> {
    settings.public_name = settings.public_name.trim().to_string();
    settings.organization = settings.organization.trim().to_string();
    settings.description = settings.description.trim().to_string();
    settings.rules = settings.rules.trim().to_string();
    settings.channels = settings.channels.trim().to_string();
    settings.country_code = settings.country_code.trim().to_ascii_uppercase();
    settings.country_name = settings.country_name.trim().to_string();
    settings.city = settings.city.trim().to_string();
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
            if !incoming.trim().is_empty() {
                cfg.connection_key = incoming.trim().to_string();
            }
        }
        if cfg.connection_key.trim().is_empty() {
            cfg.connection_key = crate::webui::fresh_connection_key();
        }
        cfg.save()
            .map_err(|error| format!("salvar configuração da rede: {error:#}"))?;
        cfg.connection_key.clone()
    };
    state
        .database
        .save_network_settings(&settings)
        .map_err(|error| format!("salvar detalhes da rede: {error:#}"))?;

    if let Some(publisher) = &state.pkarr_publisher {
        if let Err(error) = publisher.publish_geo_label(&settings.geo_label).await {
            warn!("não foi possível republicar o rótulo geo no pkarr: {error:#}");
        }
    }

    if !fanout {
        return Ok(0);
    }
    Ok(fanout_network_settings(state, &settings, &connection_key).await)
}

async fn fanout_network_settings(
    state: &GatewayState,
    settings: &NetworkSettings,
    connection_key: &str,
) -> usize {
    let mut synced = 0;
    for peer in state.all_peers().await {
        if peer.node_id == state.network.node_id {
            continue;
        }
        let Ok(client) = state.peer_http.for_peer(&peer.node_id) else {
            warn!(peer = %peer.node_id, "não foi possível criar cliente para sincronizar detalhes da rede");
            continue;
        };
        let mut updated = false;
        let mut gateway_urls = peer.gateway_candidates();
        if gateway_urls.is_empty() {
            if let Some(url) = peer.gateway_url() {
                gateway_urls.push(url);
            }
        }
        for base in gateway_urls {
            let request = match state
                .control_json_request(
                    client
                        .post(format!("{base}{NETWORK_SETTINGS_PATH}"))
                        .timeout(crate::webui::PEER_TIMEOUT),
                    NETWORK_SETTINGS_PATH,
                    &NetworkSettingsEnvelope {
                        network: settings.clone(),
                        connection_key: if settings.visibility != NetworkVisibility::Public {
                            connection_key.to_string()
                        } else {
                            String::new()
                        },
                    },
                )
                .await
            {
                Ok(request) => request,
                Err(error) => {
                    warn!(peer = %peer.node_id, "não foi possível assinar detalhes da rede: {error:#}");
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
            warn!(peer = %peer.node_id, "peer não confirmou os detalhes atualizados da rede");
        }
    }
    synced
}

pub(crate) async fn internal_network_settings(
    auth: InternalNodeAuth,
    State(state): State<GatewayState>,
    Json(envelope): Json<NetworkSettingsEnvelope>,
) -> Response {
    let is_admin = auth.principal().is_some_and(|issuer| {
        crate::admin_trust::control_issuer_is_admin(&state.admin_trust.document(), issuer)
    });
    if !is_admin {
        return network_error(
            StatusCode::FORBIDDEN,
            "admin_authority_required",
            "a alteração da rede precisa ser assinada por uma autoridade administrativa",
        );
    }
    match apply_network_settings_with_key(
        &state,
        envelope.network,
        false,
        Some(&envelope.connection_key),
    )
    .await
    {
        Ok(_) => Json(json!({ "saved": true })).into_response(),
        Err(error) => network_error(StatusCode::BAD_REQUEST, "network_settings_invalid", &error),
    }
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
        .is_some();
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
    async fn changing_visibility_never_removes_the_internal_credential() {
        let _env_lock = crate::config::TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let state = state_with("admin", vec![]).await;
        let config_path = std::env::temp_dir().join(format!(
            "pom-network-visibility-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let previous_config = std::env::var_os("POM_CONFIG");
        std::env::set_var("POM_CONFIG", &config_path);

        let private = NetworkSettings {
            public_name: "Rede de teste".into(),
            visibility: NetworkVisibility::Private,
            ..NetworkSettings::default()
        };
        apply_network_settings_with_key(&state, private.clone(), false, None)
            .await
            .expect("rede privada inicial");
        assert_eq!(state.auth.read().await.connection_key, "chave");

        let public = NetworkSettings {
            country_code: "BR".into(),
            country_name: "Brasil".into(),
            city: "Curitiba".into(),
            visibility: NetworkVisibility::Public,
            ..private.clone()
        };
        apply_network_settings_with_key(&state, public, false, None)
            .await
            .expect("transição para rede pública");
        let public_config = state.auth.read().await;
        assert_eq!(public_config.connection_key, "chave");
        assert_eq!(crate::auth::internal_node_token(&public_config), None);
        drop(public_config);

        apply_network_settings_with_key(&state, private, false, None)
            .await
            .expect("retorno para rede privada");
        let private_config = state.auth.read().await;
        assert_eq!(private_config.connection_key, "chave");
        assert_eq!(
            crate::auth::internal_node_token(&private_config).as_deref(),
            Some("chave")
        );
        drop(private_config);

        match previous_config {
            Some(value) => std::env::set_var("POM_CONFIG", value),
            None => std::env::remove_var("POM_CONFIG"),
        }
        std::fs::remove_file(config_path).ok();
    }
}
