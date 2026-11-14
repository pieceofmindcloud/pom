//! GPU energy and temperature sampling for the logs plugin's Metrics screen.
//!
//! Once a minute an admin node that runs the logs plugin reads every member's
//! `/api/ui/node` (its own through `node_info`), keeps each GPU's power,
//! temperature, utilization and VRAM plus the model the machine serves, and
//! hands one `{"type": "gpu_samples"}` event to the plugin. The plugin keeps
//! the hour in memory and writes one row per machine, card and hour; nothing
//! here is persisted. Two nodes on one machine report the same cards, so a
//! card is counted once per tick, keyed by hostname and index.

use super::*;

pub(crate) const GPU_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// What one `/api/ui/node` answer contributes to a tick, or `None` when it
/// carries no GPU.
pub(crate) fn machine_sample(
    node: &serde_json::Value,
    seen: &mut std::collections::HashSet<(String, u64)>,
) -> Option<serde_json::Value> {
    let node_id = node["node_id"].as_str()?.to_string();
    let hostname = node["system"]["hostname"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let name = node["node_name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .unwrap_or(&hostname)
        .to_string();
    let model = node["active_model"].as_str().unwrap_or("").to_string();
    let machine = if hostname.is_empty() {
        node_id.clone()
    } else {
        hostname.clone()
    };
    let gpus: Vec<_> = node["system"]["gpus"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|gpu| {
            let index = gpu["index"].as_u64()?;
            if !seen.insert((machine.clone(), index)) {
                return None;
            }
            Some(json!({
                "index": index,
                "name": gpu["name"],
                "power_watts": gpu["power_watts"],
                "temperature_c": gpu["temperature_c"],
                "utilization_pct": gpu["utilization_pct"],
                "vram_used_mb": gpu["vram_used_mb"],
                "vram_total_mb": gpu["vram_total_mb"],
            }))
        })
        .collect();
    if gpus.is_empty() {
        return None;
    }
    Some(json!({
        "node_id": node_id,
        "machine": machine,
        "name": name,
        "model": model,
        "serving": !model.is_empty(),
        "gpus": gpus,
    }))
}

async fn read_node(
    state: &GatewayState,
    peer: &crate::network::PeerInfo,
) -> Option<serde_json::Value> {
    if peer.node_id == state.network.node_id {
        let mut node =
            serde_json::to_value(node_info(HeaderMap::new(), State(state.clone())).await.0).ok()?;
        node["node_id"] = json!(peer.node_id);
        return Some(node);
    }
    let response = state
        .control_get(peer, "/api/ui/node", PEER_TIMEOUT)
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut node: serde_json::Value = response.json().await.ok()?;
    if node["node_id"].as_str().is_none() {
        node["node_id"] = json!(peer.node_id);
    }
    Some(node)
}

/// Starts the sampler; each tick does nothing unless this node may see the
/// network's peers and runs the logs plugin.
pub(crate) fn spawn_gpu_metrics_sampler(state: GatewayState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(GPU_SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if state
                .plugins
                .runtime_for(crate::plugins::LOGS_PLUGIN_CODE)
                .is_none()
            {
                continue;
            }
            let profile = state.auth.read().await.profile;
            if !profile.allows(crate::profile::ProfileAction::ViewNetworkPeers) {
                continue;
            }
            let peers = state.all_peers().await;
            let reads =
                futures::future::join_all(peers.iter().map(|peer| read_node(&state, peer))).await;
            let mut seen = std::collections::HashSet::new();
            let machines: Vec<_> = reads
                .iter()
                .flatten()
                .filter_map(|node| machine_sample(node, &mut seen))
                .collect();
            if machines.is_empty() {
                continue;
            }
            let event = json!({
                "type": "gpu_samples",
                "at": chrono::Utc::now().timestamp(),
                "machines": machines,
            });
            if let Err(error) = state
                .plugins
                .ingest_event(crate::plugins::LOGS_PLUGIN_CODE, event)
                .await
            {
                tracing::debug!(%error, "the logs plugin did not take the GPU samples");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_two_nodes_of_one_machine_report_is_counted_once() {
        let node = |id: &str| {
            json!({
                "node_id": id,
                "node_name": "rig",
                "active_model": "qwen",
                "system": {"hostname": "host-a", "gpus": [
                    {"index": 0, "name": "RTX", "power_watts": 120.5, "temperature_c": 61.0},
                    {"index": 1, "name": "RTX", "power_watts": null, "temperature_c": null}
                ]}
            })
        };
        let mut seen = std::collections::HashSet::new();
        let first = machine_sample(&node("a"), &mut seen).expect("first node reports its cards");
        assert_eq!(first["gpus"].as_array().unwrap().len(), 2);
        assert_eq!(first["serving"], true);
        assert_eq!(first["name"], "rig");
        assert!(machine_sample(&node("b"), &mut seen).is_none());
    }
}
