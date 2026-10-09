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
        CLUSTER_LOGS_POLICY_PATH,
        &[],
    )?;
    let response = clients
        .get_internal_from_candidates(
            principal_id,
            base_urls,
            CLUSTER_LOGS_POLICY_PATH,
            connection_key,
            &headers,
            tunnel,
        )
        .await
        .context("não deu para falar com o principal")?;
    let body: LogsReply = response
        .json()
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

pub(crate) fn spawn_ratchet(
    net: Arc<crate::network::Network>,
    _clients: Arc<PeerClients>,
    auth: Arc<RwLock<crate::config::Config>>,
    logs: Arc<ClusterLogs>,
    signing_key: Arc<ed25519_dalek::SigningKey>,
    authority_principal: crate::admin_trust::AuthorityPrincipal,
    trust: Arc<crate::admin_trust::AdminTrust>,
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
            let candidates =
                net.order_gateway_bases(&peer.node_id, peer.gateway_candidates_with_tls(announced));
            if candidates.is_empty() {
                warn!("principal {principal} não anuncia porta de gateway; política de logs não pôde ser confirmada");
                continue;
            }
            let (local, connection_key) = {
                let cfg = auth.read().await;
                (
                    net.cluster_logs_enabled(),
                    crate::auth::internal_node_token(&cfg).unwrap_or_default(),
                )
            };
            let probe = PeerClients::new(std::time::Duration::from_secs(10));
            probe.set_tls(announced);
            let fetched = fetch_from_principal_candidates(
                &probe,
                &principal,
                &candidates,
                &connection_key,
                &signing_key,
                &authority_principal,
                &trust,
                crate::peer_client::TunnelPeer::from_peer(&peer).as_ref(),
            )
            .await;
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
    use axum::response::IntoResponse;

    const PRINCIPAL: &str = "principal-1";

    #[derive(Clone)]
    struct PrincipalPolicyState {
        network: std::sync::Arc<crate::network::Network>,
    }

    async fn principal_logs_route(
        axum::extract::State(state): axum::extract::State<PrincipalPolicyState>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        let bearer = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        if request
            .headers()
            .get(crate::auth::INTERNAL_NODE_REQUEST_HEADER)
            .is_none()
            || bearer != Some("Bearer cluster-key")
        {
            return axum::http::StatusCode::FORBIDDEN.into_response();
        }
        axum::Json(serde_json::json!({
            "logs_enabled": state.network.cluster_logs_enabled(),
        }))
        .into_response()
    }

    #[test]
    fn a_disagreeing_principal_announce_triggers_a_fetch() {
        assert_eq!(
            on_announce(false, true, true, PRINCIPAL),
            PolicyOutcome::Fetch {
                principal: PRINCIPAL.into()
            }
        );
    }

    #[test]
    fn a_regular_node_announce_never_becomes_the_policy_source() {
        assert_eq!(
            on_announce(false, true, false, PRINCIPAL),
            PolicyOutcome::Unchanged
        );
    }

    #[test]
    fn a_failed_fetch_preserves_the_local_value() {
        assert_eq!(
            adopt_or_keep(true, Err(anyhow::anyhow!("principal fora do ar"))),
            PolicyOutcome::Unchanged
        );
        assert_eq!(
            adopt_or_keep(false, Err(anyhow::anyhow!("principal fora do ar"))),
            PolicyOutcome::Unchanged
        );
    }

    /// Dois nós reais do testkit usam listeners TCP reais. O endereço primário
    /// do principal aponta para um processo fantasma que responde 403, enquanto
    /// o candidato anunciado aponta para o gateway correto. A sonda precisa
    /// manter a identidade do principal, o marcador interno e a chave ao cair
    /// para o segundo endereço.
    #[tokio::test]
    async fn probe_from_secondary_retries_a_ghost_gateway_candidate() {
        let principal = std::sync::Arc::new(
            crate::network::Network::testkit(
                PRINCIPAL.into(),
                "principal-authority".into(),
                vec![],
                None,
                None,
                None,
            )
            .await
            .unwrap(),
        );
        principal.set_cluster_logs_enabled(true);
        let secondary = crate::network::Network::testkit(
            "secondary-1".into(),
            "secondary-authority".into(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let state = PrincipalPolicyState {
            network: principal.clone(),
        };
        let wrong_listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let gateway_port = wrong_listener.local_addr().unwrap().port();
        let right_listener = tokio::net::TcpListener::bind(("127.0.0.1", gateway_port))
            .await
            .unwrap();
        let wrong_app =
            axum::Router::new().fallback(|| async { axum::http::StatusCode::FORBIDDEN });
        let right_app = axum::Router::new()
            .route(
                CLUSTER_LOGS_POLICY_PATH,
                axum::routing::get(principal_logs_route),
            )
            .with_state(state);
        let wrong_task = tokio::spawn(async move {
            axum::serve(wrong_listener, wrong_app).await.unwrap();
        });
        let right_task = tokio::spawn(async move {
            axum::serve(right_listener, right_app).await.unwrap();
        });

        secondary.peers.write().await.insert(
            PRINCIPAL.into(),
            crate::network::PeerInfo {
                node_id: PRINCIPAL.into(),
                pubkey: "principal-authority".into(),
                cluster_id: None,
                trust_version: None,
                relay: None,
                addr: format!("[::1]:{gateway_port}"),
                addr_candidates: vec![format!("127.0.0.1:{gateway_port}")],
                model: "model".into(),
                deploy_id: String::new(),
                start_layer: 0,
                end_layer: 1,
                total_layers: 2,
                memory_mb: 1.0,
                last_seen: chrono::Utc::now(),
                ollama_port: None,
                inference_port: None,
                inference_quic_port: None,
                inference_frame_capabilities: Vec::new(),
                gateway_port: Some(gateway_port),
                gateway_quic_port: None,
                profile: crate::profile::NodeProfile::Admin,
                tls: false,
                hostname: "principal".into(),
                capabilities: Vec::new(),
                connection_verified: true,
                disk_total_mb: 0,
                disk_free_mb: 0,
                api_keys_active: 0,
                libp2p_peer_id: None,
                libp2p_addrs: Vec::new(),
                tp: None,
                runtime_capacity: None,
                idle: false,
            },
        );
        let candidates = secondary
            .peers
            .read()
            .await
            .get(PRINCIPAL)
            .unwrap()
            .gateway_candidates_with_tls(false);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let authority_principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            PRINCIPAL,
            "principal-authority",
            &signing_key,
        );
        let trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "cluster",
            authority_principal.clone(),
            &signing_key,
        )
        .unwrap();
        let clients = PeerClients::new(std::time::Duration::from_secs(2));
        let fetched = fetch_from_principal_candidates(
            &clients,
            PRINCIPAL,
            &candidates,
            "cluster-key",
            &signing_key,
            &authority_principal,
            &trust,
            None,
        )
        .await
        .unwrap();

        assert!(fetched);
        assert_eq!(candidates.len(), 2);
        wrong_task.abort();
        right_task.abort();
    }
}
