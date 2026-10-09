use super::*;
pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/deploy/plan", post(plan_endpoint))
        .route("/api/ui/deploy", post(deploy_endpoint))
        .route(
            "/api/ui/deploy/node/options",
            post(add_node_options_endpoint),
        )
        .route("/api/ui/deploy/node", post(add_node_endpoint))
        .route("/api/ui/deploy/node/remove", post(remove_node_endpoint))
        .route("/api/ui/undeploy", post(undeploy_endpoint))
        .route(
            "/api/ui/deployments",
            get(list_deployments).post(receive_deployment),
        )
        .route(
            "/api/ui/deployments/:id/activate",
            post(activate_deployment),
        )
        .route("/api/ui/deployments/:id/redeploy", post(redeploy_endpoint))
}

// ── The deployment record ────────────────────────────────────────────────────

/// The activation is the current version of a `deploy_id` in the cluster. When
/// two nodes still disagree, the newest stamp wins; the version breaks the tie
/// between copies produced at the same instant.
pub(super) fn deployment_is_newer(
    candidate: &crate::deployment::Deployment,
    current: &crate::deployment::Deployment,
) -> bool {
    candidate.activated_at > current.activated_at
        || (candidate.activated_at == current.activated_at
            && candidate.active_version > current.active_version)
}

pub(super) fn newest_deployment(
    records: impl IntoIterator<Item = crate::deployment::Deployment>,
) -> Option<crate::deployment::Deployment> {
    records.into_iter().fold(None, |current, candidate| {
        Some(match current {
            Some(current) if !deployment_is_newer(&candidate, &current) => current,
            _ => candidate,
        })
    })
}

/// Only the deployments this node keeps on disk, without asking anyone.
///
/// It is what a peer answers when the coordinator comes to gather the cluster
/// view (`?local=1`), and it is what keeps the sweep from calling itself in a
/// circle.
pub(super) fn local_deployments(
    self_id: &str,
    database: &crate::db::PomDb,
) -> anyhow::Result<serde_json::Value> {
    let all = database.deployments()?;
    Ok(json!({ "node_id": self_id, "deployments": all }))
}

#[derive(Debug, Default, Deserialize)]
pub struct ListDeploymentsQuery {
    /// Answer only with this node's records, without sweeping the cluster.
    #[serde(default)]
    pub local: bool,
}

/// Every deployment of the cluster, with the state of each one.
///
/// A deployment's record lives in one copy per participating node, so
/// **no single machine knows the whole list**: this route asks each peer and
/// joins the answers, which is the same design as `survey`. Copies of the same
/// `deploy_id` are deduplicated by keeping the one with the latest
/// `activated_at` - a node that was down during an `activate` has the old copy.
///
/// Two words that are **not** synonyms, and the screen must not confuse them:
///
/// * `serving` - a live peer announces this `deploy_id`. It means "it is up".
/// * `is_default` - this is the one that answers whoever asks only for the
///   model name, with no `@` suffix. See `deployment::active`.
///
/// A deployment can be up without being the default one of its model.
pub(super) async fn list_deployments(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Query(q): Query<ListDeploymentsQuery>,
) -> Response {
    let self_id = state.network.node_id.clone();
    if q.local {
        return match local_deployments(&self_id, &state.database) {
            Ok(body) => Json(body).into_response(),
            Err(error) => {
                tracing::error!("could not read local deployments: {error:#}");
                bad(json!({ "error": "could not read local deployments" }))
            }
        };
    }

    let peers = state.all_peers().await;
    // By `deploy_id`, keeping the most recently activated copy.
    let mut by_id: HashMap<String, crate::deployment::Deployment> = HashMap::new();
    let mut stored_by: HashMap<String, Vec<String>> = HashMap::new();

    let absorb = |who: &str,
                   records: Vec<crate::deployment::Deployment>,
                   by_id: &mut HashMap<String, crate::deployment::Deployment>,
                   stored_by: &mut HashMap<String, Vec<String>>| {
        for mut d in records {
            d.ensure_versions();
            stored_by
                .entry(d.deploy_id.clone())
                .or_default()
                .push(who.to_string());
            if by_id
                .get(&d.deploy_id)
                .map(|current| deployment_is_newer(&d, current))
                .unwrap_or(true)
            {
                by_id.insert(d.deploy_id.clone(), d);
            }
        }
    };

    let local = match state.database.deployments() {
        Ok(local) => local,
        Err(error) => {
            tracing::error!("could not read local deployments: {error:#}");
            return bad(json!({ "error": "could not read local deployments" }));
        }
    };
    absorb(&self_id, local, &mut by_id, &mut stored_by);

    let peer_state = state.clone();
    let searches = peers.iter().filter(|p| p.node_id != self_id).map(|peer| {
        let state = peer_state.clone();
        let node_id = peer.node_id.clone();
        let peer = peer.clone();
        async move {
            let resp = state
                .control_get(&peer, "/api/ui/deployments?local=true", PEER_TIMEOUT)
                .await
                .ok()?;
            let body: serde_json::Value = resp.json().await.ok()?;
            let records: Vec<crate::deployment::Deployment> =
                serde_json::from_value(body.get("deployments")?.clone()).ok()?;
            Some((node_id, records))
        }
    });

    for (node_id, records) in futures::future::join_all(searches)
        .await
        .into_iter()
        .flatten()
    {
        absorb(&node_id, records, &mut by_id, &mut stored_by);
    }

    let all: Vec<crate::deployment::Deployment> = {
        let mut v: Vec<_> = by_id.into_values().collect();
        v.sort_by(|a, b| a.deploy_id.cmp(&b.deploy_id));
        v
    };

    // "Up" is what the peers announce, not what the record says: the record
    // survives a node going down, and showing it as serving would be lying to
    // whoever is trying to understand why inference stopped.
    let announced: std::collections::BTreeSet<String> = peers
        .iter()
        .filter(|p| p.is_alive() && !p.deploy_id.is_empty())
        .map(|p| p.deploy_id.clone())
        .collect();
    let lines: Vec<serde_json::Value> = all
        .iter()
        .map(|d| {
            let is_default = crate::deployment::active(&all, &d.model.label)
                .is_some_and(|a| a.deploy_id == d.deploy_id);
            let mut holders = stored_by.get(&d.deploy_id).cloned().unwrap_or_default();
            holders.sort();
            holders.dedup();
            json!({
                "deployment": d,
                "serving": announced.contains(&d.deploy_id),
                "idle": announced.contains(&d.deploy_id)
                    && peers.iter().any(|p| p.is_alive() && p.deploy_id == d.deploy_id)
                    && peers.iter().filter(|p| p.is_alive() && p.deploy_id == d.deploy_id).all(|p| p.idle),
                "is_default": is_default,
                "holders": holders,
            })
        })
        .collect();

    Json(json!({ "node_id": self_id, "deployments": lines })).into_response()
}

/// Receives from the coordinator the record of a deployment this node is part of.
///
/// Storing it is what lets this node, after a restart, say which set it was
/// part of - and it is what makes the cluster listing survive the fall of
/// whoever coordinated.
pub(super) async fn receive_deployment(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(record): Json<crate::deployment::Deployment>,
) -> Response {
    let self_id = state.network.node_id.clone();
    // A record that does not mention this machine is not its to keep. The
    // exception is an update it already knew about: after removing a
    // machine, the snapshot must also reach the removed peer so that it does
    // not announce an old version on the next fan-out.
    let already_knew = state
        .database
        .deployment(&record.deploy_id)
        .ok()
        .flatten()
        .is_some();
    if record.node(&self_id).is_none() && !already_knew {
        return bad(json!({
            "error": "this node is not part of the deployment"
        }));
    }
    // Accept it even when incomplete - a deployment that does not close the
    // model is a real state of the cluster, and refusing the record would only
    // make the machine forget which set it is part of. But say it out loud,
    // because a coverage with a hole would only show up much later, as
    // "Gap at layer N" on the first request.
    if !record.covers_the_model() {
        warn!(
            "deployment {} does not cover the {} layers of {}",
            record.deploy_id, record.model.total_layers, record.model.label
        );
    }
    match state.database.save_deployment(&record) {
        Ok(_) => Json(json!({ "stored": true, "deploy_id": record.deploy_id })).into_response(),
        Err(e) => bad(json!({ "error": format!("{e:#}") })),
    }
}

/// Layers the new range shares with the ones already up.
///
/// It is not a refusal: it is what the screen shows the operator so he knows
/// he is creating a parallel route instead of filling a gap.
pub fn overlapping_layers(existing: &[(u32, u32)], new: (u32, u32)) -> Vec<u32> {
    (new.0..=new.1)
        .filter(|c| existing.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}

/// Position of this range in layer order, and how many ranges there are in total.
///
/// `peer_index`/`peer_count` are not the target list of a deployment: they are
/// the position in the pipeline, which is the layer order. Whoever adds one
/// node at a time has to say so explicitly, otherwise the request's only
/// target becomes "peer 1 of 1" and every added node declares itself the last
/// of the split.
///
/// Replicas of the same range occupy the same logical stage, even when they
/// come in through the incremental endpoint that adds one machine.
pub fn split_position(existing: &[(u32, u32)], new: (u32, u32)) -> (u32, u32) {
    let mut ranges: Vec<(u32, u32)> = existing.to_vec();
    ranges.push(new);
    ranges.sort();
    ranges.dedup();
    let index = ranges.iter().position(|f| *f == new).unwrap_or(0) as u32;
    (index, ranges.len() as u32)
}

/// Layers left with nobody after taking this machine out.
///
/// Empty means another machine covers the same range - the network loses a
/// parallel route and keeps serving, and the notice to the operator is mild.
/// Non-empty means a gap, and then the confirmation is explicit.
pub fn uncovered_after_removal(remaining: &[(u32, u32)], removed: (u32, u32)) -> Vec<u32> {
    (removed.0..=removed.1)
        .filter(|c| !remaining.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}
