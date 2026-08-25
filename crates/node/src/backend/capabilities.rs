//! Capacidade de implantação calculada a partir de evidência do modelo,
//! runtime e topologia do plano.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Capability {
    pub state: CapabilityState,
    pub source: String,
    pub reason: String,
    /// Limitação de execução que não prova ausência da capacidade no modelo.
    /// A UI mostra este aviso, mas mantém o controle acionável.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeploymentCapabilities {
    pub fit_memory: Capability,
    pub no_thinking: Capability,
    pub no_embeddings: Capability,
    pub speculative_decoding: Capability,
    pub mtp: Capability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerTopology {
    pub node_id: String,
    pub start_layer: Option<u32>,
    pub end_layer: Option<u32>,
    pub total_layers: Option<u32>,
    pub peer_index: Option<u32>,
    pub peer_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerEmbeddingsDecision {
    pub node_id: String,
    pub safe: Option<bool>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoEmbeddingsEvaluation {
    pub capability: Capability,
    pub peers: Vec<PeerEmbeddingsDecision>,
}

pub(crate) fn supported(source: impl Into<String>, reason: impl Into<String>) -> Capability {
    Capability {
        state: CapabilityState::Supported,
        source: source.into(),
        reason: reason.into(),
        warning: None,
    }
}

pub(crate) fn unsupported(source: impl Into<String>, reason: impl Into<String>) -> Capability {
    Capability {
        state: CapabilityState::Unsupported,
        source: source.into(),
        reason: reason.into(),
        warning: None,
    }
}

pub(crate) fn unknown(source: impl Into<String>, reason: impl Into<String>) -> Capability {
    Capability {
        state: CapabilityState::Unknown,
        source: source.into(),
        reason: reason.into(),
        warning: None,
    }
}

pub(crate) fn evaluate_no_embeddings(peers: &[PeerTopology]) -> NoEmbeddingsEvaluation {
    let decisions = peers
        .iter()
        .map(|peer| {
            let Some(start) = peer.start_layer else {
                return PeerEmbeddingsDecision {
                    node_id: peer.node_id.clone(),
                    safe: None,
                    reason: "posição inicial do peer não foi informada".to_string(),
                };
            };
            let Some(end) = peer.end_layer else {
                return PeerEmbeddingsDecision {
                    node_id: peer.node_id.clone(),
                    safe: None,
                    reason: "posição final do peer não foi informada".to_string(),
                };
            };
            let Some(total) = peer.total_layers else {
                return PeerEmbeddingsDecision {
                    node_id: peer.node_id.clone(),
                    safe: None,
                    reason: "total de camadas do plano não foi informado".to_string(),
                };
            };
            let Some(index) = peer.peer_index else {
                return PeerEmbeddingsDecision {
                    node_id: peer.node_id.clone(),
                    safe: None,
                    reason: "posição do peer na cadeia não foi informada".to_string(),
                };
            };
            let Some(count) = peer.peer_count else {
                return PeerEmbeddingsDecision {
                    node_id: peer.node_id.clone(),
                    safe: None,
                    reason: "quantidade de peers da cadeia não foi informada".to_string(),
                };
            };
            if total == 0 || count == 0 || index >= count || start > end || end >= total {
                return PeerEmbeddingsDecision {
                    node_id: peer.node_id.clone(),
                    safe: None,
                    reason: format!(
                        "topologia inválida para L{start}-{end}: total={total}, peer={index} de {count}"
                    ),
                };
            }

            let single_peer = count <= 1 && start == 0 && end + 1 >= total;
            let serves_last_layer = end + 1 >= total;
            let last_in_split = index + 1 >= count;
            let safe = single_peer || serves_last_layer || last_in_split;
            PeerEmbeddingsDecision {
                node_id: peer.node_id.clone(),
                safe: Some(safe),
                reason: if safe {
                    if single_peer {
                        "peer único, sem exportação de residual".to_string()
                    } else if serves_last_layer {
                        "peer serve a última camada, sem exportação de residual".to_string()
                    } else {
                        "último peer da cadeia, sem exportação de residual".to_string()
                    }
                } else {
                    format!(
                        "peer intermediário L{start}-{end} precisa exportar o residual para o próximo peer"
                    )
                },
            }
        })
        .collect::<Vec<_>>();

    let capability = if decisions.is_empty() {
        unknown("topologia do plano", "nenhum peer selecionado")
    } else if decisions.iter().any(|peer| peer.safe.is_none()) {
        unknown(
            "topologia do plano",
            "a posição ou a cobertura de pelo menos um peer não pôde ser confirmada",
        )
    } else {
        supported(
            "topologia do plano",
            "o backend confirmou a posição e a necessidade de residual de cada peer",
        )
    };

    NoEmbeddingsEvaluation {
        capability,
        peers: decisions,
    }
}

pub(crate) fn validate_no_embeddings(
    evaluation: &NoEmbeddingsEvaluation,
    requested: bool,
) -> Result<Vec<String>, String> {
    if !requested {
        return Ok(Vec::new());
    }
    let unsafe_peers = evaluation
        .peers
        .iter()
        .filter(|peer| peer.safe == Some(false))
        .map(|peer| format!("{}: {}", peer.node_id, peer.reason))
        .collect::<Vec<_>>();
    if !unsafe_peers.is_empty() {
        return Err(format!(
            "não é seguro desativar embeddings em todos os peers: {}",
            unsafe_peers.join("; ")
        ));
    }
    Ok(evaluation
        .peers
        .iter()
        .map(|peer| peer.node_id.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str, start: u32, end: u32, total: u32, index: u32, count: u32) -> PeerTopology {
        PeerTopology {
            node_id: id.into(),
            start_layer: Some(start),
            end_layer: Some(end),
            total_layers: Some(total),
            peer_index: Some(index),
            peer_count: Some(count),
        }
    }

    #[test]
    fn no_embeddings_is_safe_for_a_single_peer() {
        let evaluation = evaluate_no_embeddings(&[peer("solo", 0, 31, 32, 0, 1)]);
        assert_eq!(evaluation.capability.state, CapabilityState::Supported);
        assert_eq!(evaluation.peers[0].safe, Some(true));
        assert!(validate_no_embeddings(&evaluation, true).is_ok());
    }

    #[test]
    fn intermediary_that_exports_residual_rejects_global_override() {
        let evaluation = evaluate_no_embeddings(&[
            peer("first", 0, 15, 32, 0, 2),
            peer("last", 16, 31, 32, 1, 2),
        ]);
        assert_eq!(evaluation.peers[0].safe, Some(false));
        assert_eq!(evaluation.peers[1].safe, Some(true));
        let error = validate_no_embeddings(&evaluation, true).unwrap_err();
        assert!(error.contains("first"));
        assert!(error.contains("residual"));
    }

    #[test]
    fn unknown_topology_is_visible_and_actionable_with_explicit_risk() {
        let evaluation = evaluate_no_embeddings(&[PeerTopology {
            node_id: "mystery".into(),
            start_layer: Some(0),
            end_layer: Some(15),
            total_layers: None,
            peer_index: Some(0),
            peer_count: Some(2),
        }]);
        assert_eq!(evaluation.capability.state, CapabilityState::Unknown);
        let peers = validate_no_embeddings(&evaluation, true).unwrap();
        assert_eq!(peers, vec!["mystery"]);
        assert!(evaluation
            .capability
            .reason
            .contains("não pôde ser confirmada"));
    }
}
