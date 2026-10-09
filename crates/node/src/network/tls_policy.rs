//! Quem decide se o cluster fala TLS, e por qual caminho essa decisão viaja.
//!
//! A decisão é do nó principal. O caminho **não** é o gossip: o UDP de
//! descoberta é em claro e não autenticado, então um anúncio forjado por
//! qualquer um na rede rebaixaria o cluster inteiro para texto claro - o exato
//! ataque que o TLS existe para impedir.
//!
//! Então o campo `tls` do Announce é **gatilho, nunca valor**: discordar dele
//! só dispara uma busca autenticada no principal, e só a resposta dessa busca é
//! gravada no `pom.json`. Um intruso pode disparar a busca à vontade; não pode
//! responder por ela, porque não tem a chave de conexão.
//!
//! E a busca que falha **preserva o valor local**: um principal fora do ar ou
//! uma rede partida nunca rebaixam o cluster. Fail-closed.

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{info, warn};

use crate::peer_client::PeerClients;

/// Caminho que o principal serve e a catraca consulta.
pub const TLS_POLICY_PATH: &str = "/api/cluster/tls";

#[derive(Debug, PartialEq)]
pub enum PolicyOutcome {
    /// Nada a fazer.
    Unchanged,
    /// Buscar a verdade no principal.
    Fetch { principal: String },
    /// Valor confirmado por busca autenticada, pronto para gravar.
    Adopt(bool),
}

/// O que fazer ao ver um Announce.
///
/// `local` é o que este nó tem gravado; `announced`, o que o anúncio diz;
/// `from_principal`, se quem anunciou se apresenta como principal.
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
struct TlsReply {
    tls_required: bool,
}

/// Pergunta ao principal, por canal autenticado, se o cluster fala TLS.
///
/// Erro aqui é `Err`, e o chamador **mantém o valor local** - ver o parágrafo
/// sobre fail-closed no topo do módulo.
pub(crate) async fn fetch_from_principal_candidates(
    clients: &PeerClients,
    principal_id: &str,
    base_urls: &[String],
    connection_key: &str,
    signing_key: &ed25519_dalek::SigningKey,
    authority_principal: &crate::admin_trust::AuthorityPrincipal,
    trust: &crate::admin_trust::AdminTrust,
    tunnel: Option<&crate::peer_client::TunnelPeer>,
) -> Result<bool> {
    let headers = crate::gateway::GatewayState::signed_control_headers_for(
        signing_key,
        authority_principal.clone(),
        trust,
        "GET",
        TLS_POLICY_PATH,
        &[],
    )?;
    let resp = clients
        .get_internal_from_candidates(
            principal_id,
            base_urls,
            TLS_POLICY_PATH,
            connection_key,
            &headers,
            tunnel,
        )
        .await
        .context("could not reach the principal")?;
    let body: TlsReply = resp.json().context("principal response did not parse")?;
    info!(
        "TLS policy confirmed by the principal: {}",
        body.tls_required
    );
    Ok(body.tls_required)
}

/// Aplica o resultado da busca, preservando o local quando ela falha.
pub fn adopt_or_keep(local: bool, fetched: Result<bool>) -> PolicyOutcome {
    match fetched {
        Ok(v) if v != local => PolicyOutcome::Adopt(v),
        Ok(_) => PolicyOutcome::Unchanged,
        Err(e) => {
            warn!("TLS policy fetch failed, keeping the local value ({local}): {e}");
            PolicyOutcome::Unchanged
        }
    }
}

/// Sobe a task que atende a catraca.
///
/// Recebe `(node_id do principal, tls que ele anunciou)`, busca a política nele
/// por canal autenticado, e só então grava. O booleano anunciado é usado
/// **apenas** para saber por qual esquema discar a sonda - ver a nota no
/// `handle_message` do `network.rs`.
pub(crate) fn spawn_ratchet(
    net: std::sync::Arc<crate::network::Network>,
    clients: std::sync::Arc<PeerClients>,
    auth: std::sync::Arc<tokio::sync::RwLock<crate::config::Config>>,
    tls_server: Option<crate::serve::TlsServer>,
    signing_key: std::sync::Arc<ed25519_dalek::SigningKey>,
    authority_principal: crate::admin_trust::AuthorityPrincipal,
    trust: std::sync::Arc<crate::admin_trust::AdminTrust>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, bool)>();
    net.set_tls_probe(tx);
    tokio::spawn(async move {
        while let Some((principal, announced)) = rx.recv().await {
            let Some(peer) = net
                .alive_peers()
                .await
                .into_iter()
                .find(|p| p.node_id == principal)
            else {
                continue;
            };
            let candidates =
                net.order_gateway_bases(&peer.node_id, peer.gateway_candidates_with_tls(announced));
            if candidates.is_empty() {
                warn!("principal {principal} does not announce a gateway port; TLS policy could not be confirmed");
                continue;
            }

            let (local, key) = {
                let cfg = auth.read().await;
                (
                    cfg.tls_required,
                    crate::auth::internal_node_token(&cfg).unwrap_or_default(),
                )
            };
            // A sonda precisa falar o esquema que a URL diz, e o cache local
            // ainda está na política antiga. Um cliente próprio evita empurrar
            // a política nova para o cache antes de ela ser confirmada.
            let probe = PeerClients::new(std::time::Duration::from_secs(10));
            probe.set_tls(announced);
            let searched = fetch_from_principal_candidates(
                &probe,
                &principal,
                &candidates,
                &key,
                &signing_key,
                &authority_principal,
                &trust,
                crate::peer_client::TunnelPeer::from_peer(&peer).as_ref(),
            )
            .await;

            if let PolicyOutcome::Adopt(value) = adopt_or_keep(local, searched) {
                {
                    let mut cfg = auth.write().await;
                    cfg.tls_required = value;
                    if let Err(e) = cfg.save() {
                        warn!("TLS policy confirmed but not written: {e}");
                    }
                }
                clients.apply_cluster_policy(value);
                net.set_tls(value);
                if let Some(t) = &tls_server {
                    t.set_strict(value);
                }
                info!(
                    "cluster TLS policy adopted from the principal: {}",
                    if value { "on" } else { "off" }
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL: &str = "aaaa0000";

    /// Anúncio que concorda com o que já sabemos não faz nada. É o caso comum:
    /// um Announce a cada 5 segundos, por peer.
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

    /// Discordar **não** muda nada sozinho: só dispara a busca. O gossip é
    /// gatilho, nunca valor.
    #[test]
    fn a_principal_disagreement_triggers_a_fetch() {
        assert_eq!(
            on_announce(false, true, true, PRINCIPAL),
            PolicyOutcome::Fetch {
                principal: PRINCIPAL.to_string()
            }
        );
    }

    /// Este é o teste que sustenta a feature inteira: um nó qualquer da rede
    /// anunciando "sem TLS" é ignorado. O UDP é em claro e não autenticado, e
    /// obedecer a ele seria oferecer o rebaixamento que o TLS existe para
    /// impedir.
    #[test]
    fn a_non_principal_announce_is_ignored_even_when_disagreeing() {
        assert_eq!(
            on_announce(true, false, false, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
        assert_eq!(
            on_announce(false, true, false, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
    }

    /// Principal fora do ar não rebaixa ninguém.
    #[test]
    fn a_failing_fetch_preserves_the_local_value() {
        assert_eq!(
            adopt_or_keep(true, Err(anyhow::anyhow!("connection refused"))),
            PolicyOutcome::Unchanged
        );
        assert_eq!(
            adopt_or_keep(false, Err(anyhow::anyhow!("connection refused"))),
            PolicyOutcome::Unchanged
        );
    }

    #[test]
    fn a_successful_fetch_adopts_the_principal_value() {
        assert_eq!(adopt_or_keep(false, Ok(true)), PolicyOutcome::Adopt(true));
        assert_eq!(adopt_or_keep(true, Ok(false)), PolicyOutcome::Adopt(false));
        assert_eq!(adopt_or_keep(true, Ok(true)), PolicyOutcome::Unchanged);
    }
}
