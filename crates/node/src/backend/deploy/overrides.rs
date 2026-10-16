//! One machine's own parameters in a live deployment: `POST /api/ui/deployments/:id/nodes/:node_id/overrides`.
//!
//! The body is the whole set of overrides for that machine (an empty body clears them). The machine reloads with the
//! deployment's version parameters merged with them (`reconfigure_one`, the same path tensor parallelism uses to
//! reload one machine), and only if it accepts does the change go into the active version. Nothing else reloads,
//! and no new version is created: changing an override edits the version that is running.

use super::*;
use crate::deployment::NodeOverrides;

pub(super) async fn node_overrides_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Path((id, node_id)): Path<(String, String)>,
    Json(overrides): Json<NodeOverrides>,
) -> Response {
    if !crate::deployment::is_deploy_id(&id) {
        return bad(json!({ "error": format!("invalid deploy_id: {id}") }));
    }
    if let Err(message) = overrides.validate() {
        return bad(json!({ "error": message }));
    }
    let Some(mut record) = find_record(&state, &id).await else {
        return bad(json!({ "error": format!("unknown deployment {id}") }));
    };
    record.ensure_versions();
    if record.node(&node_id).is_none() {
        return bad(json!({ "error": format!("{node_id} is not a machine of deployment {id}") }));
    }
    let base = record
        .active()
        .map(|version| version.params.clone())
        .unwrap_or_default();
    let effective = overrides.apply(&base);

    let self_id = state.network.node_id.clone();
    let peers = state.all_peers().await;
    if let Some(error) = reconfigure_one(&state, &peers, &node_id, &self_id, "", &effective).await {
        return (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({ "error": error, "applied": false })),
        )
            .into_response();
    }

    // The machine accepted: what it runs with is what the record says from now on.
    if let Err(e) = record.set_node_overrides(&node_id, overrides.clone()) {
        return bad(json!({ "error": e.to_string() }));
    }
    if let Some(node) = record.nodes.iter_mut().find(|n| n.node_id == node_id) {
        node.config = effective;
    }
    record.commit_active_nodes();
    publish_record(&state, &record).await;

    Json(json!({
        "deploy_id": id,
        "node_id": node_id,
        "version": record.active_version,
        "overrides": overrides,
        "applied": true,
    }))
    .into_response()
}
