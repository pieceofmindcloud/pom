/// Pipeline planner for distributed layer-pipeline inference.
///
/// Given a set of peers (each covering a contiguous layer range), this module
/// selects the optimal ordered sequence of peers that covers layers 0..total_layers,
/// tracks per-peer request load, and exposes the plan to the gateway.
use crate::network::PeerInfo;
use crate::wire::FrameEncoding;
use routing_contract::{ClusterSnapshot, PeerCapacity, RoutingRequest, ROUTING_API_VERSION};
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
#[derive(Debug, Clone)]
pub struct PipelineStep {
    pub node_id: String,
    /// Full HTTP base URL, e.g. "http://192.168.1.5:8181"
    pub inference_url: String,
    pub start_layer: u32,
    pub end_layer: u32,
    /// Advertised receiver capabilities. An absent v2 signal must remain on
    /// the v1 f16 contract so rolling upgrades keep working.
    pub inference_frame_capabilities: Vec<String>,
}

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
        if self
            .inference_frame_capabilities
            .iter()
            .any(|cap| cap == "frame/v2-zstd-f16")
        {
            FrameEncoding::ZstdF16
        } else {
            FrameEncoding::F16
        }
    }
}

/// Ordered pipeline plan for a single inference request.
#[derive(Debug, Clone)]
pub struct PipelinePlan {
    /// Steps in order from first layer to last.
    pub steps: Vec<PipelineStep>,
    /// Handoffs selected by the same planner invocation as `steps`.
    pub edges: Vec<PipelineEdge>,
    pub total_layers: u32,
}

/// Translates transport-owned peer records into the stable routing-provider
/// input. Production routing remains on the existing planner until equivalence
/// coverage permits switching implementations.
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
    /// Comma-separated "ip:port" string for llama.cpp `--rpc` flag.
    pub fn rpc_workers_string(&self) -> String {
        self.steps
            .iter()
            .map(|s| s.inference_url.trim_start_matches("http://").to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Human-readable hop chain: "abc123→def456→…"
    pub fn hop_chain(&self) -> String {
        self.steps
            .iter()
            .map(|s| s.node_id[..s.node_id.len().min(8)].to_string())
            .collect::<Vec<_>>()
            .join("→")
    }
}

// ── Planner ───────────────────────────────────────────────────────────────────

/// Select the optimal inference pipeline for a given model.
///
/// Scoring: `layers_count * 100 - current_load * 10`
/// — prefers peers that cover more layers and are less busy.
///
/// Returns `None` if there is any gap in layer coverage (0..total_layers).
pub async fn plan_pipeline(
    peers: &[PeerInfo],
    model_name: &str,
    total_layers: u32,
    tracker: &LoadTracker,
) -> Option<PipelinePlan> {
    // Only peers that expose an inference server AND match the model name
    let candidates: Vec<&PeerInfo> = peers
        .iter()
        .filter(|p| p.inference_port.is_some() && p.is_alive() && p.model == model_name)
        .collect();

    if candidates.is_empty() {
        debug!(
            "Pipeline plan failed: No alive peers for model '{}' with inference_port",
            model_name
        );
        return None;
    }

    let mut steps: Vec<PipelineStep> = Vec::new();
    let mut pos: u32 = 0; // next layer we need covered

    while pos < total_layers {
        // Peers whose range includes `pos`
        let reachable: Vec<&PeerInfo> = candidates
            .iter()
            .copied()
            .filter(|p| p.start_layer <= pos && p.end_layer >= pos)
            .collect();

        if reachable.is_empty() {
            info!(
                "Pipeline plan failed: Gap at layer {} for model '{}'",
                pos, model_name
            );
            return None; // gap — no peer covers layer `pos`
        }

        // Pick the best-scoring candidate
        let mut best: Option<(&PeerInfo, i64)> = None;
        for peer in reachable {
            let load = tracker.load(&peer.node_id).await as i64;
            let layers = (peer.end_layer - peer.start_layer + 1) as i64;
            let score = layers * 100 - load * 10;
            if best.map(|(_, s)| score > s).unwrap_or(true) {
                best = Some((peer, score));
            }
        }

        let chosen = best.unwrap().0;

        // `PeerInfo::inference_url` is the one place that knows how to turn an
        // address into a URL. Splitting `chosen.addr` on ':' here instead cut
        // "[2804:d55:...]:42777" down to "[2804", and the hop died inside
        // reqwest with nothing but "builder error" to show for it.
        let Some(inference_url) = chosen.inference_url() else {
            return None;
        };

        steps.push(PipelineStep {
            node_id: chosen.node_id.clone(),
            inference_url,
            start_layer: chosen.start_layer,
            end_layer: chosen.end_layer,
            inference_frame_capabilities: chosen.inference_frame_capabilities.clone(),
        });

        pos = chosen.end_layer + 1;
    }

    let edges = steps
        .windows(2)
        .map(|pair| PipelineEdge {
            from_node_id: pair[0].node_id.clone(),
            to_node_id: pair[1].node_id.clone(),
            from_end_layer: pair[0].end_layer,
            to_start_layer: pair[1].start_layer,
        })
        .collect();

    Some(PipelinePlan {
        steps,
        edges,
        total_layers,
    })
}

#[cfg(test)]
mod ipv6_tests {
    use super::*;
    use crate::network::PeerInfo;

    fn peer(addr: &str, start: u32, end: u32) -> PeerInfo {
        PeerInfo {
            node_id: "n".repeat(64),
            pubkey: "p".into(),
            addr: addr.into(),
            model: "qwen35".into(),
            start_layer: start,
            end_layer: end,
            total_layers: 32,
            memory_mb: 0.0,
            last_seen: chrono::Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            gateway_port: Some(8090),
            role: String::new(),
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            inference_frame_capabilities: Vec::new(),
        }
    }

    #[test]
    fn peers_without_a_compression_capability_keep_the_v1_f16_wire_format() {
        let step = PipelineStep {
            node_id: "old".into(),
            inference_url: "http://127.0.0.1:8181".into(),
            start_layer: 0,
            end_layer: 1,
            inference_frame_capabilities: Vec::new(),
        };
        assert_eq!(step.frame_encoding(), crate::wire::FrameEncoding::F16);
    }

    #[test]
    fn peers_advertising_v2_receive_compressed_frames() {
        let step = PipelineStep {
            node_id: "new".into(),
            inference_url: "http://127.0.0.1:8181".into(),
            start_layer: 0,
            end_layer: 1,
            inference_frame_capabilities: vec!["frame/v2-zstd-f16".into()],
        };
        assert_eq!(step.frame_encoding(), crate::wire::FrameEncoding::ZstdF16);
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

    /// O passo do plano tem que virar URL de verdade: com IPv6 o endereco era
    /// cortado no primeiro ':' e o hop morria com "builder error" no reqwest.
    #[tokio::test]
    async fn an_ipv6_peer_becomes_a_valid_step_url() {
        let peers = vec![peer(
            "[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777",
            0,
            31,
        )];
        let tracker = LoadTracker::new();
        let plan = plan_pipeline(&peers, "qwen35", 32, &tracker)
            .await
            .expect("plano");
        assert_eq!(
            plan.steps[0].inference_url,
            "http://[2804:d55:5579:1000:89f3:3f94:7c07:e183]:8181"
        );
        let url = format!("{}/v1/prefill", plan.steps[0].inference_url);
        assert!(url.parse::<reqwest::Url>().is_ok(), "URL invalida: {url}");
    }

    #[tokio::test]
    async fn an_ipv4_peer_is_unchanged() {
        let peers = vec![peer("192.168.100.138:42777", 0, 31)];
        let tracker = LoadTracker::new();
        let plan = plan_pipeline(&peers, "qwen35", 32, &tracker)
            .await
            .expect("plano");
        assert_eq!(plan.steps[0].inference_url, "http://192.168.100.138:8181");
    }

    #[tokio::test]
    async fn plan_exposes_handoffs_for_the_selected_steps() {
        let mut first = peer("192.168.1.10:42777", 0, 15);
        first.node_id = "first".into();
        let mut second = peer("192.168.1.11:42777", 16, 31);
        second.node_id = "second".into();

        let plan = plan_pipeline(&[first, second], "qwen35", 32, &LoadTracker::new())
            .await
            .expect("plano");

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
