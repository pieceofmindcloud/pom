//! Model -> sidecar links: which vision projector and which MTP draft belong to
//! a model. The principal owns the list; every other node reads it from there
//! and keeps the last copy for when the principal does not answer.
use super::*;

const LINKS_PATH: &str = "/api/ui/model-sidecar-links";

/// The principal as a peer this node can reach, when it is not this node.
async fn primary_peer(state: &GatewayState) -> Option<crate::network::PeerInfo> {
    let primary = state.admin_trust.current_primary();
    state.all_peers().await.into_iter().find(|peer| {
        peer.node_id == primary.node_id
            && crate::identity::trust_peer_id_from_hex(&peer.pubkey).as_deref() == Some(primary.peer_id.as_str())
            && peer.pubkey == primary.pubkey
            && peer.gateway_port.is_some()
    })
}

/// The network's links: the principal's own list here, or read from it.
///
/// A secondary that cannot reach the principal answers with its last copy and
/// says so (`stale`), instead of an empty list that would read as "no links".
pub(crate) async fn load_links(state: &GatewayState) -> (Vec<crate::catalog::SidecarLink>, bool) {
    if local_is_primary(state).await {
        return (
            state.database.model_sidecar_links().unwrap_or_default(),
            false,
        );
    }
    if let Some(peer) = primary_peer(state).await {
        if let Ok(response) = state.control_get(&peer, LINKS_PATH, PEER_TIMEOUT).await {
            if response.status().is_success() {
                if let Ok(body) = response.json::<serde_json::Value>().await {
                    if let Some(links) = body.get("links").cloned().and_then(|value| {
                        serde_json::from_value::<Vec<crate::catalog::SidecarLink>>(value).ok()
                    }) {
                        if let Err(error) = state.database.replace_model_sidecar_links(&links) {
                            warn!("could not mirror the vision and MTP links: {error:#}");
                        }
                        return (links, false);
                    }
                }
            }
        }
    }
    (
        state.database.model_sidecar_links().unwrap_or_default(),
        true,
    )
}

pub(super) async fn sidecar_links(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let (links, stale) = load_links(&state).await;
    Json(json!({ "links": links, "stale": stale })).into_response()
}

/// Save one link. Only the principal writes; anyone else forwards it there.
pub(super) async fn save_sidecar_link(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(mut link): Json<crate::catalog::SidecarLink>,
) -> Response {
    if let Err(error) = link.validate() {
        return bad_request(error);
    }
    if !local_is_primary(&state).await {
        let Some(peer) = primary_peer(&state).await else {
            return bad_request("the principal node is not available to store the link");
        };
        return match state
            .control_post_json(&peer, LINKS_PATH, &link, PEER_TIMEOUT)
            .await
        {
            Ok(response) if response.status().is_success() => {
                let (links, stale) = load_links(&state).await;
                Json(json!({ "links": links, "stale": stale })).into_response()
            }
            Ok(response) => {
                let status = response.status();
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "the principal node refused the link".to_string());
                (status, body).into_response()
            }
            Err(error) => bad_request(format!("the principal node is unreachable: {error:#}")),
        };
    }
    if link.mode != crate::catalog::SidecarMode::File {
        link.file_name = None;
        link.size_bytes = None;
    }
    link.updated_at = chrono::Utc::now().to_rfc3339();
    link.updated_by = state.network.node_id.clone();
    if let Err(error) = state.database.save_model_sidecar_link(&link) {
        return bad_request(format!("could not store the link: {error:#}"));
    }
    let (links, stale) = load_links(&state).await;
    Json(json!({ "links": links, "stale": stale })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{SidecarKind, SidecarLink, SidecarMode};

    fn link(kind: SidecarKind, mode: SidecarMode, file: Option<(&str, u64)>) -> SidecarLink {
        SidecarLink {
            model_key: "Model-Q2-00001-of-00003.gguf".into(),
            kind,
            mode,
            file_name: file.map(|(name, _)| name.to_string()),
            size_bytes: file.map(|(_, size)| size),
            updated_at: String::new(),
            updated_by: String::new(),
        }
    }

    #[tokio::test]
    async fn the_principal_keeps_one_link_per_model_and_kind() {
        let state = crate::gateway::testkit::state_with("eu", vec![]).await;
        state.auth.write().await.profile = crate::profile::NodeProfile::Admin;
        assert!(
            local_is_primary(&state).await,
            "the testkit must be the principal"
        );

        let save = |link: SidecarLink| {
            let state = state.clone();
            async move {
                save_sidecar_link(ConnectionAuth, State(state), Json(link))
                    .await
                    .into_response()
                    .status()
            }
        };
        assert_eq!(
            save(link(
                SidecarKind::Mtp,
                SidecarMode::File,
                Some(("draft.gguf", 34))
            ))
            .await,
            StatusCode::OK
        );
        assert_eq!(
            save(link(SidecarKind::Mtp, SidecarMode::Embedded, None)).await,
            StatusCode::OK
        );
        assert_eq!(
            save(link(
                SidecarKind::Mmproj,
                SidecarMode::File,
                Some(("mmproj.gguf", 7))
            ))
            .await,
            StatusCode::OK
        );
        // Vision never comes embedded, and a file link needs its identity.
        assert_eq!(
            save(link(SidecarKind::Mmproj, SidecarMode::Embedded, None)).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            save(link(SidecarKind::Mtp, SidecarMode::File, None)).await,
            StatusCode::BAD_REQUEST
        );

        let (links, stale) = load_links(&state).await;
        assert!(!stale);
        assert_eq!(links.len(), 2);
        let mtp = links
            .iter()
            .find(|link| link.kind == SidecarKind::Mtp)
            .unwrap();
        assert_eq!(mtp.mode, SidecarMode::Embedded);
        assert_eq!(mtp.file_name, None);
        assert_eq!(mtp.updated_by, "eu");
    }
}
