//! Image model components (diffusion model, text encoder, VAE) that ComfyUI
//! serves on this node and on the network. Apart from the LLM runtime: a node
//! serves an LLM and image components at once.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::gateway::GatewayState;
use crate::plugin_services::ServedComponent;

/// A component served somewhere on the network.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct NetworkComponent {
    pub deploy_id: String,
    pub name: String,
    pub component: String,
    pub files: Vec<String>,
    pub status: String,
    pub node_id: String,
    pub hostname: String,
    pub plugin_code: String,
}

/// The components the image services of one machine serve.
pub(crate) fn components_of(
    node_id: &str,
    hostname: &str,
    services: &[Value],
) -> Vec<NetworkComponent> {
    services
        .iter()
        .filter(|service| service["kind"] == "image")
        .flat_map(|service| {
            let code = service["plugin_code"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            service["components"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| serde_json::from_value::<ServedComponent>(c.clone()).ok())
                .map(move |c| NetworkComponent {
                    deploy_id: c.deploy_id,
                    name: c.name,
                    component: c.component,
                    files: c.files,
                    status: c.status,
                    node_id: node_id.to_owned(),
                    hostname: hostname.to_owned(),
                    plugin_code: code.clone(),
                })
        })
        .collect()
}

/// Every image component served on the network: this node's and each live
/// peer's, sorted by deployment name, component and machine.
pub(crate) async fn network_components(state: &GatewayState) -> Vec<NetworkComponent> {
    let mut machines = vec![(state.network.node_id.clone(), state.network.hostname())];
    for peer in state.network.alive_peers().await {
        if peer.node_id != state.network.node_id {
            machines.push((peer.node_id.clone(), peer.hostname.clone()));
        }
    }
    let lookups = machines.into_iter().map(|(node_id, hostname)| async move {
        let services = crate::plugin_engine::services_of(state, &node_id)
            .await
            .unwrap_or_default();
        components_of(&node_id, &hostname, &services)
    });
    let mut all: Vec<NetworkComponent> = futures::future::join_all(lookups)
        .await
        .into_iter()
        .flatten()
        .collect();
    all.sort_by(|a, b| {
        (a.name.as_str(), a.component.as_str(), a.hostname.as_str()).cmp(&(
            b.name.as_str(),
            b.component.as_str(),
            b.hostname.as_str(),
        ))
    });
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn network_components_list_only_image_services() {
        let services = vec![
            json!({"plugin_code": "comfyui", "id": "comfyui-image", "kind": "image",
                   "name": "ComfyUI", "status": "ready",
                   "components": [{"deploy_id": "img-1", "name": "Z-Image",
                                   "component": "vae", "files": ["ae.safetensors"],
                                   "status": "ready"}]}),
            json!({"plugin_code": "strata", "id": "strata", "kind": "inference",
                   "name": "Strata", "status": "ready",
                   "components": [{"deploy_id": "x", "name": "x", "component": "vae",
                                   "files": [], "status": "ready"}]}),
        ];
        let listed = components_of("node-a", "machine-a", &services);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].node_id, "node-a");
        assert_eq!(listed[0].hostname, "machine-a");
        assert_eq!(listed[0].plugin_code, "comfyui");
        assert_eq!(listed[0].component, "vae");
        assert_eq!(listed[0].files, ["ae.safetensors"]);
    }
}
