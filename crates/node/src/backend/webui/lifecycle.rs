use super::*;
// ── Cluster launch ───────────────────────────────────────────────────────────

/// Stop the inference server on this node, announce the node as idle,
/// and free the inference port. Called by `launch` before reconfiguring
/// peers so no stale endpoint receives inference requests mid-transition.
pub(super) async fn stop_inference(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    info!("stop-inference received — stopping the server on this node");
    state.runtime.stop().await;
    Json(json!({ "stopped": true })).into_response()
}

// ── Taking a node out of service ─────────────────────────────────────────────
//
// Two different things the operator asks for, deliberately separate routes even
// though one is currently a subset of the other: unloading frees this machine's
// memory and leaves it in the cluster, ready to be given a range again; leaving
// takes the machine off the network entirely.

/// Free this machine's memory: kill the inference server, forget the range, and
/// tell the peers this node serves nothing.
///
/// Mechanically the same work `stop-inference` does during a launch, but that
/// route exists for the launch orchestration and says nothing back about what
/// was released — which is the whole answer the operator is waiting for here.
pub(super) async fn unload_model(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let unloaded = state.runtime.config().await;
    if unloaded.is_none() {
        return Json(json!({ "unloaded": false, "reason": "no model loaded" })).into_response();
    }
    state.runtime.stop().await;
    info!("model unloaded from the interface");
    Json(json!({
        "unloaded": true,
        "model": unloaded.as_ref().map(|c| c.model_label.clone()),
        "start_layer": unloaded.as_ref().map(|c| c.start_layer),
        "end_layer": unloaded.as_ref().map(|c| c.end_layer),
    }))
    .into_response()
}

/// Other parameters for the slice this node already serves.
///
/// Only the parameters: what identifies the slice (file, layer range,
/// position in the pipeline) is not here and cannot be changed through this door.
/// See `NodeConfig::with_params`.
#[derive(Debug, Deserialize)]
pub struct ReconfigureRequest {
    pub params: RuntimeParams,
    /// The slice identity, when the caller already knows it (a `redeploy`
    /// that unloaded everything before coming up again). Absent = the node re-reads its
    /// own live `NodeConfig`, which is the usual path and only works for
    /// a machine that is already serving.
    #[serde(default)]
    pub slice: Option<ReconfigureSlice>,
}

/// What identifies the slice a node should serve, for the case where it is
/// **not** serving anything and there is no live `NodeConfig` to start from.
///
/// Filled in by the coordinator of a `redeploy` from the deployment
/// record plus the snapshot of what each node was running before the
/// unload. `model_path` is the photographed live path; without it, `part_sha256`
/// resolves the local file by content (`transfer::find_part`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconfigureSlice {
    #[serde(default)]
    pub model_path: Option<String>,
    #[serde(default)]
    pub mmproj_path: Option<String>,
    /// The external MTP draft the node had loaded before the unload.
    #[serde(default)]
    pub mtp_draft_path: Option<String>,
    #[serde(default)]
    pub part_sha256: Option<String>,
    pub model_label: String,
    #[serde(default)]
    pub deployment_name: String,
    #[serde(default)]
    pub deploy_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    #[serde(default = "one_u32")]
    pub peer_count: u32,
    #[serde(default)]
    pub peer_index: u32,
    #[serde(default)]
    pub primary: bool,
    #[serde(default)]
    pub single_peer: bool,
    #[serde(default)]
    pub no_embeddings: Option<bool>,
    #[serde(default)]
    pub tp: Option<crate::network::TpIdentity>,
}

pub(super) fn one_u32() -> u32 {
    1
}

/// Reloads this machine's model with other parameters.
///
/// It is the "redeploy by changing a parameter" path: nothing is planned, nothing
/// is copied and no range changes owner - the node re-reads the **same** file with,
/// say, another `n_ctx`. Going through the whole `deploy` for this would mean
/// re-scanning the cluster and minting a new deployment, when what changed fits
/// in an `apply`.
///
/// It refuses when nothing is loaded: reconfiguring the empty makes no sense, and
/// inventing a slice from loose parameters would be guessing which one.
pub(super) async fn reconfigure(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<ReconfigureRequest>,
) -> Response {
    match reconfigure_local(state, req.params, req.slice).await {
        Ok(cfg) => Json(json!({
            "reconfigured": true,
            "model": cfg.model_label,
            "start_layer": cfg.start_layer,
            "end_layer": cfg.end_layer,
        }))
        .into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// The reconfiguration itself, without HTTP around it.
///
/// It exists separately because the coordinator of a `redeploy` is also a node of the
/// deployment: when the machine to reconfigure is itself, calling its own
/// route over the network would be going around the world to talk to itself.
pub(crate) async fn reconfigure_local(
    state: GatewayState,
    params: RuntimeParams,
    slice: Option<ReconfigureSlice>,
) -> Result<NodeConfig, String> {
    let cfg = match slice {
        // Identity given: builds the `NodeConfig` from it, whether or not the node serves anything.
        // It is the path of the `redeploy` that unloads everything before coming up.
        Some(slice) => {
            // How to find the file, in order: the photographed live path; the
            // part by content (`part_sha256`); the local catalog matching
            // label + layer range. Only when all three fail is it an error.
            let shard_dir = state.shard_dir().await;
            let model_path = if let Some(path) = slice.model_path.clone().filter(|p| !p.is_empty())
            {
                path
            } else if let Some(path) = slice
                .part_sha256
                .as_deref()
                .and_then(|sha| crate::transfer::find_part(&shard_dir, sha))
            {
                path.to_string_lossy().into_owned()
            } else if let Some(path) = resolve_deployment_slice_file(
                &state,
                &slice.model_label,
                slice.start_layer,
                slice.end_layer,
                slice.total_layers,
                slice.peer_count,
            )
            .await
            {
                path
            } else {
                return Err(format!(
                    "this machine no longer has the file of range L{}-{} of \"{}\" ({}); use the Deploy screen to copy it again",
                    slice.start_layer,
                    slice.end_layer,
                    slice.model_label,
                    slice
                        .part_sha256
                        .as_deref()
                        .map(|s| format!("hash {}", &s[..12.min(s.len())]))
                        .unwrap_or_else(|| "no part hash in the record".to_string()),
                ));
            };
            let tp_none = slice.tp.is_none();
            // `from_params` leaves `inference_port` at 0; the slice identity
            // does not carry it. Without this override the inference server came up
            // on `[::]:0` (ephemeral port) on a redeploy, and the tunnel/direct HTTP
            // could no longer find the peer.
            let inference_port = state.network.inference_port.ok_or_else(|| {
                "this node does not announce an inference port; the slice cannot be reconfigured"
                    .to_string()
            })?;
            // First the MTP draft the node had loaded, if the file
            // still exists: the deployment resolves it by the model's sidecar link,
            // outside the slice folder, and only the neighbour search lost it
            // on every redeploy. Without it, the search next to the file still applies
            // to a draft added later. See `run_apply`.
            let mtp_draft_path = slice
                .mtp_draft_path
                .clone()
                .filter(|path| std::path::Path::new(path).is_file())
                .or_else(|| {
                    crate::catalog::find_mtp_draft_sibling(std::path::Path::new(&model_path))
                });
            NodeConfig {
                deploy_id: slice.deploy_id,
                deployment_name: slice.deployment_name,
                model_path,
                mmproj_path: slice.mmproj_path,
                mtp_draft_path,
                model_label: slice.model_label,
                start_layer: slice.start_layer,
                end_layer: slice.end_layer,
                total_layers: slice.total_layers,
                peer_count: slice.peer_count.max(1),
                peer_index: slice.peer_index,
                primary: slice.primary,
                tp: slice.tp,
                inference_port,
                single_peer: slice.single_peer,
                no_embeddings: slice.no_embeddings.unwrap_or(params.no_embeddings),
                ..NodeConfig::from_params(&params, tp_none)
            }
        }
        // No identity: only swaps the parameters of what is already loaded.
        None => {
            let Some(current) = state.runtime.config().await else {
                warn!("reconfigure refused: this node is not serving anything");
                return Err("this node is not serving anything to reconfigure".into());
            };
            current.with_params(&params)
        }
    };

    info!(
        "reconfiguring {} (layers {}-{}) with new parameters",
        cfg.model_label, cfg.start_layer, cfg.end_layer
    );

    state
        .runtime
        .apply(cfg.clone())
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok(cfg)
}

/// Which machine the "leave the network" refers to.
#[derive(Debug, Default, Deserialize)]
pub struct LeaveRequest {
    /// Machine that should leave. Absent, or the node's own `node_id`, means this one.
    #[serde(default)]
    pub node_id: Option<String>,
}

/// Take a node off the network: unload, stop announcing, forget the peers.
///
/// The HTTP gateway keeps serving, because the way back in is this same screen.
///
/// An absent body or one without `node_id` keeps the old behaviour - this node leaves -,
/// which is what the "This node" menu and the Network screen always asked for. With the `node_id`
/// of another machine the request is **relayed** to the same route there, as
/// `dev/update` already does: the panel only talks to its own node (relative paths
/// in `lib/api.ts`), and a peer card on the Dashboard needs to reach that
/// machine. The one that shuts down is always the network owner of that process; this node
/// has no way to detach another from outside.
///
/// The two halves have different owners, and that is why the scope table
/// cannot handle it alone: leaving is **abdicating one's own post**, and no profile
/// needs a license for it; telling **another** machine to leave is an
/// administrator's act, and the relay travels with the internal credential, which
/// the target does not judge by the profile of whoever asked. Without this lock,
/// the route's wider scope
/// would turn "I may leave" into "I may expel anyone".
pub(super) async fn leave_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    body: Option<Json<LeaveRequest>>,
) -> Response {
    let target = body.and_then(|Json(b)| b.node_id).unwrap_or_default();
    if !target.is_empty() && target != state.network.node_id {
        if !super::local_is_primary(&state).await {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "only an administrator removes another machine from the network",
                    "node_id": target,
                })),
            )
                .into_response();
        }
        return forward_leave(&state, &target).await;
    }

    state.runtime.stop().await;
    state.network.detach().await;
    info!("node left the network from the interface");
    Json(json!({ "detached": true })).into_response()
}

/// Leave this network permanently: revoke the local trust anchor, clear the
/// network-owned part of `pom.json`, then restart into the real setup router.
/// Local hardware preferences stay in the file so returning to the wizard does
/// not make the operator repeat machine-specific choices.
pub(super) async fn leave_network_and_reset(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    // A node's own network is inherent to it and is never dropped. Leaving
    // only means something for a network the node joined.
    let own_network =
        state.admin_trust.document().primary.pubkey == state.authority_principal.pubkey;
    if own_network && !super::standby::in_standby() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "reason": "own_network",
                "error": "this is this machine's own network; it cannot be left",
            })),
        )
            .into_response();
    }
    // Before going silent, tell the network's principal. It forgets this
    // machine's admission and gives the remaining members a new connection
    // key, so the one this node holds stops being worth anything. Best
    // effort: a principal that cannot be reached is not a reason to stay.
    super::notify_principal_of_departure(&state).await;

    state.runtime.stop().await;
    state.network.detach().await;

    let trust_path = crate::config::admin_trust_path();
    if let Err(error) = std::fs::remove_file(&trust_path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("could not delete the trust root: {error}") })),
            )
                .into_response();
        }
    }

    let mut config = match crate::config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("could not read the configuration: {error:#}") })),
            )
                .into_response()
        }
    };
    // The connection key of the network being left goes with it: after the
    // principal rotates it, it opens nothing. Then this machine's own
    // network comes back from standby as it was; only a node that never had
    // one (it was set up as a member by the wizard) falls to a fresh start.
    config.prepare_for_setup();
    let restored = match super::standby::restore_primary(&mut config) {
        Ok(restored) => restored,
        Err(error) => {
            warn!("could not restore the own network; creating a new one: {error:#}");
            false
        }
    };
    if restored {
        // Reaching this machine's interface is the operator's choice, not a
        // property of either network.
        let current = state.auth.read().await;
        config.admin_exposure = current.admin_exposure;
        config.admin_access_token = current.admin_access_token.clone();
    }
    if let Err(error) = config.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("could not prepare the next setup: {error:#}") })),
        )
            .into_response();
    }
    if restored {
        super::standby::discard();
    }
    if let Err(error) = state.database.save_network_settings(&config.network) {
        warn!("could not clear the network details in the database: {error:#}");
    }
    if let Err(error) = state.database.clear_api_keys() {
        warn!("could not clear the local API keys: {error:#}");
    }

    info!(
        trust = %trust_path.display(),
        "node left the network; trust revoked and setup pending"
    );
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        let error = crate::respawn::respawn_self();
        tracing::error!("respawn after leaving the network failed, the node stays up: {error}");
    });
    Json(json!({ "left": true, "restored_own_network": restored, "restart": true })).into_response()
}

/// Deadline of a forwarded `/api/ui/apply`: the peer client's
/// (`PeerClients::new(600 s)`), which was what held before the call had its
/// own deadline. The peer answers when it accepts the job, not when the load ends.
const APPLY_FORWARD_TIMEOUT: Duration = Duration::from_secs(600);

/// Relays the "leave the network" to the chosen peer, without `node_id` - there it is local.
pub(super) async fn forward_leave(state: &GatewayState, node_id: &str) -> Response {
    let Some(peer) = state
        .all_peers()
        .await
        .into_iter()
        .find(|p| p.node_id == node_id)
    else {
        return bad_request("unknown peer");
    };
    let body = json!({});
    match state
        .control_post_json(&peer, "/api/ui/leave", &body, PEER_STOP_TIMEOUT)
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!("{node_id} left the network at the request of this node");
            Json(json!({ "detached": true, "node_id": node_id })).into_response()
        }
        Ok(r) => bad_request(format!("the machine refused: HTTP {}", r.status())),
        Err(e) => bad_request(format!("machine unreachable: {e}")),
    }
}

// ── Releasing a slice, and the network taking it back up ─────────────────────

/// What the operator asked for when releasing.
#[derive(Debug, Deserialize)]
pub struct ReleaseRequest {
    /// Try to hand the slice to another node afterwards. On by default; the
    /// operator can turn it off to free the memory and leave the range down.
    #[serde(default = "default_true")]
    pub reassign: bool,
}

pub(super) fn default_true() -> bool {
    true
}

/// A node that could take over a slice: idle, and already holding the file.
pub(super) struct Candidate {
    pub(super) node_id: String,
    pub(super) hostname: String,
    pub(super) peer: crate::network::PeerInfo,
    /// Where that same slice lives on *that* peer's disk.
    pub(super) path: String,
}

/// Find a peer that is serving nothing and already has this exact slice.
///
/// "Already has it" is deliberate: the point of releasing is that this machine
/// needs its memory back *now*, so the handover has to be something another node
/// can act on immediately. Copying a 10 GB slice across the link first would take
/// ~17 minutes, which is not a handover — that is what the deploy screen is for.
///
/// Matching is by layer range plus shard group, not by file name: the same slice
/// legitimately sits under different paths on different machines.
pub(super) async fn find_release_candidate(
    state: &GatewayState,
    cfg: &NodeConfig,
) -> Option<Candidate> {
    let self_id = state.network.node_id.clone();
    for peer in state.all_peers().await {
        if peer.node_id == self_id {
            continue;
        }
        // Busy peers are out: a node serves one range at a time.
        match state.control_get(&peer, "/api/ui/node", PEER_TIMEOUT).await {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or(json!({}));
                if !body.get("config").map(|c| c.is_null()).unwrap_or(true) {
                    continue;
                }
            }
            _ => continue,
        }

        let Ok(r) = state
            .control_get(&peer, "/api/ui/models", PEER_TIMEOUT * 3)
            .await
        else {
            continue;
        };
        if !r.status().is_success() {
            continue;
        }
        let body: serde_json::Value = r.json().await.unwrap_or(json!({}));

        let found = body
            .get("shard_sets")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .flat_map(|set| {
                set.get("shards")
                    .and_then(|s| s.as_array())
                    .into_iter()
                    .flatten()
            })
            .find(|shard| {
                let range = shard.get("shard_range").and_then(|r| r.as_array());
                let matches_range = range
                    .map(|r| {
                        r.first().and_then(|v| v.as_u64()) == Some(cfg.start_layer as u64)
                            && r.get(1).and_then(|v| v.as_u64()) == Some(cfg.end_layer as u64)
                    })
                    .unwrap_or(false);
                matches_range
                    && shard
                        .get("total_layers")
                        .and_then(|v| v.as_u64())
                        .map(|t| t == cfg.total_layers as u64)
                        .unwrap_or(true)
            })
            .and_then(|shard| shard.get("path").and_then(|p| p.as_str()).map(String::from));

        if let Some(path) = found {
            return Some(Candidate {
                node_id: peer.node_id.clone(),
                hostname: peer.hostname.clone(),
                peer: peer.clone(),
                path,
            });
        }
    }
    None
}

/// Who would take this node's slice if it were released right now.
///
/// Read-only, and the reason the confirmation dialog can promise something true:
/// warning "the model will stop answering" only when it actually will beats a
/// blanket scare that the operator learns to click through.
pub(super) async fn release_candidate(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let Some(cfg) = state.runtime.config().await else {
        return Json(json!({ "serving": false })).into_response();
    };
    let candidate = find_release_candidate(&state, &cfg).await;
    Json(json!({
        "serving": true,
        "slice": {
            "model": cfg.model_label,
            "start_layer": cfg.start_layer,
            "end_layer": cfg.end_layer,
            "total_layers": cfg.total_layers,
        },
        "candidate": candidate.map(|c| json!({
            "node_id": c.node_id,
            "hostname": c.hostname,
        })),
    }))
    .into_response()
}

/// Hand this node's slice back and free its RAM/VRAM, staying in the cluster.
///
/// The release is **never refused**, even when nobody can take the slice over.
/// That is the whole point of the feature: the machine needs its memory now, and
/// a node that has to ask permission to stop has not been given the freedom the
/// operator was promised. What the caller gets instead is a straight answer about
/// what the cluster looks like afterwards — `reassigned_to` when another node
/// picked the range up, or `uncovered: true` naming the range that is now down.
///
/// An uncovered range is not silent anywhere else either: the planner already
/// refuses to build a pipeline with a hole in it, so generation fails with the
/// gap named rather than falling back to a wrong answer.
pub(super) async fn release_slice(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    body: Option<Json<ReleaseRequest>>,
) -> Response {
    let Some(cfg) = state.runtime.config().await else {
        return bad_request("this node is not serving any slice");
    };
    let reassign = body.map(|Json(r)| r.reassign).unwrap_or(true);

    // Free the memory first, and unconditionally. Looking for a successor before
    // stopping would make the operator wait on the network for the one thing
    // they asked to happen immediately.
    state.runtime.stop().await;
    info!(
        "slice L{}-{} released by the node; memory returned",
        cfg.start_layer, cfg.end_layer
    );

    let released = json!({
        "model": cfg.model_label,
        "start_layer": cfg.start_layer,
        "end_layer": cfg.end_layer,
        "total_layers": cfg.total_layers,
    });

    if !reassign {
        return Json(json!({
            "released": true, "slice": released,
            "reassigned_to": null, "uncovered": true,
            "reason": "reassignment turned off in the request",
        }))
        .into_response();
    }

    let Some(candidate) = find_release_candidate(&state, &cfg).await else {
        warn!(
            "slice L{}-{} was left uncovered: no idle node has this file",
            cfg.start_layer, cfg.end_layer
        );
        return Json(json!({
            "released": true, "slice": released,
            "reassigned_to": null, "uncovered": true,
            "reason": "no other node is idle and has this slice on disk",
        }))
        .into_response();
    };

    // Same shape a launch dispatches, so the successor comes up exactly as this
    // node was: same label, same position in the split, same runtime params.
    let apply_req = ApplyRequest {
        // The successor enters the **same** deployment: it is the same range of the same
        // set, only on another machine.
        deploy_id: cfg.deploy_id.clone(),
        source_model_path: candidate.path.clone(),
        // The successor loads exactly the file this node was loading, and the
        // name of a part is already its hash: that is where the identity comes from.
        part_sha256: std::path::Path::new(&candidate.path)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| crate::pom::is_sha256_hex(s))
            .map(|s| s.to_string()),
        base_name: cfg.model_label.clone(),
        model_label: Some(cfg.model_label.clone()),
        deployment_name: cfg.deployment_name.clone(),
        start_layer: cfg.start_layer,
        end_layer: cfg.end_layer,
        total_layers: cfg.total_layers,
        peer_count: cfg.peer_count,
        peer_index: cfg.peer_index,
        primary: cfg.primary,
        tp: cfg.tp.clone(),
        skip_split: true,
        use_mmproj: cfg.mmproj_path.is_some(),
        mmproj_path: cfg.mmproj_path.clone(),
        mtp_draft_path: None,
        mtp_draft_file: None,
        mmproj_file: None,
        merge: None,
        params: RuntimeParams::from(&cfg),
    };

    match state
        .control_post_json(
            &candidate.peer,
            "/api/ui/apply",
            &apply_req,
            APPLY_FORWARD_TIMEOUT,
        )
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!(
                "slice L{}-{} taken over by {}",
                cfg.start_layer, cfg.end_layer, candidate.node_id
            );
            Json(json!({
                "released": true, "slice": released, "uncovered": false,
                "reassigned_to": {
                    "node_id": candidate.node_id,
                    "hostname": candidate.hostname,
                },
            }))
            .into_response()
        }
        Ok(r) => {
            let code = r.status();
            let text = r.text().await.unwrap_or_default();
            warn!(
                "{} refused to take over the slice: HTTP {code}",
                candidate.node_id
            );
            Json(json!({
                "released": true, "slice": released,
                "reassigned_to": null, "uncovered": true,
                "reason": format!("{} refused to take over: HTTP {code} {text}", candidate.hostname),
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "released": true, "slice": released,
            "reassigned_to": null, "uncovered": true,
            "reason": format!("{} unreachable: {e}", candidate.hostname),
        }))
        .into_response(),
    }
}

/// Announce again and go looking for the bootstrap nodes.
pub(super) async fn rejoin_network(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
) -> Response {
    let assignment = state.local_assignment.read().await.clone();
    match state.network.rejoin(assignment.as_ref()).await {
        Ok(()) => Json(json!({ "detached": false })).into_response(),
        Err(e) => bad_request(format!("could not return to the network: {e}")),
    }
}

/// One local slice feeding a merge on this peer: the file and the range it
/// stands for. The peer joins these into one GGUF before serving it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeSource {
    /// Path of the slice *on this peer*.
    pub source_model_path: String,
    pub start_layer: u32,
    pub end_layer: u32,
}

/// One entry per peer taking part in the launch.
#[derive(Debug, Deserialize)]
pub struct LaunchTarget {
    pub node_id: String,
    /// Path of the source model *on that peer*.
    pub source_model_path: String,
    pub start_layer: u32,
    pub end_layer: u32,
    #[serde(default)]
    pub skip_split: bool,
    /// Hash of the part this target loads, when the `.pom` already knows it. It is what
    /// spares the destination from guessing a file name.
    #[serde(default)]
    pub part_sha256: Option<String>,
    /// Local slices to join into one file before serving. When present, the
    /// peer merges them (`splitter::merge`) and serves the union; the merge
    /// replaces `skip_split` because the file covering the range only comes
    /// into existence through the merge.
    #[serde(default)]
    pub merge: Option<Vec<MergeSource>>,
    /// How many of this peer's layers go on its GPU, overriding
    /// `params.n_gpu_layers` for this peer alone.
    ///
    /// The only knob that is honestly per-machine: the rest of `RuntimeParams`
    /// is either protocol (context window, batch, chunk) or sampling, and both
    /// have to agree across the pipeline. Offload does not — a box with a 5060
    /// Ti and a CPU-only box in the same cluster want different numbers, and
    /// before this the whole launch had to settle on one.
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    /// Where this peer's KV cache lives, overriding `params.offload_kqv` for
    /// it alone.
    ///
    /// For the same reason as the offload: the cache is sized by the context
    /// window, and a machine with 16 GB of VRAM and one without a GPU in the same
    /// pipeline cannot both be right about where it fits.
    #[serde(default)]
    pub offload_kqv: Option<bool>,
    /// Per-target override, computed from the plan's topology. `None`
    /// preserves the final peer's automatic behaviour.
    #[serde(default)]
    pub no_embeddings: Option<bool>,
    /// Position of this peer in the pipeline, when it is not its position in this
    /// request.
    ///
    /// `launch` deduces `peer_index`/`peer_count` from the list of targets, which only
    /// holds when the request describes the whole pipeline. Whoever adds one
    /// machine at a time (`deploy::add_node_endpoint`) sends one target and
    /// needs to state the real position - without this the added node declares itself the last
    /// of the split, comes up without residual capture and inference dies on the
    /// first token with "no embeddings".
    #[serde(default)]
    pub peer_index: Option<u32>,
    #[serde(default)]
    pub peer_count: Option<u32>,
    /// Tensor parallelism identity of this target (path C). When present
    /// (primary or worker of a group), the apply extracts the shard/registers the
    /// collective instead of only slicing by layers.
    #[serde(default)]
    pub tp: Option<crate::network::TpIdentity>,
    /// The target should load the adjacent multimodal projector.
    #[serde(default)]
    pub use_mmproj: bool,
    /// Projector path on the target peer, resolved from its shard metadata.
    #[serde(default)]
    pub mmproj_path: Option<String>,
    /// External MTP draft on the target peer (the pipeline tail), resolved by
    /// the deployment from the model's sidecar link. `None` keeps the peer's
    /// own lookup next to the loaded file.
    #[serde(default)]
    pub mtp_draft_path: Option<String>,
    /// The same draft by name + size, for the target to find on its own disk
    /// when the coordinator did not know the path (a file copied a moment ago).
    #[serde(default)]
    pub mtp_draft_file: Option<crate::catalog::SidecarFile>,
    /// The projector by name + size, under the same rule, for the head.
    #[serde(default)]
    pub mmproj_file: Option<crate::catalog::SidecarFile>,
}

#[derive(Debug, Deserialize)]
pub struct LaunchRequest {
    /// The deployment this launch is bringing up. Minted by
    /// `deploy::deploy_endpoint`; empty in a standalone launch, which did not go through
    /// any deployment.
    #[serde(default)]
    pub deploy_id: String,
    pub base_name: String,
    /// The identity selected in the deployment screen, propagated unchanged
    /// to every apply request. Missing values keep older callers compatible.
    #[serde(default)]
    pub model_label: Option<String>,
    /// Operational name shown in jobs, status and telemetry.
    #[serde(default)]
    pub deployment_name: String,
    pub total_layers: u32,
    pub targets: Vec<LaunchTarget>,
    /// Node that should act as the entry point.
    #[serde(default)]
    pub primary_node_id: Option<String>,
    #[serde(default)]
    pub params: RuntimeParams,
    #[serde(default)]
    pub use_mmproj: bool,
}

/// The cluster-wide settings as one peer will actually receive them.
///
/// Everything in `RuntimeParams` has to agree across the pipeline — the context
/// window and the batch decide the prompt budget the gateway enforces, and the
/// sampling is only used by the last peer — except how much of its own share a
/// machine offloads to its GPU. That one is a property of the machine.
pub(crate) fn params_for(base: &RuntimeParams, target: &LaunchTarget) -> RuntimeParams {
    let mut params = base.clone();
    if let Some(gpu) = target.n_gpu_layers {
        params.n_gpu_layers = gpu;
    }
    if let Some(kqv) = target.offload_kqv {
        params.offload_kqv = kqv;
    }
    if let Some(no_embeddings) = target.no_embeddings {
        params.no_embeddings = no_embeddings;
    }
    params
}

pub(super) fn launch_no_embeddings_evaluation(
    req: &LaunchRequest,
) -> crate::capabilities::NoEmbeddingsEvaluation {
    let mut ranges = Vec::new();
    for target in &req.targets {
        if target.tp.is_none() {
            let range = (target.start_layer, target.end_layer);
            if !ranges.contains(&range) {
                ranges.push(range);
            }
        }
    }
    let pipeline_count = ranges.len() as u32;
    let peers = req
        .targets
        .iter()
        .map(|target| {
            let derived_index = if target.tp.is_some() {
                Some(0)
            } else {
                ranges
                    .iter()
                    .position(|range| *range == (target.start_layer, target.end_layer))
                    .map(|value| value as u32)
            };
            crate::capabilities::PeerTopology {
                node_id: target.node_id.clone(),
                start_layer: Some(target.start_layer),
                end_layer: Some(target.end_layer),
                total_layers: Some(req.total_layers),
                peer_index: target.peer_index.or(derived_index),
                peer_count: target.peer_count.or_else(|| {
                    if target.tp.is_some() {
                        Some(1)
                    } else {
                        Some(pipeline_count)
                    }
                }),
            }
        })
        .collect::<Vec<_>>();
    crate::capabilities::evaluate_no_embeddings(&peers)
}

/// Dispatch the planned configuration to every peer, including this one.
pub(crate) async fn launch(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<LaunchRequest>,
) -> Response {
    if req.targets.is_empty() {
        return bad_request("no peer selected");
    }
    // Checked before phase 1, which stops inference on every target: a launch
    // that cannot possibly apply must not be allowed to take the cluster down
    // on its way to failing. `deploy.rs` refuses this earlier and with the
    // offending files named; this is the guard for every other caller.
    if req.total_layers == 0 {
        return bad_request(
            "the number of layers of the model was not provided, and the slices cannot be assigned without this total. Slices generated by the Slice screen write this; the old ones do not, and in that case the total is deduced from the coverage of the slices.",
        );
    }
    if req.use_mmproj && !req.targets.iter().any(|target| target.use_mmproj) {
        return bad_request("this model has no vision: no target received an mmproj");
    }
    let deployment = match normalize_deployment_name(Some(&req.deployment_name), &req.base_name) {
        Ok(name) => name,
        Err(error) => return bad_request(error),
    };
    if let Some(t) = req.targets.iter().find(|t| t.end_layer >= req.total_layers) {
        return bad_request(format!(
            "slice L{}-{} goes past the end of a model with {} layers",
            t.start_layer, t.end_layer, req.total_layers
        ));
    }

    if req.params.no_embeddings {
        if let Err(reason) = crate::capabilities::validate_no_embeddings(
            &launch_no_embeddings_evaluation(&req),
            true,
        ) {
            return bad_request(reason);
        }
    }

    let peers = state.all_peers().await;
    let self_id = state.network.node_id.clone();
    let peer_count = req.targets.len() as u32;
    // Refused here too, before anything is stopped: a target list built by
    // hand must not get as far as unloading the other machines.
    if let Some(infer) = req.targets.iter().find_map(|target| {
        peers
            .iter()
            .find(|peer| peer.node_id == target.node_id)
            .filter(|peer| crate::gateway::peer_is_infer(&state, peer))
    }) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "reason": "infer_target",
                "error": format!(
                    "{} was admitted for inference only and cannot serve a model",
                    infer.hostname
                ),
            })),
        )
            .into_response();
    }

    // Phase 1: stop inference on every target peer so no stale endpoint
    // receives inference requests mid-transition, and to free ports before
    // the new inference servers try to bind.
    for target in &req.targets {
        if target.node_id == self_id {
            state.runtime.stop().await;
            continue;
        }
        let Some(peer) = peers.iter().find(|p| p.node_id == target.node_id) else {
            warn!("peer {} not found for stop-inference", target.node_id);
            continue;
        };
        match state
            .control_post_empty(peer, "/api/ui/stop-inference", PEER_STOP_TIMEOUT)
            .await
        {
            Ok(r) if r.status().is_success() => {
                info!("peer {} stopped inference", target.node_id);
            }
            Ok(r) => {
                warn!(
                    "peer {} refused stop-inference: HTTP {}",
                    target.node_id,
                    r.status()
                );
            }
            Err(e) => {
                warn!(
                    "peer {} unreachable for stop-inference: {e}",
                    target.node_id
                );
            }
        }
    }

    // Brief settle: let the OS release ports.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Phase 2: dispatch apply with the new configuration.
    let mut results = Vec::new();

    for (index, target) in req.targets.iter().enumerate() {
        let params = params_for(&req.params, target);
        let apply_req = ApplyRequest {
            source_model_path: target.source_model_path.clone(),
            base_name: req.base_name.clone(),
            model_label: req.model_label.clone(),
            deployment_name: deployment.clone(),
            start_layer: target.start_layer,
            end_layer: target.end_layer,
            total_layers: req.total_layers,
            peer_count: target.peer_count.unwrap_or(peer_count),
            peer_index: target.peer_index.unwrap_or(index as u32),
            primary: req.primary_node_id.as_deref() == Some(target.node_id.as_str()),
            tp: target.tp.clone(),
            skip_split: target.skip_split,
            deploy_id: req.deploy_id.clone(),
            part_sha256: target.part_sha256.clone(),
            merge: target.merge.clone(),
            use_mmproj: req.use_mmproj && target.use_mmproj,
            mmproj_path: target.mmproj_path.clone(),
            mtp_draft_path: target.mtp_draft_path.clone(),
            mtp_draft_file: target.mtp_draft_file.clone(),
            mmproj_file: target.mmproj_file.clone(),
            params,
        };

        if target.node_id == self_id {
            let resp = apply(ConnectionAuth, State(state.clone()), Json(apply_req)).await;
            results.push(json!({
                "node_id": target.node_id,
                "dispatched": resp.status().is_success(),
                "local": true,
            }));
            continue;
        }

        let Some(peer) = peers.iter().find(|p| p.node_id == target.node_id) else {
            results.push(json!({
                "node_id": target.node_id,
                "dispatched": false,
                "error": "peer not found",
            }));
            continue;
        };

        // The peer's /api/ui/apply is authenticated: `control_post_json` carries the
        // node's signature and credential - without them the remote leg of a
        // launch always came back 401.
        match state
            .control_post_json(peer, "/api/ui/apply", &apply_req, APPLY_FORWARD_TIMEOUT)
            .await
        {
            Ok(r) if r.status().is_success() => {
                let body: serde_json::Value = r.json().await.unwrap_or(json!({}));
                results.push(json!({
                    "node_id": target.node_id,
                    "dispatched": true,
                    "job_id": body.get("job_id"),
                }));
            }
            Ok(r) => {
                let code = r.status();
                let body = r.text().await.unwrap_or_default();
                results.push(json!({
                    "node_id": target.node_id,
                    "dispatched": false,
                    "error": format!("HTTP {code}: {body}"),
                }));
            }
            Err(e) => results.push(json!({
                "node_id": target.node_id,
                "dispatched": false,
                "error": format!("{e:#}"),
            })),
        }
    }

    Json(json!({ "results": results })).into_response()
}
