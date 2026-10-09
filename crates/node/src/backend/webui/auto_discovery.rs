use super::*;

#[derive(Debug, Serialize)]
pub struct AutoDiscoveryStatusResponse {
    pub enabled: bool,
    pub active: bool,
    pub searching: bool,
    pub found: bool,
    pub port: u16,
    pub peers: Vec<crate::local_discovery::LocalDiscoveryPeer>,
}

#[derive(Debug, Deserialize)]
pub struct AutoDiscoveryRequest {
    pub enabled: bool,
}

pub async fn auto_discovery_status(State(state): State<GatewayState>) -> impl IntoResponse {
    let status = state.network.local_auto_discovery_status().await;
    Json(AutoDiscoveryStatusResponse {
        enabled: state.auth.read().await.local_auto_discovery,
        active: status.active,
        searching: status.searching,
        found: status.found,
        port: status.port,
        peers: status.peers,
    })
}

pub async fn save_auto_discovery(
    State(state): State<GatewayState>,
    Json(request): Json<AutoDiscoveryRequest>,
) -> Response {
    if request.enabled {
        if state.admin_trust.document().setup.is_none() {
            let setup = match super::trust::setup_metadata_from_local_configuration(&state).await {
                Ok(Some(setup)) => setup,
                Ok(None) => {
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({ "error": "could not recover the initial network data for local discovery" })),
                    )
                        .into_response()
                }
                Err(error) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": format!("could not read the initial network data: {error:#}") })),
                    )
                        .into_response()
                }
            };
            if let Err(error) = state.admin_trust.ensure_setup_metadata(
                setup,
                &state.authority_signing_key,
                &state.authority_principal,
            ) {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": format!("could not complete the initial network data: {error:#}") })),
                )
                    .into_response();
            }
        }
        if let Err(error) = state.network.set_local_auto_discovery(true).await {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("could not enable local discovery: {error:#}") })),
            )
                .into_response();
        }
    } else {
        state.network.local_discovery().disable().await;
    }
    let save_result = {
        let mut config = state.auth.write().await;
        config.local_auto_discovery = request.enabled;
        config.save()
    };
    if let Err(error) = save_result {
        if request.enabled {
            state.network.local_discovery().disable().await;
        }
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("the preference was not saved: {error:#}") })),
        )
            .into_response();
    }
    let status = state.network.local_auto_discovery_status().await;
    Json(AutoDiscoveryStatusResponse {
        enabled: request.enabled,
        active: status.active,
        searching: status.searching,
        found: status.found,
        port: status.port,
        peers: status.peers,
    })
    .into_response()
}
