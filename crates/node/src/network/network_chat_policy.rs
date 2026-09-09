//! Catraca autenticada da política do Chat da rede.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::network_chat::NetworkChat;
use crate::peer_client::PeerClients;

pub const NETWORK_CHAT_POLICY_PATH: &str = "/api/cluster/network-chat";

#[derive(Debug, PartialEq)]
pub enum PolicyOutcome {
    Unchanged,
    Fetch { principal: String },
    Adopt(bool),
}

pub fn on_announce(
    local: bool,
    announced: bool,
    from_principal: bool,
    principal_id: &str,
) -> PolicyOutcome {
    if !from_principal || announced == local {
        PolicyOutcome::Unchanged
    } else {
        PolicyOutcome::Fetch {
            principal: principal_id.to_string(),
        }
    }
}

pub fn adopt_or_keep(local: bool, fetched: Result<bool>) -> PolicyOutcome {
    match fetched {
        Ok(value) if value != local => PolicyOutcome::Adopt(value),
        Ok(_) => PolicyOutcome::Unchanged,
        Err(error) => {
            warn!("busca da política do Chat da rede falhou, mantendo {local}: {error}");
            PolicyOutcome::Unchanged
        }
    }
}

#[derive(Deserialize)]
struct NetworkChatReply {
    network_chat_enabled: bool,
}

pub async fn fetch_from_principal(
    clients: &PeerClients,
    principal_id: &str,
    base_url: &str,
    connection_key: &str,
) -> Result<bool> {
    let http = clients.for_peer(principal_id)?;
    let response = http
        .get(format!("{base_url}{NETWORK_CHAT_POLICY_PATH}"))
        .bearer_auth(connection_key)
        .timeout(crate::webui::PEER_TIMEOUT)
        .send()
        .await
        .context("não deu para falar com o principal")?;
    if !response.status().is_success() {
        anyhow::bail!("principal respondeu {}", response.status());
    }
    Ok(response
        .json::<NetworkChatReply>()
        .await
        .context("resposta do Chat da rede não parseou")?
        .network_chat_enabled)
}

pub fn spawn_ratchet(
    net: Arc<crate::network::Network>,
    _clients: Arc<PeerClients>,
    auth: Arc<RwLock<crate::config::Config>>,
    chat: Arc<NetworkChat>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
    net.set_network_chat_probe(tx);
    tokio::spawn(async move {
        while let Some((principal, announced_tls)) = rx.recv().await {
            let Some(peer) = net
                .alive_peers()
                .await
                .into_iter()
                .find(|peer| peer.node_id == principal)
            else {
                continue;
            };
            let Some(port) = peer.gateway_port else {
                warn!("principal {principal} não anuncia porta de gateway; política do Chat da rede não pôde ser confirmada");
                continue;
            };
            let base = crate::peer_client::peer_base_url(
                &crate::network::ip_host(&peer.addr),
                port,
                announced_tls,
            );
            let (local, connection_key) = {
                let cfg = auth.read().await;
                (net.network_chat_enabled(), cfg.connection_key.clone())
            };
            let probe = PeerClients::new(std::time::Duration::from_secs(10));
            probe.set_tls(announced_tls);
            let fetched = fetch_from_principal(&probe, &principal, &base, &connection_key).await;
            if let PolicyOutcome::Adopt(value) = adopt_or_keep(local, fetched) {
                {
                    let mut cfg = auth.write().await;
                    cfg.network_chat_enabled = value;
                    if let Err(error) = cfg.save() {
                        warn!("política do Chat da rede confirmada mas não gravada: {error}");
                    }
                }
                net.set_network_chat_enabled(value);
                chat.set_enabled(value);
                info!(
                    "política do Chat da rede adotada do principal: {}",
                    if value { "ligada" } else { "desligada" }
                );
            }
        }
    });
}
