/// Pipeline planner for distributed layer-pipeline inference.
///
/// Given a set of peers (each covering a contiguous layer range), this module
/// selects the optimal ordered sequence of peers that covers layers 0..total_layers,
/// tracks per-peer request load, and exposes the plan to the gateway.
use crate::network::PeerInfo;
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
}

/// Ordered pipeline plan for a single inference request.
#[derive(Debug, Clone)]
pub struct PipelinePlan {
    /// Steps in order from first layer to last.
    pub steps: Vec<PipelineStep>,
    pub total_layers: u32,
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
        debug!("Pipeline plan failed: No alive peers for model '{}' with inference_port", model_name);
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
            info!("Pipeline plan failed: Gap at layer {} for model '{}'", pos, model_name);
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
        });

        pos = chosen.end_layer + 1;
    }

    Some(PipelinePlan { steps, total_layers })
}

#[cfg(test)]
mod ipv6_tests {
    use super::*;
    use crate::network::PeerInfo;

    fn peer(addr: &str, start: u32, end: u32) -> PeerInfo {
        PeerInfo {
            node_id: "n".repeat(64), pubkey: "p".into(), addr: addr.into(),
            model: "qwen35".into(), start_layer: start, end_layer: end, total_layers: 32,
            memory_mb: 0.0, last_seen: chrono::Utc::now(),
            ollama_port: None, inference_port: Some(8181), gateway_port: Some(8090),
            role: String::new(), hostname: String::new(), connection_verified: false,
            disk_total_mb: 0, disk_free_mb: 0, api_keys_active: 0,
        }
    }

    /// O passo do plano tem que virar URL de verdade: com IPv6 o endereco era
    /// cortado no primeiro ':' e o hop morria com "builder error" no reqwest.
    #[tokio::test]
    async fn an_ipv6_peer_becomes_a_valid_step_url() {
        let peers = vec![peer("[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777", 0, 31)];
        let tracker = LoadTracker::new();
        let plan = plan_pipeline(&peers, "qwen35", 32, &tracker).await.expect("plano");
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
        let plan = plan_pipeline(&peers, "qwen35", 32, &tracker).await.expect("plano");
        assert_eq!(plan.steps[0].inference_url, "http://192.168.100.138:8181");
    }
}
