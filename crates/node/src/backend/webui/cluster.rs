use super::*;
// ── Cluster view ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct PeerView {
    /// The plugin whose engine runs this machine's model, `None` for llama.cpp. A pipeline is built on one engine.
    pub(super) engine: Option<String>,
    /// That engine's part in a prefill/decode split (`prefill` or `decode`), `None` when it does both. The Flow draws
    /// a prefill machine as the head and an edge from it to the decode machines of the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) role: Option<&'static str>,
    pub(super) node_id: String,
    pub(super) short_id: String,
    pub(super) addr: String,
    pub(super) model: String,
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    pub(super) total_layers: u32,
    pub(super) capabilities: Vec<routing_contract::ModelCapability>,
    pub(super) inference_url: Option<String>,
    /// Address through which this node talks to the peer, and the latency it measured.
    pub(super) route: Option<RouteView>,
    pub(super) gateway_url: Option<String>,
    /// In-flight requests reported by this peer. A remote read that failed is
    /// `None`, not zero: zero is a real idle measurement.
    pub(super) load: Option<u32>,
    pub(super) is_self: bool,
    /// Present when the peer answered `/api/ui/node`.
    pub(super) node: Option<serde_json::Value>,
    /// Why the peer could not be reached, when it could not.
    pub(super) unreachable: Option<String>,
    /// Every known address of this peer is discredited and the node has already
    /// asked the network for its current address. It is not the same as
    /// `unreachable`: here there is work in progress, and the screen says
    /// "rediscovering" instead of announcing a dead end.
    pub(super) rediscovering: bool,
}

#[derive(Debug, Serialize)]
pub struct RouteView {
    pub addr: String,
    pub rtt_ms: f64,
}

/// Route chosen for this peer, or `None` when it has not been measured yet.
///
/// `None` is information: it means "I do not know yet", and the screen says so
/// instead of showing an address nobody confirmed.
pub fn route_view_for(node_id: &str, table: &crate::route::RouteTable) -> Option<RouteView> {
    table
        .snapshot()
        .into_iter()
        .find(|(id, _)| id == node_id)
        .map(|(_, choice)| RouteView {
            addr: choice.addr,
            rtt_ms: choice.rtt_ms,
        })
}

#[derive(Debug, Serialize)]
pub(super) struct RoutingEdgeView {
    pub(super) from_node_id: String,
    pub(super) to_node_id: String,
    pub(super) model_id: String,
    pub(super) from_end_layer: u32,
    pub(super) to_start_layer: u32,
}

/// All viable handoffs observed in the model inventory. These are candidates,
/// not routing commands: the planner remains the sole authority selecting the
/// live path and balancing load. A prefill machine of a plugin engine hands a
/// prompt's state to every decode (or both) machine of the same model and
/// layers: that is an edge too, from its last layer to the other's first.
pub(super) fn candidate_edges(views: &[PeerView]) -> Vec<RoutingEdgeView> {
    let mut edges = Vec::new();
    for from in views.iter().filter(|view| view.role == Some("prefill")) {
        for to in views.iter().filter(|view| {
            view.node_id != from.node_id && view.engine == from.engine && view.role != Some("prefill")
        }) {
            for from_capability in from.capabilities.iter().filter(|c| c.supports_inference) {
                if to.capabilities.iter().any(|c| {
                    c.supports_inference
                        && c.model_id == from_capability.model_id
                        && c.start_layer == from_capability.start_layer
                        && c.end_layer == from_capability.end_layer
                }) {
                    edges.push(RoutingEdgeView {
                        from_node_id: from.node_id.clone(),
                        to_node_id: to.node_id.clone(),
                        model_id: from_capability.model_id.clone(),
                        from_end_layer: from_capability.end_layer,
                        to_start_layer: from_capability.start_layer,
                    });
                }
            }
        }
    }
    for from in views {
        for from_capability in &from.capabilities {
            if !from_capability.supports_inference {
                continue;
            }
            for to in views {
                if to.node_id == from.node_id {
                    continue;
                }
                // a handoff is between two machines of one engine (the pipeline wire is that engine's own)
                if to.engine != from.engine {
                    continue;
                }
                for to_capability in &to.capabilities {
                    if !to_capability.supports_inference
                        || to_capability.model_id != from_capability.model_id
                        || to_capability.end_layer <= from_capability.end_layer
                        || to_capability.start_layer > from_capability.end_layer.saturating_add(1)
                    {
                        continue;
                    }
                    edges.push(RoutingEdgeView {
                        from_node_id: from.node_id.clone(),
                        to_node_id: to.node_id.clone(),
                        model_id: from_capability.model_id.clone(),
                        from_end_layer: from_capability.end_layer,
                        to_start_layer: to_capability.start_layer,
                    });
                }
            }
        }
    }
    edges.sort_by(|left, right| {
        (
            &left.model_id,
            &left.from_node_id,
            &left.to_node_id,
            left.from_end_layer,
            left.to_start_layer,
        )
            .cmp(&(
                &right.model_id,
                &right.from_node_id,
                &right.to_node_id,
                right.from_end_layer,
                right.to_start_layer,
            ))
    });
    edges.dedup_by(|left, right| {
        left.model_id == right.model_id
            && left.from_node_id == right.from_node_id
            && left.to_node_id == right.to_node_id
            && left.from_end_layer == right.from_end_layer
            && left.to_start_layer == right.to_start_layer
    });
    edges
}

/// Coverage is calculated per advertised model, not just the model currently
/// selected for generation. The dashboard may observe every model without
/// affecting the routing decision.
#[derive(Debug, Serialize)]
pub(super) struct ModelCoverageView {
    pub(super) model_id: String,
    pub(super) total_layers: u32,
    pub(super) covered_layers: usize,
    pub(super) coverage: Vec<bool>,
    pub(super) complete: bool,
    /// Machines that contribute layers to this coverage and **could not be
    /// read** in this pass.
    ///
    /// Coverage comes from the capabilities announced in the gossip, not from
    /// the read. Without this list, an unreadable peer is a full-strength
    /// coverage contributor and the screen shows "complete" for a model half of
    /// which nobody can query. It does not remove the layers from the count -
    /// removing them would be lying in the other direction, because the peer
    /// may be serving just fine and only the control address died - but it
    /// forces the screen to say that the coverage is not confirmed.
    pub(super) unreadable_nodes: Vec<String>,
}

/// What a read of `/api/ui/node` produced for a peer.
#[derive(Default)]
pub(super) struct PeerRead {
    pub(super) node: Option<serde_json::Value>,
    pub(super) unreachable: Option<String>,
    pub(super) rediscovering: bool,
}

/// No known address of the peer answers.
///
/// While the search for the current address is recent, this is "rediscovering" -
/// work in progress that the next gossip round usually resolves. Once
/// `REDISCOVERY_GIVE_UP` has passed, the network has been asked enough times:
/// saying "rediscovering" again would be the same dishonest notice as before
/// in another word, so the screen goes back to saying it cannot read the peer.
pub(super) fn without_live_address(
    network: &crate::network::Network,
    node_id: &str,
    known: usize,
) -> PeerRead {
    if network.is_rediscovering(node_id) {
        return PeerRead::rediscovering();
    }
    PeerRead::failed(format!(
        "none of the {known} known addresses answered, and the network did not return a new one"
    ))
}

impl PeerRead {
    pub(super) fn ok(node: serde_json::Value) -> Self {
        Self {
            node: Some(node),
            ..Self::default()
        }
    }

    pub(super) fn failed(error: String) -> Self {
        Self {
            unreachable: Some(error),
            ..Self::default()
        }
    }

    pub(super) fn rediscovering() -> Self {
        Self {
            rediscovering: true,
            ..Self::default()
        }
    }
}

/// Extract the peer's own load reading from `/api/ui/node`.
///
/// `GatewayState.loads` only knows requests travelling through the local
/// gateway. Using it for a remote peer made every remote card say zero even
/// while that peer was serving a request. Missing or malformed remote data is
/// deliberately kept unknown.
fn reported_peer_load(node: &serde_json::Value) -> Option<u32> {
    node.get("load")
        .and_then(serde_json::Value::as_u64)
        .and_then(|load| u32::try_from(load).ok())
}

/// The peers this node reaches only through a relay right now.
///
/// The relay first coordinates a hole punch; when it does not close, the
/// relay carries the traffic itself (`Network::peer_route`, kind `relayed`).
/// That fallback is what costs latency and, on a public relay, bandwidth, and
/// it is the only case the interface warns about: being behind a NAT whose
/// hole punch worked is not.
pub(super) fn relayed_peers(state: &GatewayState, peers: &[PeerInfo]) -> Vec<serde_json::Value> {
    let mut relayed: Vec<serde_json::Value> = peers
        .iter()
        .filter(|peer| peer.node_id != state.network.node_id)
        .filter_map(|peer| {
            let route = state.network.peer_route(peer);
            (route.kind == "relayed").then(|| {
                json!({
                    "node_id": peer.node_id,
                    "hostname": peer.hostname,
                    "relay_kind": route.relay_kind,
                })
            })
        })
        .collect();
    relayed.sort_by(|a, b| a["node_id"].as_str().cmp(&b["node_id"].as_str()));
    relayed
}

pub(super) async fn cluster_fingerprint(state: &GatewayState, peers: &[PeerInfo]) -> String {
    let mut peer_state = serde_json::to_value(peers).expect("UI peers must serialize");
    if let Some(peers) = peer_state.as_array_mut() {
        for peer in peers {
            // Liveness is represented by presence in this list. The timestamp
            // itself is not rendered and would invalidate the cache on every
            // heartbeat, including the synthetic self peer.
            if let Some(peer) = peer.as_object_mut() {
                peer.remove("last_seen");
            }
        }
    }
    let mut loads = Vec::with_capacity(peers.len());
    for peer in peers {
        loads.push(json!({
            "node_id": peer.node_id,
            "load": if peer.node_id == state.network.node_id {
                local_inference_load(state).await
            } else {
                state.loads.load(&peer.node_id).await
            },
        }));
    }
    let mut routes = state
        .network
        .routes
        .snapshot()
        .into_iter()
        .map(|(node_id, route)| {
            json!({
                "node_id": node_id,
                "addr": route.addr,
                "rtt_ms": route.rtt_ms,
            })
        })
        .collect::<Vec<_>>();
    routes.sort_by(|left, right| left["node_id"].as_str().cmp(&right["node_id"].as_str()));

    etag_for_json(&json!({
        "self_id": state.network.node_id,
        "peers": peer_state,
        "loads": loads,
        "routes": routes,
        "runtime": state.runtime.status().await,
        "config": state.runtime.config().await,
        "active_model": state.displayed_model().await,
        "detached": state.network.is_detached(),
        // Without this the traversal snapshot would freeze in the cache: neither
        // the NAT report nor a change of relay touches `peers`.
        "traversal": state.network.traversal_status(),
        // A peer moving between a hole punched path and the relay fallback
        // touches neither.
        "relayed_peers": relayed_peers(state, peers),
        // The pinned route of each edge changes without touching `peers`.
        "route_plan": crate::route_plan::book().signature(),
    }))
}

/// Sampling time is not a UI fact. Keep the actual metrics in the version, but
/// remove only this timestamp so a fresh sample with identical values does not
/// invalidate the semantic cluster version.
pub(super) fn cluster_etag(body: &serde_json::Value) -> String {
    let mut normalized = body.clone();
    if let Some(node) = normalized.get_mut("node") {
        remove_system_sample_time(node);
    }
    if let Some(peers) = normalized
        .get_mut("peers")
        .and_then(|value| value.as_array_mut())
    {
        for peer in peers {
            if let Some(node) = peer.get_mut("node") {
                remove_system_sample_time(node);
            }
        }
    }
    etag_for_json(&normalized)
}

pub(super) fn remove_system_sample_time(node: &mut serde_json::Value) {
    if let Some(system) = node
        .get_mut("system")
        .and_then(|value| value.as_object_mut())
    {
        system.remove("sampled_at");
    }
}

pub(super) async fn cluster(headers: HeaderMap, State(state): State<GatewayState>) -> Response {
    let profile = state.auth.read().await.profile;
    if !profile.allows(crate::profile::ProfileAction::ViewNetwork) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "this profile cannot open the network" })),
        )
            .into_response();
    }
    let self_id = state.network.node_id.clone();
    let peers = state
        .all_peers()
        .await
        .into_iter()
        .filter(|peer| {
            profile.allows(crate::profile::ProfileAction::ViewNetworkPeers)
                || peer.node_id == self_id
        })
        .collect::<Vec<_>>();
    let relayed = relayed_peers(&state, &peers);
    let runtime_ready_by_node: HashMap<_, _> = peers
        .iter()
        .map(|peer| {
            (
                peer.node_id.clone(),
                peer.runtime_capacity.is_some() || peer.idle,
            )
        })
        .collect();
    let fingerprint = cluster_fingerprint(&state, &peers).await;
    let mut cache = state.cluster_cache.lock().await;
    if let Some(snapshot) = cache.get(&fingerprint, Instant::now(), CLUSTER_CACHE_TTL) {
        return conditional_json_with_version(
            &headers,
            &snapshot.body,
            &snapshot.etag,
            snapshot.version,
        );
    }
    // Fan out to every peer at once; a slow peer must not serialise the others.
    //
    // Each peer is read through its **candidates**, not through a single
    // address, and the result feeds the `RouteTable` failure memory - the same
    // one as `mark_failure`/`is_failed` of the inference path. That is what
    // keeps the Dashboard from redialing forever an address that died: one that
    // fails goes out of use and is re-evaluated after `FAILURE_COOLDOWN`; one
    // that answers clears the mark at once and becomes the next read's favorite.
    let peer_state = state.clone();
    let futures: Vec<_> = peers
        .iter()
        .map(|p| {
            let is_self = p.node_id == self_id;
            let node_id = p.node_id.clone();
            let candidates = p.gateway_candidates();
            // Client pinned to the destination's node_id: with the cluster on TLS,
            // the shared one has no way to verify the peer's certificate and every
            // control call would fail - that is how the deploy screen stopped
            // seeing the other nodes' slices.
            let http = state.peer_http.for_peer(&p.node_id);
            let state = peer_state.clone();
            let routes = state.network.routes.clone();
            let network = state.network.clone();
            let peer = p.clone();
            async move {
                if is_self {
                    return PeerRead::default();
                }
                if candidates.is_empty() {
                    return match read_node_via_tunnel(&state, &peer).await {
                        Some(v) => PeerRead::ok(v),
                        None => PeerRead::default(),
                    };
                }
                let http = match http {
                    Ok(c) => c,
                    Err(e) => return PeerRead::failed(format!("no client: {e}")),
                };
                let usable = routes.usable(&node_id, &candidates);
                if usable.is_empty() {
                    // Every known address is discredited. Insisting does not
                    // revive any of them: what solves it is the network saying
                    // again where the peer is - or the tunnel, for a peer that
                    // no address reaches.
                    if let Some(v) = read_node_via_tunnel(&state, &peer).await {
                        network.note_peer_read(&node_id);
                        return PeerRead::ok(v);
                    }
                    network.request_peer_addresses(&node_id).await;
                    return without_live_address(&network, &node_id, candidates.len());
                }
                let mut last_error = None;
                for base in &usable {
                    let url = format!("{base}/api/ui/node");
                    let request = match state
                        .control_get_request(http.get(&url), "/api/ui/node")
                        .await
                    {
                        Ok(request) => request,
                        Err(error) => {
                            last_error = Some(format!("control signature unavailable: {error}"));
                            continue;
                        }
                    };
                    let response = tokio::time::timeout(PEER_TIMEOUT, request.send()).await;
                    match response {
                        Ok(Ok(r)) if r.status().is_success() => match r.json().await {
                            Ok(v) => {
                                routes.mark_success(&node_id, base);
                                network.note_peer_read(&node_id);
                                return PeerRead::ok(v);
                            }
                            // An invalid response is not a dead address: the peer
                            // answered. Discrediting the address here would hide
                            // a payload defect behind a route search.
                            Err(e) => last_error = Some(format!("invalid response: {e}")),
                        },
                        // The same holds for an HTTP error: whoever answers
                        // 401 or 500 is alive at that address.
                        Ok(Ok(r)) => last_error = Some(format!("HTTP {}", r.status())),
                        Ok(Err(e)) => {
                            routes.mark_failure(&node_id, base);
                            last_error = Some(format!("{e}"));
                        }
                        Err(_) => {
                            routes.mark_failure(&node_id, base);
                            last_error = Some("timeout".to_string());
                        }
                    }
                }
                if let Some(v) = read_node_via_tunnel(&state, &peer).await {
                    network.note_peer_read(&node_id);
                    return PeerRead::ok(v);
                }
                // If the last attempt took down the last usable address, the
                // new address can already be requested in this same pass.
                if routes.usable(&node_id, &candidates).is_empty() {
                    network.request_peer_addresses(&node_id).await;
                    return without_live_address(&network, &node_id, candidates.len());
                }
                PeerRead::failed(last_error.unwrap_or_else(|| "no answer".to_string()))
            }
        })
        .collect();

    let fetched = futures::future::join_all(futures).await;

    let mut views = Vec::new();
    for (p, reading) in peers.iter().zip(fetched) {
        let PeerRead {
            node,
            unreachable,
            rediscovering,
        } = reading;
        let is_self = p.node_id == self_id;
        let node = if is_self {
            // Serialise our own view through the same shape as a remote one.
            serde_json::to_value(node_info(HeaderMap::new(), State(state.clone())).await.0).ok()
        } else {
            node
        };
        let load = if is_self {
            Some(local_inference_load(&state).await)
        } else {
            node.as_ref().and_then(reported_peer_load)
        };
        views.push(PeerView {
            engine: crate::plugin_engine::engine_of_peer(p),
            role: Some(crate::plugin_engine::role_of_capabilities(&p.inference_frame_capabilities))
                .filter(|role| !role.is_both() && crate::plugin_engine::engine_of_peer(p).is_some())
                .map(|role| role.as_str()),
            short_id: p.node_id[..p.node_id.len().min(12)].to_string(),
            node_id: p.node_id.clone(),
            addr: p.addr.clone(),
            model: p.model.clone(),
            start_layer: p.start_layer,
            end_layer: p.end_layer,
            total_layers: p.total_layers,
            capabilities: if p.capabilities.is_empty() && !p.model.is_empty() {
                vec![routing_contract::ModelCapability {
                    model_id: p.model.clone(),
                    start_layer: p.start_layer,
                    end_layer: p.end_layer,
                    total_layers: Some(p.total_layers),
                    supports_inference: p.inference_port.is_some(),
                }]
            } else {
                p.capabilities.clone()
            },
            inference_url: p.inference_url(),
            route: route_view_for(&p.node_id, &state.network.routes),
            gateway_url: p.gateway_url(),
            load,
            is_self,
            node,
            unreachable,
            rediscovering,
        });
    }

    views.sort_by_key(|v| (v.start_layer, v.short_id.clone()));

    // Layer coverage for every model in the inventory. Older peers do not
    // advertise `total_layers` per capability, so use their legacy peer total
    // when the model matches, otherwise infer the smallest possible total from
    // the greatest advertised layer.
    let mut model_ids: Vec<_> = views
        .iter()
        .flat_map(|view| {
            view.capabilities
                .iter()
                .map(|capability| capability.model_id.clone())
        })
        .collect();
    model_ids.sort();
    model_ids.dedup();
    let model_inventory: Vec<ModelCoverageView> = model_ids
        .into_iter()
        .map(|model_id| {
            let requested_model = &model_id;
            let capabilities: Vec<_> = views
                .iter()
                .flat_map(|view| {
                    view.capabilities
                        .iter()
                        .filter(move |capability| capability.model_id == *requested_model)
                        .map(move |capability| (view, capability))
                })
                .collect();
            let total_layers = capabilities
                .iter()
                .filter_map(|(view, capability)| {
                    capability.total_layers.or_else(|| {
                        (view.model == model_id && view.total_layers > 0)
                            .then_some(view.total_layers)
                    })
                })
                .max()
                .unwrap_or_else(|| {
                    capabilities
                        .iter()
                        .map(|(_, capability)| capability.end_layer + 1)
                        .max()
                        .unwrap_or(0)
                });
            let mut coverage = vec![false; total_layers as usize];
            for (_, capability) in capabilities {
                for layer in capability.start_layer
                    ..=capability.end_layer.min(total_layers.saturating_sub(1))
                {
                    if let Some(slot) = coverage.get_mut(layer as usize) {
                        *slot = true;
                    }
                }
            }
            let covered_layers = coverage.iter().filter(|covered| **covered).count();
            let runtime_ready = views
                .iter()
                .filter(|view| {
                    view.capabilities
                        .iter()
                        .any(|capability| capability.model_id == model_id)
                })
                .all(|view| {
                    runtime_ready_by_node
                        .get(&view.node_id)
                        .copied()
                        .unwrap_or(false)
                });
            let mut unreadable_nodes: Vec<String> = views
                .iter()
                .filter(|view| !view.is_self && (view.unreachable.is_some() || view.rediscovering))
                .filter(|view| {
                    view.capabilities
                        .iter()
                        .any(|capability| capability.model_id == model_id)
                })
                .map(|view| view.node_id.clone())
                .collect();
            unreadable_nodes.sort();
            unreadable_nodes.dedup();
            ModelCoverageView {
                model_id,
                total_layers,
                covered_layers,
                complete: total_layers > 0
                    && covered_layers == total_layers as usize
                    && runtime_ready
                    && unreadable_nodes.is_empty(),
                coverage,
                unreadable_nodes,
            }
        })
        .collect();

    // Preserve the legacy active-model fields for existing UI consumers.
    let active = state.displayed_model().await;
    let active_mmproj_path = views.iter().find_map(|view| {
        let node = view.node.as_ref()?;
        let config = node.get("config")?;
        if config
            .get("start_layer")
            .and_then(serde_json::Value::as_u64)
            != Some(0)
        {
            return None;
        }
        let serves_active_model = view.model == active
            || view
                .capabilities
                .iter()
                .any(|capability| capability.model_id == active)
            || node.get("active_model").and_then(serde_json::Value::as_str)
                == Some(active.as_str())
            || config
                .get("model_label")
                .and_then(serde_json::Value::as_str)
                == Some(active.as_str());
        if !serves_active_model {
            return None;
        }
        config
            .get("mmproj_path")
            .and_then(serde_json::Value::as_str)
            .filter(|path| !path.is_empty())
            .map(str::to_string)
    });
    let active_coverage = model_inventory
        .iter()
        .find(|inventory| inventory.model_id == active);
    let total = active_coverage
        .map(|inventory| inventory.total_layers)
        .unwrap_or(0);
    let covered_count = active_coverage
        .map(|inventory| inventory.covered_layers)
        .unwrap_or(0);
    let covered = active_coverage
        .map(|inventory| inventory.coverage.clone())
        .unwrap_or_default();

    // Ask the same planner used by generation which handoffs it selected.
    // This is observability only: the UI cannot influence routing or load
    // balancing through this endpoint.
    let routing_edges = if active.is_empty() || total == 0 {
        Vec::new()
    } else {
        let mut planned = None;
        for engine in crate::plugin_engine::pipeline_engines(&peers, None) {
            let group = crate::plugin_engine::peers_of_engine(&peers, engine.as_deref());
            if let Some(plan) = crate::pipeline::plan_pipeline(&group, &active, None, total, &state.loads).await {
                planned = Some(plan);
                break;
            }
        }
        planned
            .map(|plan| {
                plan.edges
                    .into_iter()
                    .map(|edge| RoutingEdgeView {
                        from_node_id: edge.from_node_id,
                        to_node_id: edge.to_node_id,
                        model_id: active.clone(),
                        from_end_layer: edge.from_end_layer,
                        to_start_layer: edge.to_start_layer,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let candidate_edges = candidate_edges(&views);

    let self_node = serde_json::to_value(node_info(HeaderMap::new(), State(state.clone())).await.0).ok();

    let body = json!({
        "self_id": self_id,
        "active_model": active,
        "total_layers": total,
        "covered_layers": covered_count,
        "coverage": covered,
        "complete": active_coverage.is_some_and(|inventory| inventory.complete),
        "mmproj_path": active_mmproj_path,
        "model_inventory": model_inventory,
        "peers": views,
        "routing_edges": routing_edges,
        "candidate_edges": candidate_edges,
        "node": self_node,
        // Diagnostics of this node's NAT traversal. See `TraversalStatus`:
        // without this, "the two nodes do not talk to each other" can only be
        // investigated with `RUST_LOG=debug` on the right machine.
        "traversal": state.network.traversal_status(),
        // Peers reached only through a relay: the hole punch did not close
        // and the relay carries the traffic (`relayed_peers`).
        "relayed_peers": relayed,
        // Pinned route per outgoing edge of this node (`route_plan`): over which
        // transport inference and the return to the gateway go right now, and
        // the alternatives already measured.
        "route_plan": crate::route_plan::book().snapshot(),
    });

    let etag = cluster_etag(&body);
    let snapshot = cache.store(fingerprint, body, etag, Instant::now());
    conditional_json_with_version(&headers, &snapshot.body, &snapshot.etag, snapshot.version)
}

/// The Chat needs only aggregate facts about the active pipeline. It must not
/// read `/api/ui/cluster`: infer is intentionally local-only there, and a
/// denied or reduced topology must never become an empty model or incomplete
/// coverage in the composer.
#[derive(Debug, Default, Deserialize)]
pub struct ChatCapabilitiesQuery {
    /// The model chosen in the Chat; a `@<deploy_id>` suffix is ignored.
    #[serde(default)]
    pub model: String,
}

/// What the Chat can do with a model: vision, thinking and layer coverage.
/// It answers for the model the Chat chose, or, without one, for the model
/// the network serves. Reading the model this node loads was wrong on a node
/// that only coordinates: it loads nothing, so a model served elsewhere came
/// back with `coverage: unavailable`.
pub(super) async fn chat_capabilities(
    State(state): State<GatewayState>,
    Query(query): Query<ChatCapabilitiesQuery>,
) -> impl IntoResponse {
    Json(chat_capabilities_value(&state, &query.model, None, false).await)
}

/// The OpenAI model catalog uses the same head/projector check as the Chat.
/// An explicit deployment alias must not inherit another deployment's vision.
pub(crate) async fn model_vision_supported(state: &GatewayState, model: &str) -> bool {
    let (_, deploy_id) = crate::deployment::split_selector(model);
    chat_capabilities_value(state, model, deploy_id, true)
        .await
        .get("vision")
        .and_then(serde_json::Value::as_str)
        == Some("supported")
}

async fn chat_capabilities_value(
    state: &GatewayState,
    model: &str,
    deploy_id: Option<&str>,
    vision_only: bool,
) -> serde_json::Value {
    let requested = crate::deployment::split_selector(model.trim()).0.to_string();
    let active_model = if requested.is_empty() {
        state.displayed_model().await
    } else {
        requested
    };
    let peers = state.all_peers().await;
    let local_id = state.network.node_id.clone();
    let local_config = state.runtime.config().await;
    let matching: Vec<_> = peers
        .iter()
        .filter(|peer| {
            peer.is_alive()
                && (peer.runtime_capacity.is_some() || peer.idle)
                && deploy_id.is_none_or(|id| peer.deploy_id == id)
                && ((peer.node_id == local_id
                    && local_config.as_ref().is_some_and(|config| {
                        config.start_layer == 0 && config.model_label == active_model
                    }))
                    || peer.model == active_model
                    || peer
                        .capabilities
                        .iter()
                        .any(|capability| capability.model_id == active_model))
        })
        .collect();

    let total_layers = matching
        .iter()
        .flat_map(|peer| {
            peer.capabilities
                .iter()
                .filter(|capability| capability.model_id == active_model)
                .filter_map(|capability| capability.total_layers)
                .chain((peer.total_layers > 0).then_some(peer.total_layers))
        })
        .max()
        .unwrap_or_else(|| {
            matching
                .iter()
                .map(|peer| peer.end_layer.saturating_add(1))
                .max()
                .unwrap_or(0)
        });
    let total_layers = local_config
        .as_ref()
        .filter(|config| config.model_label == active_model)
        .map(|config| config.total_layers)
        .into_iter()
        .chain(std::iter::once(total_layers))
        .max()
        .unwrap_or(0);
    let mut coverage = vec![false; total_layers as usize];
    for peer in &matching {
        let ranges = peer
            .capabilities
            .iter()
            .filter(|capability| capability.model_id == active_model)
            .map(|capability| (capability.start_layer, capability.end_layer));
        let ranges = if peer.capabilities.is_empty() && peer.model == active_model {
            Box::new(std::iter::once((peer.start_layer, peer.end_layer)))
                as Box<dyn Iterator<Item = (u32, u32)>>
        } else {
            Box::new(ranges)
        };
        for (start, end) in ranges {
            for layer in start..=end.min(total_layers.saturating_sub(1)) {
                if let Some(slot) = coverage.get_mut(layer as usize) {
                    *slot = true;
                }
            }
        }
    }
    if let Some(config) = local_config
        .as_ref()
        .filter(|config| config.model_label == active_model && config.start_layer == 0)
    {
        for layer in config.start_layer..=config.end_layer.min(total_layers.saturating_sub(1)) {
            if let Some(slot) = coverage.get_mut(layer as usize) {
                *slot = true;
            }
        }
    }
    let coverage_state = if total_layers == 0 || matching.is_empty() {
        "unavailable"
    } else if coverage.iter().all(|covered| *covered) {
        "complete"
    } else {
        "incomplete"
    };

    let head_candidates: Vec<_> = matching
        .iter()
        .filter(|peer| peer.start_layer == 0)
        .collect();
    let mut vision_supported = false;
    let mut vision_unknown = false;
    let mut vision_candidate = false;
    let mut thinking_enabled = false;
    let mut thinking_disabled = false;
    let mut thinking_unknown = false;

    for peer in head_candidates {
        vision_candidate = true;
        let config = if peer.node_id == local_id {
            local_config.as_ref().map(|config| {
                json!({
                    "start_layer": config.start_layer,
                    "model_label": config.model_label,
                    "mmproj_path": config.mmproj_path,
                    "no_thinking": config.no_thinking,
                    "inference_service": config.inference_service,
                })
            })
        } else {
            let Ok(http) = state.peer_http.for_peer(&peer.node_id) else {
                vision_unknown = true;
                continue;
            };
            let candidates = state
                .network
                .routes
                .usable(&peer.node_id, &peer.gateway_candidates());
            let mut result = None;
            for base in candidates {
                let Ok(request) = state
                    .control_get_request(http.get(format!("{base}/api/ui/node")), "/api/ui/node")
                    .await
                else {
                    continue;
                };
                let response = tokio::time::timeout(PEER_TIMEOUT, request.send()).await;
                if let Ok(Ok(response)) = response {
                    if response.status().is_success() {
                        if let Ok(value) = response.json::<serde_json::Value>().await {
                            result = Some(value);
                            break;
                        }
                    }
                }
            }
            if result.is_none() {
                result = read_node_via_tunnel(&state, &peer).await;
            }
            result.and_then(|body| body.get("config").cloned())
        };

        let Some(config) = config else {
            vision_unknown = true;
            continue;
        };
        let serves_head = config
            .get("start_layer")
            .and_then(serde_json::Value::as_u64)
            == Some(0)
            && (config
                .get("model_label")
                .and_then(serde_json::Value::as_str)
                == Some(active_model.as_str())
                || config
                    .get("active_model")
                    .and_then(serde_json::Value::as_str)
                    == Some(active_model.as_str()));
        if !serves_head {
            vision_unknown = true;
            continue;
        }
        // a plugin engine turns the thinking on and off per request (the turn's `thinking`): `no_thinking` is the
        // llama.cpp runtime's switch, which a plugin deployment never sets
        let plugin_engine = config
            .get("inference_service")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|code| !code.is_empty());
        match config
            .get("no_thinking")
            .and_then(serde_json::Value::as_bool)
            .map(|off| off && !plugin_engine)
        {
            Some(true) => thinking_disabled = true,
            Some(false) => thinking_enabled = true,
            None => thinking_unknown = true,
        }
        if config
            .get("mmproj_path")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|path| !path.is_empty())
        {
            vision_supported = true;
        }
    }

    let vision_state = if vision_supported {
        "supported"
    } else if !vision_candidate || vision_unknown {
        "unavailable"
    } else {
        "unsupported"
    };
    if vision_only {
        return json!({ "vision": vision_state });
    }
    let thinking_state = if thinking_unknown || (!thinking_enabled && !thinking_disabled) {
        "unavailable"
    } else if thinking_disabled {
        "unsupported"
    } else {
        "supported"
    };

    let rag_state = if crate::rag::available(&state).await {
        "supported"
    } else {
        "unavailable"
    };
    let rag_collections = crate::rag::collections(&state).await;
    // ComfyUI workflows some machine offers for the Chat's Generate image.
    let image_workflows = crate::chat_image::network_workflows(state).await;
    // A machine of the pipeline has its model unloaded (`unload_if_idle`) or
    // is loading it again: the next answer waits for the reload, and the Chat
    // says so instead of looking stuck.
    let asleep = matching.iter().any(|peer| peer.idle);

    json!({
        "active_model": active_model,
        "vision": vision_state,
        "coverage": coverage_state,
        "asleep": asleep,
        "thinking": thinking_state,
        // A plugin of some machine serves retrieval to this network.
        "rag": rag_state,
        // What a request may keep its retrieval to (`rag_collection`).
        "rag_collections": rag_collections,
        "image": if image_workflows.is_empty() { "unavailable" } else { "supported" },
        "image_workflows": image_workflows,
    })
}

/// How a machine loaded the model: the `/health` of its inference server,
/// raw.
///
/// It stays out of `/api/ui/cluster` on purpose. The Dashboard reloads the
/// cluster on a short cycle, and this is detail that only matters with the card
/// open: fetching on demand avoids paying one `/health` per peer on every screen
/// refresh, and a peer that does not answer no longer takes down the whole listing.
///
/// **It holds for any node, this one included.** The local entry of `all_peers`
/// has `inference_url` like the others, and it is the same path that
/// `ready_for_pipeline` already walks - a single piece of code, so the local
/// node's card never shows less than a remote one's card.
pub(super) async fn peer_runtime(
    State(state): State<GatewayState>,
    Path(node_id): Path<String>,
) -> Response {
    if !state
        .auth
        .read()
        .await
        .profile
        .allows(crate::profile::ProfileAction::ViewNetworkPeers)
        && node_id != state.network.node_id
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "the infer profile can only see its own machine" })),
        )
            .into_response();
    }

    let Some(peer) = state
        .all_peers()
        .await
        .into_iter()
        .find(|p| p.node_id == node_id)
    else {
        return (StatusCode::NOT_FOUND, "unknown machine").into_response();
    };

    let Some(base) = peer.inference_url() else {
        // The peer exists and does not serve inference: it is not a network
        // error, it is the normal state of a machine with no slice deployed.
        return Json(json!({ "loaded": false, "reason": "no inference server" })).into_response();
    };

    let http = match state.peer_http.for_peer(&node_id) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("no client for {node_id}: {e}"),
            )
                .into_response()
        }
    };

    let url = format!("{base}/health");
    match tokio::time::timeout(PEER_TIMEOUT, http.get(&url).send()).await {
        Ok(Ok(r)) if r.status().is_success() => match r.json::<serde_json::Value>().await {
            Ok(v) => Json(json!({ "loaded": true, "health": v })).into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, format!("invalid response: {e}")).into_response(),
        },
        Ok(Ok(r)) => Json(json!({ "loaded": false, "reason": format!("HTTP {}", r.status()) }))
            .into_response(),
        Ok(Err(e)) => Json(json!({ "loaded": false, "reason": e.to_string() })).into_response(),
        Err(_) => Json(json!({ "loaded": false, "reason": "timeout" })).into_response(),
    }
}

/// A peer's `/api/ui/node` through the p2p tunnel only: the last resort of
/// the reads that already walked the peer's addresses with the `RouteTable`
/// failure memory. A peer behind symmetric NAT has no address that answers,
/// and without this it stayed listed but unreadable.
async fn read_node_via_tunnel(
    state: &GatewayState,
    peer: &crate::network::PeerInfo,
) -> Option<serde_json::Value> {
    crate::peer_client::TunnelPeer::from_peer(peer)?;
    let response = state
        .control_get_via_tunnel(peer, "/api/ui/node", PEER_TIMEOUT)
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// Each peer's installed models, so the setup screen can say who is missing what.
pub(super) async fn cluster_models(State(state): State<GatewayState>) -> impl IntoResponse {
    let self_id = state.network.node_id.clone();
    let profile = state.auth.read().await.profile;
    let peers = state
        .all_peers()
        .await
        .into_iter()
        .filter(|peer| {
            profile.allows(crate::profile::ProfileAction::ViewNetworkPeers)
                || peer.node_id == self_id
        })
        .collect::<Vec<_>>();
    let local = local_catalog(&state).await;
    // The name this node announces, not the raw OS hostname. See `models`.
    let local_host = state.network.hostname();

    let peer_state = state.clone();
    let futures: Vec<_> = peers
        .iter()
        .filter(|p| p.node_id != self_id)
        .map(|p| {
            let peer = p.clone();
            let node_id = p.node_id.clone();
            let state = peer_state.clone();
            async move {
                match state
                    .control_get(&peer, "/api/ui/models", PEER_TIMEOUT * 3)
                    .await
                {
                    Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                        Ok(v) => (node_id, Some(v), None),
                        Err(e) => (node_id, None, Some(format!("invalid response: {e}"))),
                    },
                    Ok(r) => (node_id, None, Some(format!("HTTP {}", r.status()))),
                    Err(e) => (node_id, None, Some(format!("{e:#}"))),
                }
            }
        })
        .collect();

    let hf_sources = crate::model_provider_backend::installed_sources(state.model_dirs().await).await;
    let mut out = vec![json!({
        "hf_sources": hf_sources,
        "node_id": self_id,
        "hostname": local_host,
        "is_self": true,
        "models": local.models,
        "shard_sets": local.shard_sets,
        "sidecars": local.sidecars,
        "image_files": local.image_files,
        "scanned": local.scanned,
        "metadata_complete": local.metadata_complete,
    })];

    for (node_id, value, error) in futures::future::join_all(futures).await {
        match value {
            Some(v) => out.push(json!({
                "node_id": node_id,
                "hostname": v.get("hostname").cloned().unwrap_or(json!(null)),
                "is_self": false,
                "models": v.get("models").cloned().unwrap_or(json!([])),
                "shard_sets": v.get("shard_sets").cloned().unwrap_or(json!([])),
                "sidecars": v.get("sidecars").cloned().unwrap_or(json!([])),
                "image_files": v.get("image_files").cloned().unwrap_or(json!([])),
                // Absent on an older node: its models just offer no Download here.
                "hf_sources": v.get("hf_sources").cloned().unwrap_or(json!([])),
                "scanned": v.get("scanned").cloned().unwrap_or(json!([])),
                "metadata_complete": v
                    .get("metadata_complete")
                    .and_then(|value| value.as_bool())
                    .map_or(serde_json::Value::Null, serde_json::Value::Bool),
            })),
            // A peer that did not answer is still a machine with a name: the
            // local peer table keeps what it announced. Without this the
            // object went out **without `hostname`** and the screen fell back to
            // the `node_id`, that is, a hexadecimal prefix in place of the name.
            None => out.push(json!({
                "node_id": node_id,
                "hostname": peers
                    .iter()
                    .find(|p| p.node_id == node_id)
                    .map(|p| p.hostname.clone())
                    .filter(|h| !h.is_empty()),
                "is_self": false,
                "error": error,
                "models": [],
                "shard_sets": [],
                "sidecars": [],
            })),
        }
    }

    Json(json!({ "peers": out }))
}

#[cfg(test)]
mod tests {
    use super::reported_peer_load;
    use serde_json::json;

    #[test]
    fn remote_load_is_present_only_when_the_peer_reported_it() {
        assert_eq!(reported_peer_load(&json!({ "load": 3 })), Some(3));
        assert_eq!(reported_peer_load(&json!({})), None);
        assert_eq!(reported_peer_load(&json!({ "load": -1 })), None);
    }
}
