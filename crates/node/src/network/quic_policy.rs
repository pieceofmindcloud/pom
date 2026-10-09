//! Política de QUIC do cluster. O anúncio só desperta a busca autenticada;
//! nunca é uma fonte de verdade, pelo mesmo motivo da política de TLS.

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::peer_client::PeerClients;

pub const QUIC_POLICY_PATH: &str = "/api/cluster/quic";

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

#[derive(Deserialize)]
struct QuicReply {
    quic_enabled: bool,
}

/// Pergunta ao principal a política QUIC por todos os endereços dele, o que
/// já respondeu primeiro, e por último pelo túnel libp2p (`/api/cluster/quic`
/// está na lista do túnel). Só o primeiro endereço anunciado falhava para
/// sempre com um principal atrás de NAT ou num IPv6 inalcançável.
pub(crate) async fn fetch_from_principal(
    clients: &PeerClients,
    principal_id: &str,
    base_urls: &[String],
    connection_key: &str,
    announced_tls: bool,
    tunnel: Option<&crate::peer_client::TunnelPeer>,
) -> Result<bool> {
    let resp = clients
        .get_internal_from_candidates(
            principal_id,
            base_urls,
            QUIC_POLICY_PATH,
            connection_key,
            &reqwest::header::HeaderMap::new(),
            tunnel,
        )
        .await
        .context("could not reach the principal to confirm QUIC")?;
    let body: QuicReply = resp
        .json()
        .context("QUIC policy response did not parse")?;
    info!(
        quic_enabled = body.quic_enabled,
        announced_tls, "QUIC policy confirmed by the principal"
    );
    Ok(body.quic_enabled)
}

pub fn adopt_or_keep(local: bool, fetched: Result<bool>) -> PolicyOutcome {
    match fetched {
        Ok(value) if value != local => PolicyOutcome::Adopt(value),
        Ok(_) => PolicyOutcome::Unchanged,
        Err(error) => {
            warn!("QUIC policy fetch failed, keeping the local value ({local}): {error}");
            PolicyOutcome::Unchanged
        }
    }
}

pub fn spawn_ratchet(
    net: std::sync::Arc<crate::network::Network>,
    clients: std::sync::Arc<PeerClients>,
    auth: std::sync::Arc<tokio::sync::RwLock<crate::config::Config>>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
    net.set_quic_probe(tx);
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
            if peer.gateway_port.is_none() {
                warn!("principal {principal} does not announce a gateway port; QUIC policy could not be confirmed");
                continue;
            }
            let bases = net.order_gateway_bases(
                &peer.node_id,
                peer.gateway_candidates_with_tls(announced_tls),
            );
            let tunnel = crate::peer_client::TunnelPeer::from_peer(&peer);
            let (local, key) = {
                let cfg = auth.read().await;
                (cfg.quic_enabled, cfg.connection_key.clone())
            };
            let probe = PeerClients::new(std::time::Duration::from_secs(10));
            probe.set_tls(announced_tls);
            let fetched = fetch_from_principal(
                &probe,
                &principal,
                &bases,
                &key,
                announced_tls,
                tunnel.as_ref(),
            )
            .await;
            if let PolicyOutcome::Adopt(value) = adopt_or_keep(local, fetched) {
                {
                    let mut cfg = auth.write().await;
                    cfg.quic_enabled = value;
                    if let Err(error) = cfg.save() {
                        warn!("QUIC policy confirmed but not written: {error}");
                    }
                }
                clients.set_quic(value);
                crate::peer_client::set_quic_enabled(value);
                info!(
                    quic_enabled = value,
                    "cluster QUIC policy adopted from the principal"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL: &str = "aaaa0000";

    #[test]
    fn an_agreeing_announce_does_nothing() {
        assert_eq!(
            on_announce(true, true, true, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
        assert_eq!(
            on_announce(false, false, true, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
    }

    #[test]
    fn a_principal_announce_triggers_a_fetch_without_adopting_gossip() {
        assert_eq!(
            on_announce(false, true, true, PRINCIPAL),
            PolicyOutcome::Fetch {
                principal: PRINCIPAL.to_string()
            }
        );
        assert_eq!(
            on_announce(false, true, false, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
    }

    #[test]
    fn on_by_default_and_a_failed_fetch_keeps_the_local_value() {
        let cfg: crate::config::Config =
            serde_json::from_str(r#"{"profile":"node","connection_key":"k"}"#).unwrap();
        assert!(cfg.quic_enabled);
        assert!(cfg.network_chat_enabled);
        assert_eq!(
            adopt_or_keep(false, Err(anyhow::anyhow!("offline"))),
            PolicyOutcome::Unchanged
        );
        assert_eq!(adopt_or_keep(true, Ok(false)), PolicyOutcome::Adopt(false));
    }

    #[test]
    fn a_confirmed_policy_adopts_only_the_principal_value() {
        assert_eq!(adopt_or_keep(false, Ok(true)), PolicyOutcome::Adopt(true));
        assert_eq!(adopt_or_keep(true, Ok(true)), PolicyOutcome::Unchanged);
    }
}
