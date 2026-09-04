//! Catraca autenticada da política de coleta de logs.
//!
//! O booleano no `Announce` só desperta uma consulta. A resposta do principal,
//! protegida pela chave de conexão, é a única fonte que pode mudar o valor
//! local.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::cluster_logs::ClusterLogs;
use crate::peer_client::PeerClients;

pub const CLUSTER_LOGS_POLICY_PATH: &str = "/api/cluster/logs";

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
        return PolicyOutcome::Unchanged;
    }
    PolicyOutcome::Fetch {
        principal: principal_id.to_string(),
    }
}

#[derive(Deserialize)]
struct LogsReply {
    logs_enabled: bool,
}

pub async fn fetch_from_principal(
    clients: &PeerClients,
    principal_id: &str,
    base_url: &str,
    connection_key: &str,
) -> Result<bool> {
    let http = clients.for_peer(principal_id)?;
    let response = http
        .get(format!("{base_url}{CLUSTER_LOGS_POLICY_PATH}"))
        .bearer_auth(connection_key)
        .timeout(crate::webui::PEER_TIMEOUT)
        .send()
        .await
        .context("não deu para falar com o principal")?;
    if !response.status().is_success() {
        anyhow::bail!("principal respondeu {}", response.status());
    }
    let body: LogsReply = response
        .json()
        .await
        .context("resposta da política de logs não parseou")?;
    Ok(body.logs_enabled)
}

pub fn adopt_or_keep(local: bool, fetched: Result<bool>) -> PolicyOutcome {
    match fetched {
        Ok(value) if value != local => PolicyOutcome::Adopt(value),
        Ok(_) => PolicyOutcome::Unchanged,
        Err(error) => {
            warn!("busca da política de logs falhou, mantendo o valor local ({local}): {error}");
            PolicyOutcome::Unchanged
        }
    }
}

pub fn spawn_ratchet(
    net: Arc<crate::network::Network>,
    _clients: Arc<PeerClients>,
    auth: Arc<RwLock<crate::config::Config>>,
    logs: Arc<ClusterLogs>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
    net.set_cluster_logs_probe(tx);
    tokio::spawn(async move {
        while let Some((principal, announced)) = rx.recv().await {
            let Some(peer) = net
                .alive_peers()
                .await
                .into_iter()
                .find(|peer| peer.node_id == principal)
            else {
                continue;
            };
            let Some(port) = peer.gateway_port else {
                warn!("principal {principal} não anuncia porta de gateway; política de logs não pôde ser confirmada");
                continue;
            };
            let base = crate::peer_client::peer_base_url(
                &crate::network::ip_host(&peer.addr),
                port,
                announced,
            );
            let (local, connection_key) = {
                let cfg = auth.read().await;
                (net.cluster_logs_enabled(), cfg.connection_key.clone())
            };
            let probe = PeerClients::new(std::time::Duration::from_secs(10));
            probe.set_tls(announced);
            let fetched = fetch_from_principal(&probe, &principal, &base, &connection_key).await;
            if let PolicyOutcome::Adopt(value) = adopt_or_keep(local, fetched) {
                {
                    let mut cfg = auth.write().await;
                    cfg.cluster_logs_enabled = value;
                    if let Err(error) = cfg.save() {
                        warn!("política de logs confirmada mas não gravada: {error}");
                    }
                }
                net.set_cluster_logs_enabled(value);
                logs.set_enabled(value);
                info!(
                    "política de logs do cluster adotada do principal: {}",
                    if value { "ligada" } else { "desligada" }
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL: &str = "principal-1";

    #[test]
    fn anuncio_do_principal_discordante_dispara_busca() {
        assert_eq!(
            on_announce(false, true, true, PRINCIPAL),
            PolicyOutcome::Fetch {
                principal: PRINCIPAL.into()
            }
        );
    }

    #[test]
    fn anuncio_de_no_comum_nunca_vira_fonte_da_politica() {
        assert_eq!(
            on_announce(false, true, false, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
    }

    #[test]
    fn falha_na_busca_preserva_o_valor_local() {
        assert_eq!(
            adopt_or_keep(true, Err(anyhow::anyhow!("principal fora do ar"))),
            PolicyOutcome::Unchanged
        );
        assert_eq!(
            adopt_or_keep(false, Err(anyhow::anyhow!("principal fora do ar"))),
            PolicyOutcome::Unchanged
        );
    }
}
