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

pub async fn fetch_from_principal(
    clients: &PeerClients,
    principal_id: &str,
    base_url: &str,
    connection_key: &str,
    announced_tls: bool,
) -> Result<bool> {
    let http = clients.for_peer(principal_id)?;
    let resp = http
        .get(format!("{base_url}{QUIC_POLICY_PATH}"))
        .bearer_auth(connection_key)
        .timeout(crate::webui::PEER_TIMEOUT)
        .send()
        .await
        .context("não deu para falar com o principal para confirmar QUIC")?;
    if !resp.status().is_success() {
        anyhow::bail!("principal respondeu {}", resp.status());
    }
    let body: QuicReply = resp
        .json()
        .await
        .context("resposta da política QUIC não parseou")?;
    info!(
        quic_enabled = body.quic_enabled,
        announced_tls, "política QUIC confirmada pelo principal"
    );
    Ok(body.quic_enabled)
}

pub fn adopt_or_keep(local: bool, fetched: Result<bool>) -> PolicyOutcome {
    match fetched {
        Ok(value) if value != local => PolicyOutcome::Adopt(value),
        Ok(_) => PolicyOutcome::Unchanged,
        Err(error) => {
            warn!("busca da política QUIC falhou, mantendo o valor local ({local}): {error}");
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
            let Some(port) = peer.gateway_port else {
                warn!("principal {principal} não anuncia porta de gateway; política QUIC não pôde ser confirmada");
                continue;
            };
            let base = crate::peer_client::peer_base_url(
                &crate::network::ip_host(&peer.addr),
                port,
                announced_tls,
            );
            let (local, key) = {
                let cfg = auth.read().await;
                (cfg.quic_enabled, cfg.connection_key.clone())
            };
            let probe = PeerClients::new(std::time::Duration::from_secs(10));
            probe.set_tls(announced_tls);
            let fetched =
                fetch_from_principal(&probe, &principal, &base, &key, announced_tls).await;
            if let PolicyOutcome::Adopt(value) = adopt_or_keep(local, fetched) {
                {
                    let mut cfg = auth.write().await;
                    cfg.quic_enabled = value;
                    if let Err(error) = cfg.save() {
                        warn!("política QUIC confirmada mas não gravada: {error}");
                    }
                }
                clients.set_quic(value);
                crate::peer_client::set_quic_ativo(value);
                info!(
                    quic_enabled = value,
                    "política QUIC do cluster adotada do principal"
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
    fn anuncio_que_concorda_nao_faz_nada() {
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
    fn anuncio_do_principal_dispara_busca_sem_adotar_gossip() {
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
    fn default_off_e_falha_preservam_o_valor_local() {
        let cfg: crate::config::Config =
            serde_json::from_str(r#"{"profile":"node","connection_key":"k"}"#).unwrap();
        assert!(!cfg.quic_enabled);
        assert_eq!(
            adopt_or_keep(false, Err(anyhow::anyhow!("offline"))),
            PolicyOutcome::Unchanged
        );
        assert_eq!(adopt_or_keep(true, Ok(false)), PolicyOutcome::Adopt(false));
    }

    #[test]
    fn politica_confirmada_adota_apenas_o_valor_do_principal() {
        assert_eq!(adopt_or_keep(false, Ok(true)), PolicyOutcome::Adopt(true));
        assert_eq!(adopt_or_keep(true, Ok(true)), PolicyOutcome::Unchanged);
    }
}
