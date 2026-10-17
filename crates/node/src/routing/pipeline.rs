/// Pipeline planner for distributed layer-pipeline inference.
///
/// Given a set of peers (each covering a contiguous layer range), this module
/// selects the optimal ordered sequence of peers that covers layers 0..total_layers,
/// tracks per-peer request load, and exposes the plan to the gateway.
use crate::network::PeerInfo;
use crate::wire::{FrameEncoding, WireDtype};
use routing_contract::{ClusterSnapshot, PeerCapacity, RoutingRequest, ROUTING_API_VERSION};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use tokio::sync::RwLock;
use tracing::{debug, info};

// ── Load tracking ─────────────────────────────────────────────────────────────

/// Tracks in-flight inference requests per peer node.
/// RAII guards via `acquire()` ensure counts are always decremented.
#[derive(Clone, Default)]
pub struct LoadTracker(Arc<RwLock<HashMap<String, Arc<AtomicU32>>>>);

impl LoadTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current in-flight count for a node (0 if unseen).
    pub async fn load(&self, node_id: &str) -> u32 {
        self.0
            .read()
            .await
            .get(node_id)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Increment the counter and return a guard that decrements on drop.
    pub async fn acquire(&self, node_id: &str) -> LoadGuard {
        let counter = {
            let mut map = self.0.write().await;
            map.entry(node_id.to_string())
                .or_insert_with(|| Arc::new(AtomicU32::new(0)))
                .clone()
        };
        counter.fetch_add(1, Ordering::Relaxed);
        LoadGuard { counter }
    }
}

/// How many requests of `model` have chosen their first stage so far. Each request takes the next value, so requests
/// that arrive together start their rotation over the heads at different points (see `fastest::rotate_head`).
pub fn next_head_turn(model: &str) -> usize {
    static TURNS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, usize>>> = std::sync::OnceLock::new();
    let mut turns = TURNS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let turn = turns.entry(model.to_owned()).or_insert(0);
    let current = *turn;
    *turn = turn.wrapping_add(1);
    current
}

/// Decrements the peer load counter when dropped (RAII).
pub struct LoadGuard {
    counter: Arc<AtomicU32>,
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

// ── Pipeline types ────────────────────────────────────────────────────────────

/// A single peer step within the inference pipeline.
///
/// Serializable because it travels on the wire as `next_hops` when relay
/// mode is active (`CAP_RELAY_CHAIN`): a relaying hop pops itself off the
/// front of the list it received and forwards the rest, unchanged, to the
/// next hop's URL — see `inference/relay.rs` and `inference/infer_server.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStep {
    pub node_id: String,
    /// Full HTTP base URL, e.g. "http://192.168.1.5:8181"
    pub inference_url: String,
    /// All the addresses through which this hop can be reached.
    ///
    /// `inference_url` stays filled in and is what an old node reads. Whoever
    /// dials prefers this list, because the choice has to belong to whoever measured: the
    /// plan may have been built by a machine on another network.
    #[serde(default)]
    pub inference_candidates: Vec<String>,
    /// `host:port` addresses announced for this hop's HTTP/3 endpoint.
    /// It is never derived from `inference_url`, which remains exclusively
    /// a TCP URL for HTTP/1.1 or HTTPS.
    #[serde(default)]
    pub quic_addr_candidates: Vec<String>,
    pub start_layer: u32,
    pub end_layer: u32,
    /// The sub-range **this step executes**, which is not always the range it
    /// loaded.
    ///
    /// `start_layer`/`end_layer` say what the peer has on disk; these say what
    /// the sender is telling it to run now. The two are separate because
    /// a peer with L0-59 can be the entry of one branch (it executes L0-59) and the tail
    /// of another (it would execute L40-59) - the loaded range is the same, the work is not.
    ///
    /// The sender decides, and always sends it: without this the recipient ran the
    /// whole slice it loaded, which is how an overlap became a layer
    /// executed twice with no error at all.
    pub exec_start: u32,
    pub exec_end: u32,
    /// Capabilities of the canonical inference frame and relay.
    pub inference_frame_capabilities: Vec<String>,
    /// This hop's endpoint id (the public key, hex), when known — lets a caller reach
    /// it over the endpoint as a NAT-traversal fallback when direct HTTP fails.
    #[serde(default)]
    pub endpoint_id: Option<String>,
    /// UDP addresses announced by the hop to rebuild the p2p tunnel.
    /// The plan carries these addresses because the process that executes the hop may
    /// not have received the same announce in its own discovery table.
    #[serde(default)]
    pub endpoint_addr_candidates: Vec<String>,
}

/// Advertised when a node can continue a relay itself — see the `next_hops`
/// field threaded through `inference/wire.rs` meta — instead of requiring
/// the gateway to call every hop directly. All-or-nothing per plan: if any
/// step lacks it, the whole request falls back to the hub relay unchanged.
pub const CAP_RELAY_CHAIN: &str = "relay/v1-chain";

/// Advertised when a node, relaying a chain, answers the *originator* directly
/// (the `reply_to` in the frame meta) instead of holding its own HTTP response
/// open until the result bubbles back up the call stack — see
/// [`crate::direct`]. Strictly stronger than [`CAP_RELAY_CHAIN`]: a node that
/// advertises this one also advertises that. Same all-or-nothing rule; a plan
/// with one older peer falls back to the bubbling ring unchanged.
pub const CAP_RELAY_DIRECT: &str = "relay/v2-direct";

/// Advertised when a node can draft and verify a batch of candidate tokens
/// in one decode (prompt-lookup speculative decoding) instead of exactly
/// one token per call. Same all-or-nothing-per-plan fallback rule.
pub const CAP_SPECULATE_LOOKUP: &str = "speculate/v1-lookup";

/// Advertised when that node's engine managed to attach the model's own `nextn`
/// draft head - multi-token prediction (MTP). Distinct from
/// [`CAP_SPECULATE_LOOKUP`], which is the repetition lookup in
/// `infer_server::find_draft` and uses no weights at all: see `crate::mtp`.
///
/// It is not all-or-nothing like the others. The `nextn` block lives in the last
/// layer of the model, so in a sliced pipeline only the **tail** has it, and the one
/// that builds the batch is the **head**. So the condition is "the head can draft",
/// which today is the same as saying that one machine serves the whole model; the
/// other hops only need to know how to decode a batch (`CAP_SPECULATE_LOOKUP`),
/// which is exactly what `supports_mtp` requires of them.
pub const CAP_SPECULATE_MTP: &str = "speculate/v1-mtp";

/// Advertised by every new binary, unconditionally, to mark that it
/// speaks the sliced MTP protocol: `speculate` with a prefabricated `draft`, the
/// `tokens` field (which gives the tail the batch composition for `observe`) and the
/// `mtp_k` field (the batch width requested by the gateway).
///
/// It is **all-or-nothing** again, and the reason is closing a hole: an old
/// peer only has `CAP_SPECULATE_MTP`/`CAP_SPECULATE_LOOKUP` and, without this cap,
/// `supports_mtp` would let a mixed ring through - the old head would receive a
/// `speculate {source: Mtp, draft}` it does not understand, and the new tail
/// would produce a `next_draft` it ignores. Requiring the cap from **all** the
/// steps rules the scenario out at once, instead of living with it.
pub const CAP_SPECULATE_MTP_SLICED: &str = "speculate/v1-mtp-sliced";

/// Advertised when a node's p2p endpoint has actually come up
/// (`Network::p2p_tunnel_ready`) — unlike the other capabilities above,
/// this is checked **per-hop**, not all-or-nothing across a plan: a mix of
/// directly-reachable and NAT'd hops in the same pipeline is normal. The
/// relay prefers this transport per hop and falls back to the direct client
/// when it is unavailable or fails at the transport layer.
pub const CAP_P2P_TUNNEL: &str = "transport/iroh-tunnel-v1";

/// Advertised when the node serves `/v1/link`: one long-lived HTTP body pair
/// carrying successive `frame/v1-batch` frames instead of a fresh request per
/// activation. Checked **per-hop**, like the tunnel: the caller opens the
/// channel when the next hop advertises it, and any transport failure degrades
/// to the request-per-frame ladder without ending the generation.
pub const CAP_LINK_STREAM: &str = "transport/link-v1";

/// Advertised only while the local inference server has the validated
/// multi-item scheduler active. It is the first and only inference frame
/// contract; peers are never downgraded to an older wire format.
pub const CAP_FRAME_BATCH: &str = "inference/frame-v1-batch";

/// Advertised when a node can decode and encode the optional FP8 E4M3FN
/// representation of hidden state. This is all-or-nothing across a plan:
/// sending f8 through one peer that only knows f16 would corrupt the answer.
pub const CAP_FRAME_DTYPE_F8: &str = "inference/frame-v1-dtype-f8";
/// Optional signed-int4 hidden-state representation, 64 values per FP16 scale.
pub const CAP_FRAME_DTYPE_I4G64: &str = "inference/frame-v1-dtype-i4g64";
/// Optional signed-int4 hidden-state representation, 128 values per FP16 scale.
pub const CAP_FRAME_DTYPE_I4G128: &str = "inference/frame-v1-dtype-i4g128";

/// Advertised when a remote complete-model peer can own the autoregressive
/// loop and return generated tokens through bounded opportunistic batches.
pub const CAP_REMOTE_TOKEN_STREAM: &str = "inference/remote-token-stream-v1";

/// Advertised when this peer has a `slot_save_path` configured and can save
/// or restore a session's KV cache to local disk via `POST /v1/session/slot`.
/// A deployment snapshot is all-or-nothing: `gateway::snapshot_deployment`
/// refuses unless every hop advertises this.
pub const CAP_SLOT_PERSIST: &str = "inference/slot-persist-v1";

/// A handoff chosen by the planner between two consecutive pipeline steps.
///
/// The layer boundaries let observers attach the edge to the exact advertised
/// ranges, even when a node advertises overlapping model slices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineEdge {
    pub from_node_id: String,
    pub to_node_id: String,
    pub from_end_layer: u32,
    pub to_start_layer: u32,
}

impl PipelineStep {
    pub fn frame_encoding(&self) -> FrameEncoding {
        FrameEncoding::F16
    }

    /// Whether this step's advertised `inference_frame_capabilities` list
    /// contains a given capability string.
    pub fn supports(&self, capability: &str) -> bool {
        self.inference_frame_capabilities
            .iter()
            .any(|cap| cap == capability)
    }
}

/// Ordered pipeline plan for a single inference request.
#[derive(Debug, Clone)]
pub struct PipelinePlan {
    /// Steps in order from first layer to last.
    pub steps: Vec<PipelineStep>,
    /// Handoffs selected by the same planner invocation as `steps`.
    pub edges: Vec<PipelineEdge>,
}

/// The route chosen for a session. The list is the affinity per stage, not
/// a list of alternative peers: after the session enters the KV of a
/// replica, the following rounds have to go back to the same node IDs.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteAffinity {
    pub deployment_id: String,
    pub session_id: String,
    pub hop_replicas: Vec<String>,
}

#[allow(dead_code)]
impl RouteAffinity {
    pub fn from_plan(
        deployment_id: impl Into<String>,
        session_id: impl Into<String>,
        plan: &PipelinePlan,
    ) -> Self {
        Self {
            deployment_id: deployment_id.into(),
            session_id: session_id.into(),
            hop_replicas: plan.steps.iter().map(|step| step.node_id.clone()).collect(),
        }
    }

    pub fn matches_plan(&self, plan: &PipelinePlan) -> bool {
        self.hop_replicas.len() == plan.steps.len()
            && self
                .hop_replicas
                .iter()
                .zip(&plan.steps)
                .all(|(node_id, step)| node_id == &step.node_id)
    }
}

/// Translates transport-owned peer records into the stable routing-provider
/// input. Production routing remains on the existing planner until equivalence
/// coverage permits switching implementations - that is why it still has no caller
/// outside the tests, and the `allow` is the mark of that, not carelessness.
#[allow(dead_code)]
pub(crate) fn routing_request_from_peers(total_layers: u32, peers: &[PeerInfo]) -> RoutingRequest {
    RoutingRequest {
        api_version: ROUTING_API_VERSION,
        cluster: ClusterSnapshot {
            total_layers,
            peers: peers
                .iter()
                .map(|peer| PeerCapacity {
                    peer_id: peer.node_id.clone(),
                    start_layer: peer.start_layer,
                    end_layer: peer.end_layer,
                    supports_inference: peer.inference_port.is_some(),
                })
                .collect(),
        },
    }
}

impl PipelinePlan {
    /// Validate the wire precision before the first request is sent.
    ///
    /// f16 is the baseline. Every lossy extension must be advertised by every
    /// hop, otherwise the request fails before sending ambiguous payloads.
    pub fn validate_wire_dtype(&self, dtype: WireDtype) -> anyhow::Result<()> {
        let Some(capability) = wire_dtype_capability(dtype) else {
            return Ok(());
        };
        for step in &self.steps {
            if !step.supports(capability) {
                return Err(anyhow::anyhow!(
                    "hidden state format {} was not negotiated: peer {} does not announce {}",
                    dtype.as_str(),
                    step.node_id,
                    capability
                ));
            }
        }
        Ok(())
    }

    /// Human-readable hop chain: "abc123→def456→…"
    pub fn hop_chain(&self) -> String {
        self.steps
            .iter()
            .map(|s| s.node_id[..s.node_id.len().min(8)].to_string())
            .collect::<Vec<_>>()
            .join("→")
    }

    /// True only when every step in the plan advertises `CAP_RELAY_CHAIN` —
    /// a plan with one old peer in the middle falls back to the hub relay
    /// entirely rather than mixing relay and hub calls mid-chain.
    pub fn supports_relay_chain(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.supports(CAP_RELAY_CHAIN))
    }

    /// True when every step can answer the originator directly and the chain
    /// is long enough for that to save anything.
    ///
    /// A single-hop plan is excluded deliberately: gateway→hop→gateway is
    /// already the minimum, and routing its answer out of band would only add
    /// a callback to the same two legs.
    pub fn supports_direct_reply(&self) -> bool {
        self.steps.len() >= 2 && self.steps.iter().all(|s| s.supports(CAP_RELAY_DIRECT))
    }

    /// Same all-or-nothing rule as `supports_relay_chain`, for speculative
    /// decoding: every hop must be able to decode a >1-token batch and, on
    /// the tail, verify it.
    pub fn supports_speculation(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().all(|s| s.supports(CAP_SPECULATE_LOOKUP))
    }

    /// Can this plan draft through the model's `nextn` block?
    ///
    /// The block lives in the last layer, therefore in the **tail** of the pipeline - it
    /// is the tail that advertises `CAP_SPECULATE_MTP`. The head (which builds the batch)
    /// receives the ready draft from the gateway, which in turn gathered it from the
    /// tail's direct response in the previous round (`docs/subsystems/
    /// inference.md` §5.2). **All** need `CAP_SPECULATE_LOOKUP`
    /// because the batch larger than one token goes through them, and **all** need
    /// `CAP_SPECULATE_MTP_SLICED` because the new protocol (prefabricated
    /// draft + `tokens` + `mtp_k` on the wire) only exists in new
    /// binaries - an old peer in the ring would receive a `speculate` it does not understand.
    pub fn supports_mtp(&self) -> bool {
        !self.steps.is_empty()
            && self.steps.iter().all(|s| s.supports(CAP_SPECULATE_LOOKUP))
            && self
                .steps
                .iter()
                .all(|s| s.supports(CAP_SPECULATE_MTP_SLICED))
            && self
                .steps
                .last()
                .is_some_and(|s| s.supports(CAP_SPECULATE_MTP))
    }
}

pub fn wire_dtype_capability(dtype: WireDtype) -> Option<&'static str> {
    match dtype {
        WireDtype::F32 | WireDtype::F16 => None,
        WireDtype::F8 => Some(CAP_FRAME_DTYPE_F8),
        WireDtype::I4G64 => Some(CAP_FRAME_DTYPE_I4G64),
        WireDtype::I4G128 => Some(CAP_FRAME_DTYPE_I4G128),
    }
}

// ── Planner ───────────────────────────────────────────────────────────────────

/// Select the optimal inference pipeline for a given model.
///
/// Scoring: `layers_count * 100 - current_load * 10`
/// — prefers peers that cover more layers and are less busy.
///
/// `deploy_id` chooses **which** deployment serves, when the same model is
/// up more than once. `None` means "whichever is there": it matches by the
/// label, which is the usual behavior and the only one possible with a peer of
/// an earlier version, which announces no deployment at all.
///
/// Returns `None` if there is any gap in layer coverage (0..total_layers).
pub async fn plan_pipeline(
    peers: &[PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    tracker: &LoadTracker,
) -> Option<PipelinePlan> {
    plan_pipeline_with_rtt(
        peers,
        model_name,
        deploy_id,
        total_layers,
        tracker,
        &HashMap::new(),
    )
    .await
}

/// Returns the live peers that announce an inference server for the
/// model. The deployment resolution is the same one the planner uses, so
/// that the infer screen does not show a head the gateway would never choose.
pub fn model_pipeline_candidates<'a>(
    peers: &'a [PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
) -> Vec<&'a PeerInfo> {
    let serving: Vec<&PeerInfo> = peers
        .iter()
        .filter(|p| {
            p.inference_port.is_some()
                && p.is_alive()
                && !p.is_tp_worker()
                && match deploy_id {
                    Some(id) => p.deploy_id == id,
                    None => p.model == model_name,
                }
        })
        .collect();

    // A REPLICA STILL LOADING is not a route while another one of the same
    // range is ready: it is alive and announces the model, but it has not
    // published its capacity yet, and a request planned through it waits (or
    // is refused) until it finishes loading. With no ready twin it stays: the
    // request then waits for it, as before.
    let ready_ranges: std::collections::HashSet<(u32, u32)> = serving
        .iter()
        .filter(|p| !p.is_warming())
        .map(|p| (p.start_layer, p.end_layer))
        .collect();
    let serving: Vec<&PeerInfo> = serving
        .into_iter()
        .filter(|p| !p.is_warming() || !ready_ranges.contains(&(p.start_layer, p.end_layer)))
        .collect();

    match deploy_id {
        Some(_) => serving,
        None => {
            let mut deployment_ids: Vec<&str> = serving
                .iter()
                .map(|p| p.deploy_id.as_str())
                .filter(|deployment| !deployment.is_empty())
                .collect();
            deployment_ids.sort_unstable();
            deployment_ids.dedup();
            match deployment_ids.first().copied() {
                None => serving,
                Some(selected) => serving
                    .into_iter()
                    .filter(|p| p.deploy_id == selected || p.deploy_id.is_empty())
                    .collect(),
            }
        }
    }
}

/// Heads are only the ranges that start at layer zero. The rest of the
/// topology stays private to the gateway; this cut is the only topology
/// the infer profile needs to know to start a request.
pub fn model_head_candidates<'a>(
    peers: &'a [PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
) -> Vec<&'a PeerInfo> {
    model_pipeline_candidates(peers, model_name, deploy_id)
        .into_iter()
        .filter(|peer| peer.start_layer == 0)
        .collect()
}

/// Same planner, with RTTs measured by the gateway that is about to originate
/// the request. Coverage is the first key, so a longer contiguous slice avoids
/// an extra handoff. Among equally useful slices, the score combines fresh RTT
/// (100 points per millisecond) and in-flight load (10 points per request).
/// Unknown RTT gets the maximum latency penalty and `node_id` breaks exact ties.
pub async fn plan_pipeline_with_rtt(
    peers: &[PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    tracker: &LoadTracker,
    rtts: &HashMap<String, f64>,
) -> Option<PipelinePlan> {
    plan_pipeline_with_rtt_from_head(
        peers,
        model_name,
        deploy_id,
        total_layers,
        tracker,
        rtts,
        None,
    )
    .await
}

/// Plan a pipeline while optionally pinning the peer that receives layer 0.
///
/// The UI may expose more than one complete-model head. When the caller has
/// explicitly picked one, that choice is authoritative for the first step;
/// RTT and load still select the remaining contiguous stages. An invalid or
/// unavailable choice fails closed instead of silently switching to a
/// different head and misleading the operator.
pub async fn plan_pipeline_with_rtt_from_head(
    peers: &[PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    tracker: &LoadTracker,
    rtts: &HashMap<String, f64>,
    preferred_head: Option<&str>,
) -> Option<PipelinePlan> {
    let serving = model_pipeline_candidates(peers, model_name, deploy_id);

    if serving.is_empty() {
        debug!(
            "Pipeline plan failed: No alive peers for model '{}'{} with inference_port",
            model_name,
            deploy_id
                .map(|d| format!(" (deployment {d})"))
                .unwrap_or_default()
        );
        return None;
    }

    // A pipeline cannot mix deployments. Without this, two deployments
    // of the same model - which is exactly what `deploy_id` now allows -
    // would become a single set of peers, and the layer by layer walk would choose
    // now from one, now from the other: half the prompt in one configuration, half in
    // the other, with the operator seeing a pipeline they did not deploy.
    //
    // When the caller did not choose, the choice is made here, and deterministically:
    // two machines of the cluster have to arrive at the same deployment. The order by
    // id is the stable criterion until the registry says which one is active
    // (`deployment::active`).
    let candidates = serving;

    let mut steps: Vec<PipelineStep> = Vec::new();
    let mut pos: u32 = 0; // next layer we need covered

    while pos < total_layers {
        // Only a peer whose loaded range starts at `pos` can be the next hop.
        // The inference engine enters at the first block of its loaded slice;
        // choosing a peer that merely covers `pos` would emit an execution
        // subrange beginning in the middle of that slice and fail after the
        // request had already crossed the network.
        let reachable: Vec<&PeerInfo> = candidates
            .iter()
            .copied()
            .filter(|p| p.start_layer == pos && p.end_layer >= pos)
            .collect();

        if reachable.is_empty() {
            info!(
                "Pipeline plan failed: Gap at layer {} for model '{}'",
                pos, model_name
            );
            return None; // gap — no peer covers layer `pos`
        }

        // Pick the candidate with the longest useful slice first. The second
        // key is a weighted speed penalty: a small RTT advantage can be lost
        // when its peer is carrying enough in-flight work.
        let mut best: Option<(&PeerInfo, i64, i64)> = None;
        for peer in reachable.iter().copied() {
            let load = tracker.load(&peer.node_id).await as i64;
            let layers = (peer.end_layer - pos + 1) as i64;
            let rtt_penalty = rtts
                .get(&peer.node_id)
                .map(|rtt| (*rtt * 100.0).max(0.0) as i64)
                .unwrap_or(100_000);
            let speed_penalty = rtt_penalty.min(99_999) + load * 10;
            if best
                .map(|(current, current_layers, current_penalty)| {
                    layers > current_layers
                        || (layers == current_layers
                            && (speed_penalty < current_penalty
                                || (speed_penalty == current_penalty
                                    && peer.node_id < current.node_id)))
                })
                .unwrap_or(true)
            {
                best = Some((peer, layers, speed_penalty));
            }
        }

        let chosen = if pos == 0 {
            match preferred_head {
                Some(preferred) => reachable
                    .iter()
                    .copied()
                    .find(|peer| peer.node_id == preferred),
                None => best.map(|(peer, _, _)| peer),
            }
        } else {
            best.map(|(peer, _, _)| peer)
        };

        let Some(chosen) = chosen else {
            // A requested head is only allowed to fail closed at layer zero.
            // Later gaps are the normal no-pipeline result handled below.
            return None;
        };

        steps.push(step_for(chosen, pos)?);

        pos = chosen.end_layer + 1;
    }

    Some(plan_from_steps(steps))
}

/// The step that executes `peer` starting at layer `pos`. `None` when the peer
/// has no inference address.
pub(crate) fn step_for(peer: &PeerInfo, pos: u32) -> Option<PipelineStep> {
    // `PeerInfo::inference_url` is the one place that knows how to turn an
    // address into a URL. Splitting `chosen.addr` on ':' here instead cut
    // "[2804:d55:...]:42777" down to "[2804", and the hop died inside
    // reqwest with nothing but "builder error" to show for it.
    let inference_url = peer.inference_url()?;
    Some(PipelineStep {
        node_id: peer.node_id.clone(),
        inference_url,
        inference_candidates: peer.inference_candidates(),
        quic_addr_candidates: peer.quic_addr_candidates(),
        start_layer: peer.start_layer,
        end_layer: peer.end_layer,
        // What is left to cover starting at `pos`, not the whole slice. When
        // the two coincide - the common case - nothing changes; when they do not, this
        // is where it gets recorded that the peer would have to enter mid-range,
        // instead of the recipient finding that out too late.
        exec_start: pos,
        exec_end: peer.end_layer,
        inference_frame_capabilities: peer.inference_frame_capabilities.clone(),
        endpoint_id: peer.endpoint_id.clone(),
        endpoint_addr_candidates: peer.endpoint_dialable_addrs(),
    })
}

/// The plan with the edges between contiguous steps.
pub(crate) fn plan_from_steps(steps: Vec<PipelineStep>) -> PipelinePlan {
    let edges = steps
        .windows(2)
        .filter(|pair| pair[1].exec_start == pair[0].exec_end.saturating_add(1))
        .map(|pair| PipelineEdge {
            from_node_id: pair[0].node_id.clone(),
            to_node_id: pair[1].node_id.clone(),
            from_end_layer: pair[0].exec_end,
            to_start_layer: pair[1].exec_start,
        })
        .collect();
    PipelinePlan { steps, edges }
}

/// Address through which to talk to this hop, decided by whoever is going to dial.
///
/// Three levels, in this order:
///
/// 1. the measured route, if it exists and is still an announced candidate;
/// 2. the `inference_url`, which comes from `peer.addr` - where that peer's
///    packets arrived from, and therefore the only *proven* address before the
///    first probe;
/// 3. the first announced candidate, when not even that exists.
///
/// The order of the last two used to be the reverse, and it cost requests: since
/// `merge_observed` appends the observed address to the **end** of the list, the
/// first candidate is usually the peer's LAN, and dialing it sends the
/// request to a `192.168.x.y` of another site. A guess does not go ahead
/// of a known fact; the measurement is what promotes a better candidate, later.
///
/// `pinned` is the session's memory: once chosen, that node's address
/// does not change until the session ends.
pub fn resolve_step_url(
    step: &PipelineStep,
    table: &crate::route::RouteTable,
    pinned: &mut std::collections::HashMap<String, String>,
) -> String {
    if let Some(already) = pinned.get(&step.node_id) {
        return already.clone();
    }
    let chosen = table
        .resolve(&step.node_id)
        .filter(|url| {
            step.inference_candidates.iter().any(|c| c == url)
                && !table.is_failed(&step.node_id, url)
        })
        .unwrap_or_else(|| {
            if !step.inference_url.is_empty()
                && !table.is_failed(&step.node_id, &step.inference_url)
            {
                step.inference_url.clone()
            } else {
                step.inference_candidates
                    .iter()
                    .find(|candidate| !table.is_failed(&step.node_id, candidate))
                    .cloned()
                    .unwrap_or_default()
            }
        });
    pinned.insert(step.node_id.clone(), chosen.clone());
    chosen
}

#[cfg(test)]
mod ipv6_tests {
    use super::*;
    use crate::network::PeerInfo;

    fn peer(addr: &str, start: u32, end: u32) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: "n".repeat(64),
            pubkey: "p".into(),
            cluster_id: None,
            trust_version: None,
            relay: None,
            addr: addr.into(),
            addr_candidates: Vec::new(),
            model: "qwen35".into(),
            start_layer: start,
            end_layer: end,
            total_layers: 32,
            memory_mb: 0.0,
            last_seen: chrono::Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            inference_quic_port: None,
            gateway_port: Some(8090),
            gateway_quic_port: None,
            profile: crate::profile::NodeProfile::Node,
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            inference_frame_capabilities: Vec::new(),
            endpoint_id: None,
            endpoint_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
        }
    }

    fn peer_tp(addr: &str, start: u32, end: u32, role: &str, rank: u32) -> PeerInfo {
        let mut p = peer(addr, start, end);
        p.tp = Some(crate::network::TpIdentity {
            role: role.into(),
            group_id: "g".into(),
            size: 2,
            rank,
            rank0_url: None,
            rank0_node_id: None,
            members: Vec::new(),
        });
        p
    }

    /// A tensor parallelism worker is not a pipeline hop: what enters the
    /// chain is the group's primary, even when covering the same range.
    #[tokio::test]
    async fn a_tp_worker_is_not_a_pipeline_hop() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let worker = peer_tp("192.168.1.10:42777", 0, 31, "worker", 1);
        let mut primary = peer_tp("192.168.1.11:42777", 0, 31, "primary", 0);
        primary.node_id = "primary".into();
        let tracker = LoadTracker::new();
        let plan = plan_pipeline(&[worker, primary], "qwen35", None, 32, &tracker)
            .await
            .expect("plan");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].node_id, "primary");
    }

    /// Two deployments of the **same model**, in the same cluster.
    ///
    /// It is the case that did not exist: before `deploy_id` the four peers were a
    /// single set (all with `model == "qwen35"`), and the pipeline came out with
    /// half the layers from one deployment and half from the other - without anything
    /// or anyone being able to notice.
    #[tokio::test]
    async fn two_deployments_of_one_model_never_mix() {
        // The ranges cross on purpose: the greedy walk per layer
        // prefers whoever covers more layers, so without grouping it would take
        // `a1` (0-19, the largest at 0) and then `b2` (16-31, the largest at 20) -
        // a pipeline with one foot in each deployment.
        let mut a1 = peer("10.0.0.1:42777", 0, 19);
        let mut a2 = peer("10.0.0.2:42777", 20, 31);
        let mut b1 = peer("10.0.0.3:42777", 0, 15);
        let mut b2 = peer("10.0.0.4:42777", 16, 31);
        // The test `peer` gives the same `node_id` to everyone; here they
        // need to be distinguishable, which is the point of the test.
        for (p, id, dep) in [
            (&mut a1, "a1", "d-aaaaaaaa"),
            (&mut a2, "a2", "d-aaaaaaaa"),
            (&mut b1, "b1", "d-bbbbbbbb"),
            (&mut b2, "b2", "d-bbbbbbbb"),
        ] {
            p.node_id = id.to_string();
            p.deploy_id = dep.to_string();
        }
        let all = vec![a1, a2, b1, b2];
        let tracker = LoadTracker::new();

        // Asking for one deployment, only its machines get in.
        let plan = plan_pipeline(&all, "qwen35", Some("d-aaaaaaaa"), 32, &tracker)
            .await
            .expect("deployment A covers the whole model");
        assert_eq!(plan.steps.len(), 2);
        assert!(
            plan.steps
                .iter()
                .all(|s| s.inference_url.contains("10.0.0.1")
                    || s.inference_url.contains("10.0.0.2")),
            "no hop can come from deployment B: {:?}",
            plan.steps
                .iter()
                .map(|s| &s.inference_url)
                .collect::<Vec<_>>()
        );

        let plan_b = plan_pipeline(&all, "qwen35", Some("d-bbbbbbbb"), 32, &tracker)
            .await
            .unwrap();
        assert!(plan_b
            .steps
            .iter()
            .all(|s| s.inference_url.contains("10.0.0.3") || s.inference_url.contains("10.0.0.4")));

        // Without choosing, they still do not mix: one is chosen, whole.
        let any = plan_pipeline(&all, "qwen35", None, 32, &tracker)
            .await
            .unwrap();
        let ids: std::collections::BTreeSet<&str> = any
            .steps
            .iter()
            .map(|s| {
                all.iter()
                    .find(|p| p.node_id == s.node_id)
                    .map(|p| p.deploy_id.as_str())
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(ids.len(), 1, "one pipeline, one deployment: {ids:?}");
    }

    /// Two **distinct** models loaded at the same time, each on its
    /// own range of peers. `model_pipeline_candidates` filters by
    /// exact label equality, so asking for one of the two names must never
    /// mix in hops of the other - it is the mechanism that already gives the client the
    /// choice between loaded models, as long as `resolve_model` (HTTP layer)
    /// validates the name against `GET /v1/models` instead of silently swapping it for
    /// the active one.
    #[tokio::test]
    async fn two_distinct_models_route_independently() {
        let mut a1 = peer("10.0.0.1:42777", 0, 15);
        let mut a2 = peer("10.0.0.2:42777", 16, 31);
        let mut b1 = peer("10.0.0.3:42777", 0, 31);
        for (p, id, model) in [
            (&mut a1, "a1", "modelo-a"),
            (&mut a2, "a2", "modelo-a"),
            (&mut b1, "b1", "modelo-b"),
        ] {
            p.node_id = id.to_string();
            p.model = model.to_string();
        }
        let all = vec![a1, a2, b1];
        let tracker = LoadTracker::new();

        let plan_a = plan_pipeline(&all, "modelo-a", None, 32, &tracker)
            .await
            .expect("modelo-a covers 0..31 in two hops");
        assert_eq!(plan_a.steps.len(), 2);
        assert!(plan_a
            .steps
            .iter()
            .all(|s| s.node_id == "a1" || s.node_id == "a2"));

        let plan_b = plan_pipeline(&all, "modelo-b", None, 32, &tracker)
            .await
            .expect("modelo-b covers 0..31 in a single hop");
        assert_eq!(plan_b.steps.len(), 1);
        assert_eq!(plan_b.steps[0].node_id, "b1");

        let candidates_a = model_pipeline_candidates(&all, "modelo-a", None);
        assert!(candidates_a.iter().all(|p| p.model == "modelo-a"));
        let candidates_b = model_pipeline_candidates(&all, "modelo-b", None);
        assert!(candidates_b.iter().all(|p| p.model == "modelo-b"));
    }

    /// An id nobody serves cannot fall back to the label and return the
    /// other deployment silently - that would be serving on a configuration the client
    /// explicitly did not ask for.
    #[tokio::test]
    async fn an_unknown_deployment_id_serves_nobody() {
        let mut p1 = peer("10.0.0.1:42777", 0, 31);
        p1.deploy_id = "d-aaaaaaaa".into();
        let plan =
            plan_pipeline(&[p1], "qwen35", Some("d-cccccccc"), 32, &LoadTracker::new()).await;
        assert!(plan.is_none());
    }

    /// Cluster being updated: one peer already announces a deployment, the other does
    /// not yet. The one that does not announce keeps serving the label, otherwise the
    /// one node at a time update would open a hole of layers.
    #[tokio::test]
    async fn a_peer_without_a_deployment_still_serves_its_label() {
        let mut new = peer("10.0.0.1:42777", 0, 15);
        new.deploy_id = "d-aaaaaaaa".into();
        let old = peer("10.0.0.2:42777", 16, 31); // empty deploy_id

        let plan = plan_pipeline(&[new, old], "qwen35", None, 32, &LoadTracker::new())
            .await
            .expect("the two together cover the model");
        assert_eq!(plan.steps.len(), 2);
    }

    #[tokio::test]
    async fn planner_follows_an_exact_subrange_entry_instead_of_entering_mid_slice() {
        let mut head = peer("10.0.0.1:42777", 0, 3);
        let mut overlapping = peer("10.0.0.2:42777", 2, 15);
        let mut exact = peer("10.0.0.3:42777", 4, 15);
        head.node_id = "head".into();
        overlapping.node_id = "overlapping".into();
        exact.node_id = "exact".into();

        let plan = plan_pipeline(
            &[head, overlapping, exact],
            "qwen35",
            None,
            16,
            &LoadTracker::new(),
        )
        .await
        .expect("the contiguous coverage exists");

        assert_eq!(
            plan.steps
                .iter()
                .map(|step| step.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["head", "exact"]
        );
        assert_eq!((plan.steps[1].exec_start, plan.steps[1].exec_end), (4, 15));
    }

    #[tokio::test]
    async fn equivalent_entry_branches_choose_the_lowest_gateway_rtt() {
        let mut slow = peer("10.0.0.1:42777", 0, 7);
        let mut fast = peer("10.0.0.2:42777", 0, 7);
        let mut tail = peer("10.0.0.3:42777", 8, 15);
        slow.node_id = "slow-entry".into();
        fast.node_id = "fast-entry".into();
        tail.node_id = "tail".into();

        let mut rtts = HashMap::new();
        rtts.insert("slow-entry".into(), 25.0);
        rtts.insert("fast-entry".into(), 3.0);

        let plan = plan_pipeline_with_rtt(
            &[slow, fast, tail],
            "qwen35",
            None,
            16,
            &LoadTracker::new(),
            &rtts,
        )
        .await
        .expect("both entries cover the same branch");

        assert_eq!(plan.steps[0].node_id, "fast-entry");
    }

    #[tokio::test]
    async fn planner_honors_an_explicit_inference_head() {
        let mut score_winner = peer("10.0.0.1:42777", 0, 7);
        let mut requested = peer("10.0.0.2:42777", 0, 7);
        let mut tail = peer("10.0.0.3:42777", 8, 15);
        score_winner.node_id = "score-winner".into();
        requested.node_id = "requested-head".into();
        tail.node_id = "tail".into();

        let rtts = HashMap::from([
            (String::from("score-winner"), 1.0),
            (String::from("requested-head"), 80.0),
        ]);
        let plan = plan_pipeline_with_rtt_from_head(
            &[score_winner, requested, tail],
            "qwen35",
            None,
            16,
            &LoadTracker::new(),
            &rtts,
            Some("requested-head"),
        )
        .await
        .expect("the chosen head still covers the pipeline");

        assert_eq!(plan.steps[0].node_id, "requested-head");
        assert_eq!(plan.steps[1].node_id, "tail");
    }

    #[tokio::test]
    async fn a_busy_low_rtt_entry_can_lose_to_a_quieter_entry() {
        let mut busy = peer("10.0.0.1:42777", 0, 7);
        let mut quiet = peer("10.0.0.2:42777", 0, 7);
        let mut tail = peer("10.0.0.3:42777", 8, 15);
        busy.node_id = "busy-entry".into();
        quiet.node_id = "quiet-entry".into();
        tail.node_id = "tail".into();

        let tracker = LoadTracker::new();
        let mut busy_requests = Vec::new();
        for _ in 0..20 {
            busy_requests.push(tracker.acquire("busy-entry").await);
        }
        let rtts = HashMap::from([
            (String::from("busy-entry"), 1.0),
            (String::from("quiet-entry"), 2.0),
        ]);

        let plan =
            plan_pipeline_with_rtt(&[busy, quiet, tail], "qwen35", None, 16, &tracker, &rtts)
                .await
                .expect("both entries cover the same branch");

        assert_eq!(plan.steps[0].node_id, "quiet-entry");
        drop(busy_requests);
    }

    #[test]
    fn peers_without_a_compression_capability_keep_the_v1_f16_wire_format() {
        let step = PipelineStep {
            node_id: "old".into(),
            inference_url: "http://127.0.0.1:8181".into(),
            inference_candidates: Vec::new(),
            quic_addr_candidates: Vec::new(),
            start_layer: 0,
            end_layer: 1,
            exec_start: 0,
            exec_end: 1,
            inference_frame_capabilities: Vec::new(),
            endpoint_id: None,
            endpoint_addr_candidates: Vec::new(),
        };
        assert_eq!(step.frame_encoding(), crate::wire::FrameEncoding::F16);
    }

    #[test]
    fn f8_is_refused_when_any_pipeline_step_lacks_the_capability() {
        let mut missing = step(&[]);
        missing.node_id = "old-peer".into();
        let error = plan_of(vec![step(&[CAP_FRAME_DTYPE_F8]), missing])
            .validate_wire_dtype(crate::wire::WireDtype::F8)
            .unwrap_err();
        let error = error.to_string();
        assert!(error.contains("old-peer"), "unexpected error: {error}");
        assert!(error.contains("f8"), "unexpected error: {error}");
    }

    #[test]
    fn f8_is_accepted_when_every_pipeline_step_advertises_it() {
        let plan = plan_of(vec![
            step(&[CAP_FRAME_DTYPE_F8]),
            step(&[CAP_FRAME_DTYPE_F8]),
        ]);
        assert!(plan.validate_wire_dtype(crate::wire::WireDtype::F8).is_ok());
        assert!(plan
            .validate_wire_dtype(crate::wire::WireDtype::F16)
            .is_ok());
        assert!(plan
            .validate_wire_dtype(crate::wire::WireDtype::F32)
            .is_ok());
    }

    #[test]
    fn groupwise_i4_requires_its_exact_capability_on_every_hop() {
        let plan = plan_of(vec![
            step(&[CAP_FRAME_DTYPE_I4G64]),
            step(&[CAP_FRAME_DTYPE_I4G128]),
        ]);
        assert!(plan
            .validate_wire_dtype(crate::wire::WireDtype::I4G64)
            .is_err());
        assert!(plan
            .validate_wire_dtype(crate::wire::WireDtype::I4G128)
            .is_err());

        let plan = plan_of(vec![
            step(&[CAP_FRAME_DTYPE_I4G64]),
            step(&[CAP_FRAME_DTYPE_I4G64]),
        ]);
        assert!(plan
            .validate_wire_dtype(crate::wire::WireDtype::I4G64)
            .is_ok());
    }

    #[test]
    fn route_affinity_keeps_the_selected_replica_for_the_whole_session() {
        let plan = PipelinePlan {
            steps: vec![
                PipelineStep {
                    node_id: "replica-a".into(),
                    inference_url: "http://127.0.0.1:8181".into(),
                    inference_candidates: Vec::new(),
                    quic_addr_candidates: Vec::new(),
                    start_layer: 0,
                    end_layer: 7,
                    exec_start: 0,
                    exec_end: 7,
                    inference_frame_capabilities: Vec::new(),
                    endpoint_id: None,
                    endpoint_addr_candidates: Vec::new(),
                },
                PipelineStep {
                    node_id: "replica-b".into(),
                    inference_url: "http://127.0.0.1:8182".into(),
                    inference_candidates: Vec::new(),
                    quic_addr_candidates: Vec::new(),
                    start_layer: 8,
                    end_layer: 15,
                    exec_start: 8,
                    exec_end: 15,
                    inference_frame_capabilities: Vec::new(),
                    endpoint_id: None,
                    endpoint_addr_candidates: Vec::new(),
                },
            ],
            edges: Vec::new(),
        };
        let affinity = RouteAffinity::from_plan("deploy-1", "session-1", &plan);
        assert_eq!(affinity.hop_replicas, vec!["replica-a", "replica-b"]);
        assert!(affinity.matches_plan(&plan));

        let mut changed = plan.clone();
        changed.steps[0].node_id = "replica-c".into();
        assert!(!affinity.matches_plan(&changed));
    }

    fn step_with(candidates: Vec<&str>, url: &str) -> PipelineStep {
        PipelineStep {
            node_id: "n1".into(),
            inference_url: url.into(),
            inference_candidates: candidates.into_iter().map(String::from).collect(),
            quic_addr_candidates: Vec::new(),
            start_layer: 0,
            end_layer: 9,
            exec_start: 0,
            exec_end: 9,
            inference_frame_capabilities: Vec::new(),
            endpoint_id: None,
            endpoint_addr_candidates: Vec::new(),
        }
    }

    /// A step without candidates is a node running an earlier binary. Falling back to
    /// `inference_url` is what keeps a cluster of mixed versions from
    /// splitting in half.
    #[test]
    fn a_step_without_candidates_falls_back_to_the_resolved_url() {
        let table = crate::route::RouteTable::new();
        let mut pinned = std::collections::HashMap::new();
        let step = step_with(vec![], "http://192.168.1.9:8181");
        assert_eq!(
            resolve_step_url(&step, &table, &mut pinned),
            "http://192.168.1.9:8181",
        );
    }

    /// The session's choice does not change midway: the KV cache on the other side is
    /// per session, and switching address halfway is worse than the slow route.
    #[test]
    fn the_route_is_pinned_for_the_whole_session() {
        let table = crate::route::RouteTable::new();
        let mut pinned = std::collections::HashMap::new();
        pinned.insert("n1".to_string(), "http://escolhido:8181".to_string());
        let step = step_with(vec!["http://outro:8181"], "http://legado:8181");
        assert_eq!(
            resolve_step_url(&step, &table, &mut pinned),
            "http://escolhido:8181",
        );
    }

    /// Without a measurement, the address through which the peer already talked to us holds.
    ///
    /// `merge_observed` appends the packet's source address **to the end** of the
    /// list, so the first candidate is what the peer announced of itself - in
    /// general its LAN. Dialing it before measuring sends the request to a
    /// `192.168.x.y` of another site, which is unreachable from here: on 2026-08-14 the
    /// log showed three attempts on `http://192.168.0.37:8181/v1/prefill`
    /// for a peer that only answers through the global IPv6.
    ///
    /// The `inference_url` is not a guess: it comes from `peer.addr`, which is
    /// where that peer's packets arrived from. It is the only proven address
    /// we have before the first probe.
    #[test]
    fn without_a_measurement_the_observed_address_wins_over_an_announced_one() {
        let table = crate::route::RouteTable::new();
        let mut pinned = std::collections::HashMap::new();
        let step = step_with(
            vec!["http://192.168.0.37:8181", "http://[2804:f30::1]:8181"],
            "http://[2804:f30::1]:8181",
        );
        assert_eq!(
            resolve_step_url(&step, &table, &mut pinned),
            "http://[2804:f30::1]:8181",
        );
    }

    /// Measured wins: that is the point of the dynamic route phase.
    #[test]
    fn a_measured_route_beats_the_observed_address() {
        let table = crate::route::RouteTable::new();
        table.remember_for_test("n1", "http://rapido:8181", 3.0);
        let mut pinned = std::collections::HashMap::new();
        let step = step_with(
            vec!["http://rapido:8181", "http://lento:8181"],
            "http://lento:8181",
        );
        assert_eq!(
            resolve_step_url(&step, &table, &mut pinned),
            "http://rapido:8181"
        );
    }

    #[test]
    fn a_failed_route_is_not_selected_again_when_another_address_exists() {
        let table = crate::route::RouteTable::new();
        table.remember_for_test("n1", "http://blackhole:8181", 1.0);
        table.mark_failure("n1", "http://blackhole:8181");
        let mut pinned = std::collections::HashMap::new();
        let step = step_with(
            vec!["http://blackhole:8181", "http://healthy:8181"],
            "http://blackhole:8181",
        );

        assert_eq!(
            resolve_step_url(&step, &table, &mut pinned),
            "http://healthy:8181",
            "a real failure must remove the address from the next session"
        );
    }

    fn step(caps: &[&str]) -> PipelineStep {
        PipelineStep {
            node_id: "n".into(),
            inference_url: "http://127.0.0.1:8181".into(),
            inference_candidates: Vec::new(),
            quic_addr_candidates: Vec::new(),
            start_layer: 0,
            end_layer: 1,
            exec_start: 0,
            exec_end: 1,
            inference_frame_capabilities: caps.iter().map(|c| c.to_string()).collect(),
            endpoint_id: None,
            endpoint_addr_candidates: Vec::new(),
        }
    }

    fn plan_of(steps: Vec<PipelineStep>) -> PipelinePlan {
        PipelinePlan {
            steps,
            edges: Vec::new(),
        }
    }

    /// The MTP requirement falls on the **tail**, which has the `nextn` block. All need
    /// to know how to decode a batch, because the draft travels to the head.
    #[test]
    fn mtp_needs_the_tail_to_have_nextn_and_everyone_to_batch() {
        let with_mtp = &[
            CAP_SPECULATE_LOOKUP,
            CAP_SPECULATE_MTP,
            CAP_SPECULATE_MTP_SLICED,
        ][..];
        let batch_only = &[CAP_SPECULATE_LOOKUP, CAP_SPECULATE_MTP_SLICED][..];
        let old = &[CAP_SPECULATE_LOOKUP][..];

        assert!(
            plan_of(vec![step(with_mtp)]).supports_mtp(),
            "a single machine"
        );
        assert!(
            plan_of(vec![step(batch_only), step(with_mtp)]).supports_mtp(),
            "the tail has the block — it is the one that drafts, the gateway takes the draft to the head"
        );
        assert!(
            !plan_of(vec![step(with_mtp), step(batch_only)]).supports_mtp(),
            "the tail without the block cannot draft"
        );
        assert!(
            !plan_of(vec![step(with_mtp), step(&[])]).supports_mtp(),
            "a hop that does not decode a batch breaks the whole round"
        );
        assert!(
            !plan_of(vec![step(batch_only), step(old)]).supports_mtp(),
            "an old peer on the ring does not understand the new speculate — the new cap is required on all"
        );
        assert!(!plan_of(vec![]).supports_mtp());
    }

    #[test]
    fn relay_chain_requires_every_step_to_advertise_it() {
        assert!(
            plan_of(vec![step(&[CAP_RELAY_CHAIN]), step(&[CAP_RELAY_CHAIN])])
                .supports_relay_chain()
        );
        assert!(!plan_of(vec![step(&[CAP_RELAY_CHAIN]), step(&[])]).supports_relay_chain());
        assert!(!plan_of(vec![]).supports_relay_chain());
    }

    #[test]
    fn direct_reply_requires_every_step_and_more_than_one_hop() {
        assert!(
            plan_of(vec![step(&[CAP_RELAY_DIRECT]), step(&[CAP_RELAY_DIRECT])])
                .supports_direct_reply()
        );
        // An old step in the middle brings the whole chain down to the ring.
        assert!(
            !plan_of(vec![step(&[CAP_RELAY_DIRECT]), step(&[CAP_RELAY_CHAIN])])
                .supports_direct_reply()
        );
        // A single hop is already the minimum; diverting the response saves no leg.
        assert!(!plan_of(vec![step(&[CAP_RELAY_DIRECT])]).supports_direct_reply());
        assert!(!plan_of(vec![]).supports_direct_reply());
    }

    #[test]
    fn speculation_requires_every_step_to_advertise_it() {
        assert!(plan_of(vec![
            step(&[CAP_SPECULATE_LOOKUP]),
            step(&[CAP_SPECULATE_LOOKUP])
        ])
        .supports_speculation());
        assert!(!plan_of(vec![step(&[CAP_SPECULATE_LOOKUP]), step(&[])]).supports_speculation());
        assert!(!plan_of(vec![]).supports_speculation());
    }

    #[test]
    fn routing_request_from_peers_preserves_peer_ranges() {
        let peers = vec![
            peer("192.168.1.10:42777", 0, 15),
            peer("192.168.1.11:42777", 16, 31),
        ];

        let request = routing_request_from_peers(32, &peers);

        assert_eq!(request.api_version, routing_contract::ROUTING_API_VERSION);
        assert_eq!(request.cluster.total_layers, 32);
        assert_eq!(request.cluster.peers[0].peer_id, peers[0].node_id);
        assert_eq!(request.cluster.peers[0].start_layer, 0);
        assert_eq!(request.cluster.peers[0].end_layer, 15);
        assert!(request.cluster.peers[0].supports_inference);
        assert_eq!(request.cluster.peers[1].start_layer, 16);
        assert_eq!(request.cluster.peers[1].end_layer, 31);
    }

    /// The plan's step has to become a real URL: with IPv6 the address was
    /// cut at the first ':' and the hop died with "builder error" in reqwest.
    #[tokio::test]
    async fn an_ipv6_peer_becomes_a_valid_step_url() {
        // The URL's scheme comes from a global state of the process
        // (`peer_client::tls_enabled`), and the tests run in parallel. Without this
        // lock, a test that turns the cluster's policy on makes this one see
        // `https://` where it expects `http://`.
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let peers = vec![peer(
            "[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777",
            0,
            31,
        )];
        let tracker = LoadTracker::new();
        let plan = plan_pipeline(&peers, "qwen35", None, 32, &tracker)
            .await
            .expect("plan");
        assert_eq!(
            plan.steps[0].inference_url,
            "http://[2804:d55:5579:1000:89f3:3f94:7c07:e183]:8181"
        );
        let url = format!("{}/v1/prefill", plan.steps[0].inference_url);
        assert!(url.parse::<reqwest::Url>().is_ok(), "invalid URL: {url}");
    }

    #[tokio::test]
    async fn an_ipv4_peer_is_unchanged() {
        let _guard = crate::peer_client::GLOBAL_POLICY.lock();
        let peers = vec![peer("192.168.100.138:42777", 0, 31)];
        let tracker = LoadTracker::new();
        let plan = plan_pipeline(&peers, "qwen35", None, 32, &tracker)
            .await
            .expect("plan");
        assert_eq!(plan.steps[0].inference_url, "http://192.168.100.138:8181");
    }

    #[tokio::test]
    async fn an_exact_replica_prefers_the_lower_rtt() {
        let mut slow = peer("192.168.1.10:42777", 0, 31);
        slow.node_id = "slow".into();
        let mut fast = peer("192.168.1.11:42777", 0, 31);
        fast.node_id = "fast".into();
        let rtts = HashMap::from([(String::from("slow"), 80.0), (String::from("fast"), 12.0)]);

        let plan = plan_pipeline_with_rtt(
            &[slow, fast],
            "qwen35",
            None,
            32,
            &LoadTracker::new(),
            &rtts,
        )
        .await
        .expect("plan");

        assert_eq!(plan.steps[0].node_id, "fast");
    }

    #[test]
    fn head_candidates_are_only_live_layer_zero_inference_peers() {
        let mut head = peer("head", 0, 15);
        head.node_id = "head".into();
        let mut middle = peer("middle", 16, 31);
        middle.node_id = "middle".into();
        let mut dead = peer("dead", 0, 15);
        dead.node_id = "dead".into();
        dead.last_seen = chrono::Utc::now() - chrono::Duration::seconds(60);

        let peers = [head, middle, dead];
        let candidates = model_head_candidates(&peers, "qwen35", None);

        assert_eq!(
            candidates
                .iter()
                .map(|p| p.node_id.as_str())
                .collect::<Vec<_>>(),
            ["head"]
        );
    }

    #[test]
    fn mixed_deployment_exposes_whole_and_sliced_heads() {
        let mut whole = peer("whole", 0, 31);
        whole.node_id = "whole".into();
        whole.deploy_id = "d-aaaaaaaa".into();
        let mut shard_head = peer("shard-head", 0, 15);
        shard_head.node_id = "shard-head".into();
        shard_head.deploy_id = "d-aaaaaaaa".into();
        let mut shard_tail = peer("shard-tail", 16, 31);
        shard_tail.node_id = "shard-tail".into();
        shard_tail.deploy_id = "d-aaaaaaaa".into();

        let peers = [whole, shard_head, shard_tail];
        let heads = model_head_candidates(&peers, "qwen35", Some("d-aaaaaaaa"));
        let ids = heads
            .iter()
            .map(|peer| peer.node_id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["whole", "shard-head"]);
    }

    #[tokio::test]
    async fn plan_exposes_handoffs_for_the_selected_steps() {
        let mut first = peer("192.168.1.10:42777", 0, 15);
        first.node_id = "first".into();
        let mut second = peer("192.168.1.11:42777", 16, 31);
        second.node_id = "second".into();

        let plan = plan_pipeline(&[first, second], "qwen35", None, 32, &LoadTracker::new())
            .await
            .expect("plan");

        assert_eq!(
            plan.edges,
            vec![PipelineEdge {
                from_node_id: "first".into(),
                to_node_id: "second".into(),
                from_end_layer: 15,
                to_start_layer: 16,
            }]
        );
    }
}
