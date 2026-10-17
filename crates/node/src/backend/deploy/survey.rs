use super::*;
// ── What a machine can tell us about itself ──────────────────────────────────

/// One slice of the chosen set, as it exists on one machine.
#[derive(Debug, Clone)]
pub(super) struct HeldShard {
    /// The same range can exist as a whole GGUF and as a slice. The
    /// distinction has to survive the survey so the dialog does not offer the
    /// wrong choice when both files are on the machine.
    pub(super) kind: HeldKind,
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    /// How many layers the *original* model had, when the slice says so.
    ///
    /// `None` is the normal state of an old slice: only `split_model.py` writes
    /// `pom.original_block_count`, so anything cut before it — the captain's
    /// `qwen_0_15.gguf` / `qwen_16_31.gguf` — carries no total at all. It stays
    /// an `Option` all the way to `model_size` because a missing total and a
    /// total of zero are different facts, and collapsing them is what let a
    /// deploy start and die at the far end (`runtime::NodeConfig::validate`).
    pub(super) total_layers: Option<u32>,
    pub(super) name: String,
    pub(super) path: String,
    /// Adjacent llama.cpp projector, when the catalog found one.
    pub(super) mmproj_path: Option<String>,
    /// All compatible projectors exposed by the catalog for operator choice.
    pub(super) mmproj_candidates: Vec<String>,
    /// Reason returned by the catalog if the association could not be proven.
    pub(super) mmproj_reason: Option<String>,
    pub(super) size_bytes: u64,
    /// sha256 of the part, when it is content-addressed.
    ///
    /// This is the identity `launch` sends to the destination: with it the node
    /// resolves the local path on its own, and two slices of different models
    /// that happen to cover the same range stop being the same thing in the
    /// eyes of `Machine::holds`. `None` on a whole model, which is not a part
    /// of any round.
    pub(super) sha256: Option<String>,
    /// Identity of the slicing round that produced the part.
    pub(super) pom_id: Option<String>,
    /// Planner-only range backed by a whole GGUF that still needs local
    /// materialization before it can be transferred or loaded.
    pub(super) virtual_slice: bool,
    /// TP rank of this part, when the round was sliced for tensor parallelism.
    pub(super) tp_rank: Option<u32>,
    /// Total number of ranks of this TP round.
    pub(super) tp_size: Option<u32>,
    /// Whether the actual GGUF metadata identifies this model as MoE.
    pub(super) is_moe: bool,
    /// Model evidence read from the GGUF, when the peer managed to establish it.
    pub(super) model_capabilities: Option<crate::gguf::ModelCapabilityEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HeldKind {
    Shard,
    Whole,
}

/// A machine in the cluster, with the slices of this set it already holds.
pub(super) struct Machine {
    pub(super) node_id: String,
    pub(super) hostname: String,
    /// URL of the peer's inference server (rank 0 of a TP group).
    pub(super) inference_url: Option<String>,
    /// Every address announced for the inference server, in the order in
    /// which this coordinator knows them.
    pub(super) inference_candidates: Vec<String>,
    /// UDP addresses announced by the peer, to rehydrate the p2p tunnel.
    pub(super) addr_candidates: Vec<String>,
    /// Identity and capability of the inference server's alternative
    /// transport, preserved for the TP group.
    pub(super) endpoint_id: Option<String>,
    pub(super) supports_tunnel: bool,
    /// Runtime capabilities announced by the selected peer.
    pub(super) inference_capabilities: Option<Vec<String>>,
    /// Memory the weights would have to fit in: free VRAM on a machine with a
    /// discrete GPU, available RAM anywhere else. `0` means the machine did not
    /// answer, and nothing is inferred from that.
    pub(super) mem_free_mb: u64,
    pub(super) gpu_free_mb: u64,
    pub(super) ram_free_mb: u64,
    /// Room for the file itself, which is what a copy needs and what
    /// `transfer.rs` checks before it writes a byte.
    pub(super) disk_free_mb: u64,
    pub(super) rtt_ms: Option<f64>,
    /// Numeric fallback used by the planner when a peer did not expose load.
    /// `load_known` keeps that fallback out of the UI.
    pub(super) load: u32,
    pub(super) load_known: bool,
    pub(super) held: Vec<HeldShard>,
}

#[derive(Default, Clone)]
pub(super) struct PairwiseRtt {
    pub(super) values: std::collections::HashMap<(String, String), f64>,
}

impl PairwiseRtt {
    pub(super) fn remember(
        &mut self,
        from_node_id: &str,
        measurement: &crate::route::RttMeasurement,
    ) {
        self.values.insert(
            (from_node_id.to_string(), measurement.target_node_id.clone()),
            measurement.rtt_ms,
        );
    }

    pub(super) fn get(&self, from_node_id: &str, to_node_id: &str) -> Option<f64> {
        self.values
            .get(&(from_node_id.to_string(), to_node_id.to_string()))
            .copied()
    }
}

pub(super) struct Survey {
    pub(super) machines: Vec<Machine>,
    pub(super) pairwise_rtt: PairwiseRtt,
    pub(super) metadata_pending: Option<String>,
    /// Vision projectors and MTP drafts each machine reported, by node id.
    pub(super) sidecars: HashMap<String, Vec<crate::catalog::Sidecar>>,
    /// Architecture and width of the selected model, as the first machine that
    /// described it reported them. What a sidecar has to match.
    pub(super) model_shape: Option<ModelShape>,
}

/// What a sidecar has to match, read from the selected model's catalog entry.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ModelShape {
    /// The model label: the key of its sidecar link.
    pub(super) model_key: String,
    pub(super) architecture: Option<String>,
    pub(super) embedding_length: Option<u32>,
    /// The GGUF carries its own `nextn` block, so MTP needs no external draft.
    pub(super) embedded_mtp: bool,
}

/// The selected model's shape in one machine's `/api/ui/models` answer: the
/// whole model with this name, or the shards of the set with this group.
pub(super) fn model_shape_in(
    body: &serde_json::Value,
    group: &str,
    kind: DeployKind,
) -> Option<ModelShape> {
    let mut model_key = group.to_string();
    let entries: Vec<&serde_json::Value> = if kind.whole() {
        body.get("models")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter(|model| model.get("name").and_then(|name| name.as_str()) == Some(group))
            .collect()
    } else {
        let sets: Vec<&serde_json::Value> = body
            .get("shard_sets")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter(|set| {
                set.get("group").and_then(|value| value.as_str()) == Some(group)
                    || set.get("model_label").and_then(|value| value.as_str()) == Some(group)
            })
            .collect();
        if let Some(label) = sets
            .iter()
            .find_map(|set| set.get("model_label").and_then(|value| value.as_str()))
            .filter(|label| !label.is_empty())
        {
            model_key = label.to_string();
        }
        sets.into_iter()
            .flat_map(|set| {
                set.get("shards")
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
            })
            .collect()
    };
    let described = entries.iter().find(|entry| {
        entry
            .get("embedding_length")
            .and_then(|value| value.as_u64())
            .is_some()
    })?;
    Some(ModelShape {
        model_key,
        architecture: described
            .get("architecture")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        embedding_length: described
            .get("embedding_length")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok()),
        embedded_mtp: entries.iter().any(|entry| {
            entry
                .pointer("/capabilities/mtp")
                .and_then(|value| value.as_bool())
                == Some(true)
        }),
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct RttReport {
    pub(super) source_node_id: String,
    pub(super) measurements: Vec<crate::route::RttMeasurement>,
}

impl Machine {
    pub(super) fn holds(&self, start: u32, end: u32) -> Option<&HeldShard> {
        self.holds_kind(start, end, None)
    }

    pub(super) fn holds_kind(
        &self,
        start: u32,
        end: u32,
        kind: Option<HeldKind>,
    ) -> Option<&HeldShard> {
        self.held.iter().find(|s| {
            s.start_layer == start
                && s.end_layer == end
                && kind.is_none_or(|wanted| s.kind == wanted)
        })
    }
    pub(super) fn holds_ready(&self, start: u32, end: u32) -> Option<&HeldShard> {
        self.held
            .iter()
            .find(|s| s.start_layer == start && s.end_layer == end && !s.virtual_slice)
    }
    pub(super) fn holds_tp(
        &self,
        start: u32,
        end: u32,
        rank: u32,
        size: u32,
    ) -> Option<&HeldShard> {
        self.held.iter().find(|s| {
            s.start_layer == start
                && s.end_layer == end
                && s.tp_rank == Some(rank)
                && s.tp_size == Some(size)
        })
    }
    pub(super) fn label(&self) -> String {
        if self.hostname.is_empty() {
            self.node_id[..12.min(self.node_id.len())].to_string()
        } else {
            self.hostname.clone()
        }
    }
}

/// Read the load from the peer's own `/api/ui/node` response.
///
/// The coordinator's `LoadTracker` only observes requests through itself. A
/// missing remote response must not become zero in the deployment preview.
fn reported_peer_load(node: &serde_json::Value) -> Option<u32> {
    node.get("load")
        .and_then(serde_json::Value::as_u64)
        .and_then(|load| u32::try_from(load).ok())
}

/// Ask every peer what it has, in parallel.
///
/// Reuses each node's own `/api/ui/models`, which is the same answer the
/// cluster catalog screen shows — a machine is the only honest source for where
/// its own files are, and for a path we would otherwise have to guess — and its
/// `/api/ui/node`, which is the only honest source for how much room it has.
/// Test door for `survey`, which is private: the suite has to prove that the
/// survey talks to the peers through the pinned client. See
/// `webui::catalog_under_tls_tests`.
#[cfg(test)]
pub(crate) async fn survey_for_tests(state: &GatewayState, group: &str) -> usize {
    survey(state, group, DeployKind::Shards, None, None)
        .await
        .machines
        .len()
}

/// Whether a peer's partial catalog contains the model family being planned.
///
/// `metadata_complete` is deliberately not enough to answer this question:
/// it describes the peer's whole catalog, while deployment only needs the
/// selected ranges. The group presence also keeps a slow, unrelated model
/// from turning into a refusal for this deployment.
pub(super) fn catalog_contains_group(
    body: &serde_json::Value,
    group: &str,
    kind: DeployKind,
) -> bool {
    let in_shards = |set: &serde_json::Value| {
        set.get("group").and_then(|value| value.as_str()) == Some(group)
            || set.get("model_label").and_then(|value| value.as_str()) == Some(group)
    };

    match kind {
        DeployKind::Shards => body
            .get("shard_sets")
            .and_then(|value| value.as_array())
            .is_some_and(|sets| {
                sets.iter()
                    .any(|set| set.get("group").and_then(|value| value.as_str()) == Some(group))
            }),
        DeployKind::Whole => {
            body.get("models")
                .and_then(|value| value.as_array())
                .is_some_and(|models| {
                    models.iter().any(|model| {
                        model.get("name").and_then(|value| value.as_str()) == Some(group)
                    })
                })
                || body
                    .get("shard_sets")
                    .and_then(|value| value.as_array())
                    .is_some_and(|sets| sets.iter().any(in_shards))
        }
        DeployKind::TensorParallel => {
            body.get("shard_sets")
                .and_then(|value| value.as_array())
                .is_some_and(|sets| sets.iter().any(in_shards))
                || body
                    .get("models")
                    .and_then(|value| value.as_array())
                    .is_some_and(|models| {
                        models.iter().any(|model| {
                            model.get("name").and_then(|value| value.as_str()) == Some(group)
                        })
                    })
        }
    }
}

/// Return the selected whole GGUF's path when its header has not reached the
/// catalog response yet. The path comes from the peer's fast inventory, so the
/// targeted read remains restricted to a file already visible to that peer.
pub(super) fn selected_model_metadata_path(
    body: &serde_json::Value,
    group: &str,
    kind: DeployKind,
) -> Option<String> {
    if !matches!(kind, DeployKind::Whole | DeployKind::TensorParallel) {
        return None;
    }
    body.get("models")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .find(|model| {
            model.get("name").and_then(|value| value.as_str()) == Some(group)
                && model
                    .get("shard_range")
                    .is_none_or(serde_json::Value::is_null)
                && model
                    .get("total_layers")
                    .and_then(|value| value.as_u64())
                    .is_none_or(|total| total == 0)
        })
        .and_then(|model| model.get("path"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// Ask only for the selected model's GGUF header while the peer's catalog is
/// still enriching unrelated files. A failed or unavailable targeted route is
/// intentionally left as a pending selection, never guessed from a filename.
pub(super) async fn enrich_selected_model_on_peer(
    state: &GatewayState,
    peer: &PeerInfo,
    body: &mut serde_json::Value,
    path: String,
) {
    let request_body = crate::webui::ModelMetadataRequest { path: path.clone() };
    let response = state
        .control_post_json(peer, "/api/ui/model-metadata", &request_body, PEER_TIMEOUT)
        .await;
    let Ok(response) = response else { return };
    if !response.status().is_success() {
        return;
    }
    let Ok(model) = response.json::<serde_json::Value>().await else {
        return;
    };
    let Some(models) = body
        .get_mut("models")
        .and_then(|value| value.as_array_mut())
    else {
        return;
    };
    if let Some(target) = models.iter_mut().find(|candidate| {
        candidate.get("path").and_then(|value| value.as_str()) == Some(path.as_str())
    }) {
        *target = model;
    }
}

pub(super) fn selected_metadata_pending_reason(
    kind: DeployKind,
    group: &str,
    missing: &[(u32, u32)],
) -> String {
    match (kind, missing.first()) {
        (DeployKind::Whole, _) => format!(
            "the selection of {group} is still waiting for the GGUF metadata; wait for this selection to be read"
        ),
        (DeployKind::Shards, Some((start, end))) => format!(
            "the selection of slice L{start}-{end} is still waiting for the GGUF metadata; wait for this selection to be read"
        ),
        (DeployKind::TensorParallel, Some((start, end))) => format!(
            "the tensor-parallel selection L{start}-{end} is still waiting for the GGUF metadata; wait for this selection to be read"
        ),
        (_, None) => format!(
            "the selection of {group} is still waiting for the GGUF metadata; wait for this selection to be read"
        ),
    }
}

pub(super) fn selected_metadata_pending_reason_for(
    kind: DeployKind,
    group: &str,
    selected_ranges: &[(u32, u32)],
    tp: Option<&TpLaunch>,
    machines: &[Machine],
) -> Option<String> {
    if kind == DeployKind::TensorParallel && has_tp_parts(machines) {
        if let Some(tp) = tp {
            let total_layers = model_size(&candidates(machines))
                .value()
                .or_else(|| selected_ranges.iter().map(|(_, end)| end + 1).max());
            if let Some(total_layers) = total_layers {
                let end_layer = total_layers.saturating_sub(1);
                let missing_ranks = (0..tp.size)
                    .filter(|rank| {
                        !machines.iter().any(|machine| {
                            machine.held.iter().any(|shard| {
                                shard.start_layer == 0
                                    && shard.end_layer == end_layer
                                    && shard.tp_rank == Some(*rank)
                                    && shard.tp_size == Some(tp.size)
                                    && !shard.virtual_slice
                            })
                        })
                    })
                    .map(|rank| rank + 1)
                    .collect::<Vec<_>>();
                if !missing_ranks.is_empty() {
                    return Some(format!(
                        "the tensor-parallel selection of {group} is still waiting for the GGUF metadata of ranks {}; wait for this selection to be read",
                        missing_ranks
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
        return None;
    }

    let missing = selected_ranges
        .iter()
        .copied()
        .filter(|(start, end)| {
            !machines
                .iter()
                .any(|machine| machine.holds_ready(*start, *end).is_some())
        })
        .collect::<Vec<_>>();
    (!missing.is_empty()).then(|| selected_metadata_pending_reason(kind, group, &missing))
}

pub(super) async fn survey(
    state: &GatewayState,
    group: &str,
    kind: DeployKind,
    selection: Option<&[SelectedSlice]>,
    tp: Option<&TpLaunch>,
) -> Survey {
    let selected_ranges = selection.map(|selected| {
        selected
            .iter()
            .map(|slice| (slice.start_layer, slice.end_layer))
            .collect::<Vec<_>>()
    });
    // An inference node is never a deployment target: it is not surveyed, so
    // no plan can place a range on it and no screen offers it.
    let peers: Vec<PeerInfo> = state
        .all_peers()
        .await
        .into_iter()
        .filter(|peer| !crate::gateway::peer_is_infer(state, peer))
        .collect();
    let self_id = state.network.node_id.clone();
    let mut route_rtt: std::collections::HashMap<String, f64> = state
        .network
        .routes
        .snapshot()
        .into_iter()
        .map(|(node_id, route)| (node_id, route.rtt_ms))
        .collect();
    // This gateway's p2p measurement is the correct source for the machine
    // cards. The HTTP RouteTable stays only as a fallback for old peers or
    // while the first p2p probe has not answered yet.
    for measurement in state.network.rtts.snapshot() {
        route_rtt.insert(measurement.target_node_id, measurement.rtt_ms);
    }
    let mut pairwise_rtt = PairwiseRtt::default();
    for measurement in state.network.rtts.snapshot() {
        pairwise_rtt.remember(&self_id, &measurement);
    }
    for (node_id, rtt_ms) in &route_rtt {
        if pairwise_rtt.get(&self_id, node_id).is_none() {
            pairwise_rtt
                .values
                .insert((self_id.clone(), node_id.clone()), *rtt_ms);
        }
    }
    let futures = peers.into_iter().map(|peer| {
        let is_self = peer.node_id == self_id;
        let rtt_ms = if is_self {
            Some(0.0)
        } else {
            route_rtt.get(&peer.node_id).copied()
        };
        let group = group.to_string();
        let state = state;
        async move {
            // `control_get` pins the client to the destination's node_id (with
            // the cluster on TLS, without this the survey failed silently) and
            // falls back to the tunnel when no announced address answers.
            let (models, node, rtt) = futures::future::join3(
                state.control_get(&peer, "/api/ui/models", PEER_TIMEOUT * 4),
                state.control_get(&peer, "/api/ui/node", PEER_TIMEOUT),
                state.control_get(&peer, "/api/ui/network/rtt", PEER_TIMEOUT),
            )
            .await;

            let mut body: serde_json::Value = models.ok()?.json().await.ok()?;
            if let Some(path) = selected_model_metadata_path(&body, &group, kind) {
                enrich_selected_model_on_peer(state, &peer, &mut body, path).await;
            }
            let metadata_pending_candidate = body
                .get("metadata_complete")
                .and_then(|value| value.as_bool())
                == Some(false)
                && catalog_contains_group(&body, &group, kind);
            let node: serde_json::Value = match node {
                Ok(r) => r.json().await.unwrap_or(json!({})),
                Err(_) => json!({}),
            };
            let addr_candidates = node
                .get("addr_candidates")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .filter(|values| !values.is_empty())
                .unwrap_or_else(|| {
                    if peer.addr_candidates.is_empty() {
                        vec![peer.addr.clone()]
                    } else {
                        peer.addr_candidates.clone()
                    }
                });
            let inference_port = node
                .get("inference_port")
                .and_then(|value| value.as_u64())
                .and_then(|value| u16::try_from(value).ok())
                .or(peer.inference_port);
            let inference_candidates = inference_port
                .map(|port| {
                    addr_candidates
                        .iter()
                        .map(|addr| {
                            format!(
                                "{}://{}:{}",
                                crate::peer_client::peer_scheme(),
                                crate::network::ip_host(addr),
                                port
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| peer.inference_candidates());
            let endpoint_id = node
                .get("endpoint_id")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .or_else(|| peer.endpoint_id.clone());
            let supports_tunnel = node
                .get("inference_frame_capabilities")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .any(|value| value.as_str() == Some(crate::pipeline::CAP_P2P_TUNNEL))
                })
                .unwrap_or_else(|| {
                    peer.inference_frame_capabilities
                        .iter()
                        .any(|value| value == crate::pipeline::CAP_P2P_TUNNEL)
                });
            let inference_capabilities = node
                .get("inference_frame_capabilities")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .or_else(|| {
                    (!peer.inference_frame_capabilities.is_empty())
                        .then(|| peer.inference_frame_capabilities.clone())
                });
            let rtt_report = match rtt {
                Ok(response) if response.status().is_success() => {
                    response.json::<RttReport>().await.ok()
                }
                _ => None,
            };
            let (gpu_free_mb, ram_free_mb) = memory_capacity(node.get("system"));
            let load = reported_peer_load(&node);

            let mut held: Vec<HeldShard> = match kind {
                DeployKind::Shards => body
                    .get("shard_sets")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter(|set| set.get("group").and_then(|g| g.as_str()) == Some(group.as_str()))
                    .flat_map(|set| {
                        let pom_id = set.get("pom_id").and_then(|value| value.as_str());
                        let mmproj_path = set.get("mmproj_path").and_then(|value| value.as_str());
                        let mmproj_candidates =
                            parse_mmproj_candidates(set.get("mmproj_candidates"));
                        let mmproj_reason =
                            set.get("mmproj_reason").and_then(|value| value.as_str());
                        set.get("shards")
                            .and_then(|v| v.as_array())
                            .into_iter()
                            .flatten()
                            .map(move |shard| {
                                (
                                    shard,
                                    pom_id,
                                    mmproj_path,
                                    mmproj_reason,
                                    mmproj_candidates.clone(),
                                )
                            })
                    })
                    .filter_map(
                        |(shard, pom_id, mmproj_path, mmproj_reason, mmproj_candidates)| {
                            parse_shard(
                                shard,
                                pom_id,
                                mmproj_path,
                                mmproj_reason,
                                &mmproj_candidates,
                            )
                        },
                    )
                    .collect(),
                DeployKind::Whole => {
                    let mut models = body
                        .get("models")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|m| parse_whole(m, &group))
                        .collect::<Vec<_>>();
                    let mut slices = body
                        .get("shard_sets")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter(|set| {
                            set.get("group").and_then(|value| value.as_str())
                                == Some(group.as_str())
                                || set.get("model_label").and_then(|value| value.as_str())
                                    == Some(group.as_str())
                        })
                        .flat_map(|set| {
                            let pom_id = set.get("pom_id").and_then(|value| value.as_str());
                            let mmproj_path =
                                set.get("mmproj_path").and_then(|value| value.as_str());
                            let mmproj_candidates =
                                parse_mmproj_candidates(set.get("mmproj_candidates"));
                            let mmproj_reason =
                                set.get("mmproj_reason").and_then(|value| value.as_str());
                            set.get("shards")
                                .and_then(|v| v.as_array())
                                .into_iter()
                                .flatten()
                                .map(move |shard| {
                                    (
                                        shard,
                                        pom_id,
                                        mmproj_path,
                                        mmproj_reason,
                                        mmproj_candidates.clone(),
                                    )
                                })
                        })
                        .filter_map(
                            |(shard, pom_id, mmproj_path, mmproj_reason, mmproj_candidates)| {
                                parse_shard(
                                    shard,
                                    pom_id,
                                    mmproj_path,
                                    mmproj_reason,
                                    &mmproj_candidates,
                                )
                            },
                        )
                        .collect::<Vec<_>>();
                    models.append(&mut slices);
                    models
                }
                DeployKind::TensorParallel => {
                    let parts: Vec<HeldShard> = body
                        .get("shard_sets")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                        .filter(|set| {
                            set.get("group").and_then(|g| g.as_str()) == Some(group.as_str())
                                || set.get("model_label").and_then(|g| g.as_str())
                                    == Some(group.as_str())
                        })
                        .flat_map(|set| {
                            let pom_id = set.get("pom_id").and_then(|value| value.as_str());
                            let mmproj_path =
                                set.get("mmproj_path").and_then(|value| value.as_str());
                            let mmproj_candidates =
                                parse_mmproj_candidates(set.get("mmproj_candidates"));
                            let mmproj_reason =
                                set.get("mmproj_reason").and_then(|value| value.as_str());
                            set.get("tp_parts")
                                .and_then(|v| v.as_array())
                                .into_iter()
                                .flatten()
                                .map(move |shard| {
                                    (
                                        shard,
                                        pom_id,
                                        mmproj_path,
                                        mmproj_reason,
                                        mmproj_candidates.clone(),
                                    )
                                })
                        })
                        .filter_map(
                            |(shard, pom_id, mmproj_path, mmproj_reason, mmproj_candidates)| {
                                parse_shard(
                                    shard,
                                    pom_id,
                                    mmproj_path,
                                    mmproj_reason,
                                    &mmproj_candidates,
                                )
                            },
                        )
                        .collect();
                    if parts.is_empty() {
                        body.get("models")
                            .and_then(|v| v.as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(|m| parse_whole(m, &group))
                            .collect()
                    } else {
                        parts
                    }
                }
            };
            // Directory order is whatever the filesystem felt like; the screen
            // lists these as "L0–4 · L5–31" and reads as noise unsorted.
            held.sort_by_key(|s| (s.start_layer, s.end_layer, s.tp_rank.unwrap_or(0)));
            // Two copies of the same file on one machine are one range, not
            // two: `candidates` would otherwise list the machine twice as a
            // holder of it. The whole-model path meets this the moment the same
            // GGUF is reachable through two scanned folders — the captain's
            // `~/.ollama/models` is a symlink into the LM Studio library.
            held.dedup_by_key(|s| (s.start_layer, s.end_layer, s.kind, s.tp_rank, s.tp_size));

            Some((
                Machine {
                    node_id: peer.node_id.clone(),
                    inference_url: inference_candidates.first().cloned(),
                    inference_candidates,
                    addr_candidates,
                    endpoint_id,
                    supports_tunnel,
                    inference_capabilities,
                    hostname: if peer.hostname.is_empty() && is_self {
                        body.get("hostname")
                            .and_then(|h| h.as_str())
                            .unwrap_or_default()
                            .to_string()
                    } else {
                        peer.hostname.clone()
                    },
                    mem_free_mb: free_memory_mb(node.get("system")),
                    gpu_free_mb,
                    ram_free_mb,
                    // The peer's own reading beats the one it gossiped minutes ago,
                    // but the gossip is still there when it did not answer.
                    disk_free_mb: node
                        .pointer("/system/disk/free_mb")
                        .and_then(|v| v.as_f64())
                        .map(|v| v as u64)
                        .unwrap_or(peer.disk_free_mb),
                    rtt_ms,
                    load: load.unwrap_or(0),
                    load_known: load.is_some(),
                    held,
                },
                rtt_report,
                metadata_pending_candidate,
                body.get("sidecars")
                    .cloned()
                    .and_then(|value| {
                        serde_json::from_value::<Vec<crate::catalog::Sidecar>>(value).ok()
                    })
                    .unwrap_or_default(),
                model_shape_in(&body, &group, kind),
            ))
        }
    });

    let mut machines = Vec::new();
    let mut metadata_pending_candidate = false;
    let mut sidecars = HashMap::new();
    let mut model_shape: Option<ModelShape> = None;
    for result in futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
    {
        metadata_pending_candidate |= result.2;
        sidecars.insert(result.0.node_id.clone(), result.3);
        if model_shape.is_none() {
            model_shape = result.4;
        }
        if let Some(report) = &result.1 {
            if report.source_node_id == result.0.node_id {
                for measurement in &report.measurements {
                    pairwise_rtt.remember(&report.source_node_id, measurement);
                }
            }
        }
        machines.push(result.0);
    }

    let metadata_pending = selected_ranges.as_deref().and_then(|selected| {
        if !metadata_pending_candidate {
            return None;
        }
        selected_metadata_pending_reason_for(kind, group, selected, tp, &machines)
    });

    Survey {
        machines,
        pairwise_rtt,
        metadata_pending,
        sidecars,
        model_shape,
    }
}

/// Where the weights of a slice would actually have to fit.
///
/// A machine with a discrete GPU loads them into VRAM (`--n-gpu-layers 99`), so
/// its RAM figure says nothing useful. Unified memory has no separate pool to
/// report, so RAM *is* the answer there — see the note `sysmon` puts on screen.
pub(super) fn free_memory_mb(system: Option<&serde_json::Value>) -> u64 {
    let (gpu, ram) = memory_capacity(system);
    if gpu > 0 {
        gpu
    } else {
        ram
    }
}

pub(super) fn memory_capacity(system: Option<&serde_json::Value>) -> (u64, u64) {
    let Some(system) = system else { return (0, 0) };
    let gpu = system
        .get("gpus")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let total = g.get("vram_total_mb")?.as_f64()?;
            let used = g
                .get("vram_used_mb")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            Some((total - used).max(0.0))
        })
        .sum::<f64>() as u64;
    let ram = system
        .pointer("/ram/available_mb")
        .and_then(|v| v.as_f64())
        .map(|v| v as u64)
        .unwrap_or(0);
    (gpu, ram)
}

fn parse_mmproj_candidates(value: Option<&serde_json::Value>) -> Vec<String> {
    let mut candidates = value
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    candidates
}

pub(super) fn parse_shard(
    v: &serde_json::Value,
    pom_id: Option<&str>,
    set_mmproj_path: Option<&str>,
    set_mmproj_reason: Option<&str>,
    set_mmproj_candidates: &[String],
) -> Option<HeldShard> {
    let range = v.get("shard_range")?.as_array()?;
    let mmproj_path = v
        .get("mmproj_path")
        .and_then(|value| value.as_str())
        .or(set_mmproj_path)
        .map(str::to_string);
    let mut mmproj_candidates = parse_mmproj_candidates(v.get("mmproj_candidates"));
    if mmproj_candidates.is_empty() {
        mmproj_candidates.extend(set_mmproj_candidates.iter().cloned());
    }
    if let Some(path) = &mmproj_path {
        if !mmproj_candidates.iter().any(|candidate| candidate == path) {
            mmproj_candidates.push(path.clone());
        }
    }
    Some(HeldShard {
        kind: HeldKind::Shard,
        start_layer: range.first()?.as_u64()? as u32,
        end_layer: range.get(1)?.as_u64()? as u32,
        total_layers: v
            .get("total_layers")
            .and_then(|t| t.as_u64())
            .map(|t| t as u32)
            .filter(|t| *t > 0),
        name: v.get("name")?.as_str()?.to_string(),
        path: v.get("path")?.as_str()?.to_string(),
        mmproj_path,
        mmproj_candidates,
        mmproj_reason: v
            .get("mmproj_reason")
            .and_then(|value| value.as_str())
            .or(set_mmproj_reason)
            .map(str::to_string),
        size_bytes: v.get("size_bytes").and_then(|s| s.as_u64()).unwrap_or(0),
        sha256: v
            .get("sha256")
            .and_then(|h| h.as_str())
            .filter(|h| crate::pom::is_sha256_hex(h))
            .map(|h| h.to_string()),
        pom_id: pom_id.map(str::to_string),
        virtual_slice: false,
        tp_rank: v
            .get("tp_rank")
            .and_then(|rank| rank.as_u64())
            .map(|rank| rank as u32),
        tp_size: v
            .get("tp_size")
            .and_then(|size| size.as_u64())
            .map(|size| size as u32),
        is_moe: v
            .get("is_moe")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        model_capabilities: v
            .get("capabilities")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
    })
}

/// One whole model of the catalog, read as the range it covers: all of them.
///
/// This is the whole of the whole-model special case. Everything downstream —
/// the coverage arithmetic, the one-range-per-machine rule, the copy planning,
/// `launch` — then works on a `HeldShard` like any other, and a deploy of one
/// un-sliced GGUF is simply a plan with a single step whose range happens to be
/// the entire model.
///
/// Two files are skipped rather than half-described:
///
/// * a slice (`shard_range` set) — those belong to the other `kind`, and the
///   catalog serves both lists from the same scan;
/// * a model whose GGUF header did not yield a layer count. `total_layers` is
///   the number every peer is configured with, and `runtime::NodeConfig::validate`
///   refuses a zero at the far end — after `launch` has already stopped
///   inference on the target machine.
/// The model's name without what the files add around it: `.gguf`, a `-00001-of-00002` split, and the `-xxxxxxxx` tag
/// the slicing run gives a group. A whole GGUF and the slices of the same model compare equal by this.
pub(super) fn model_stem(name: &str) -> String {
    let mut stem = name.strip_suffix(".gguf").unwrap_or(name).to_string();
    let is_tag = |t: &str| t.len() == 8 && t.chars().all(|c| c.is_ascii_hexdigit());
    if let Some(at) = stem.rfind('-') {
        if is_tag(&stem[at + 1..]) {
            stem.truncate(at);
        }
    }
    let split = stem.len().checked_sub(15).map(|at| &stem[at..]);
    if let Some(part) = split {
        let digits = |t: &str| t.len() == 5 && t.chars().all(|c| c.is_ascii_digit());
        if part.starts_with('-') && digits(&part[1..6]) && &part[6..10] == "-of-" && digits(&part[10..]) {
            stem.truncate(stem.len() - 15);
        }
    }
    stem
}

pub(super) fn parse_whole(v: &serde_json::Value, group: &str) -> Option<HeldShard> {
    if !v.get("shard_range").map(|r| r.is_null()).unwrap_or(true) {
        return None;
    }
    let name = v.get("name")?.as_str()?;
    if name != group && model_stem(name) != model_stem(group) {
        return None;
    }
    let total = v
        .get("total_layers")
        .and_then(|t| t.as_u64())
        .filter(|t| *t > 0)? as u32;
    let mmproj_path = v
        .get("mmproj_path")
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let mut mmproj_candidates = parse_mmproj_candidates(v.get("mmproj_candidates"));
    if let Some(path) = &mmproj_path {
        if !mmproj_candidates.iter().any(|candidate| candidate == path) {
            mmproj_candidates.push(path.clone());
        }
    }
    Some(HeldShard {
        kind: HeldKind::Whole,
        start_layer: 0,
        end_layer: total - 1,
        total_layers: Some(total),
        name: name.to_string(),
        path: v.get("path")?.as_str()?.to_string(),
        mmproj_path,
        mmproj_candidates,
        mmproj_reason: v
            .get("mmproj_reason")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        size_bytes: v.get("size_bytes").and_then(|s| s.as_u64()).unwrap_or(0),
        // A whole model is not a part of any round: it has no part hash.
        sha256: None,
        pom_id: None,
        virtual_slice: false,
        tp_rank: None,
        tp_size: None,
        is_moe: v
            .get("is_moe")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        model_capabilities: v
            .get("capabilities")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::reported_peer_load;
    use serde_json::json;

    #[test]
    fn deployment_load_stays_unknown_when_the_peer_did_not_report_it() {
        assert_eq!(reported_peer_load(&json!({ "load": 2 })), Some(2));
        assert_eq!(reported_peer_load(&json!({})), None);
    }
}

#[cfg(test)]
mod model_stem_tests {
    use super::model_stem;

    #[test]
    fn a_whole_gguf_and_the_slices_of_the_same_model_share_a_stem() {
        let whole = "Qwen3.8-Flash-Next-GSQ-RCO-IQ3_XXS-00001-of-00002.gguf";
        let group = "Qwen3.8-Flash-Next-GSQ-RCO-IQ3_XXS-00001-of-00002-552ac6c0";
        assert_eq!(model_stem(whole), "Qwen3.8-Flash-Next-GSQ-RCO-IQ3_XXS");
        assert_eq!(model_stem(whole), model_stem(group));
        assert_ne!(model_stem("Qwen3.8-Flash-Next-GSQ-RCO-IQ2_XXS.gguf"), model_stem(whole));
        assert_eq!(model_stem("plain.gguf"), "plain");
    }
}
