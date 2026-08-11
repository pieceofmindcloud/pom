//! Deterministic open-source routing provider.

use routing_contract::{
    RoutingError, RoutingPlan, RoutingRequest, RoutingStep, ROUTING_API_VERSION,
};

/// Produce a complete, ordered plan from a peer capability snapshot.
///
/// This deliberately depends only on the versioned contract. Transport and
/// product-specific policies can select a different provider without changing
/// the open planner's input or output.
pub fn plan(request: &RoutingRequest) -> Result<RoutingPlan, RoutingError> {
    request.validate()?;

    let total_layers = request.cluster.total_layers;
    let mut expected_start = 0;
    let mut steps = Vec::new();

    while expected_start < total_layers {
        let candidate = request
            .cluster
            .peers
            .iter()
            .filter(|peer| peer.supports_inference && peer.start_layer == expected_start)
            .max_by(|left, right| {
                let left_len = left.end_layer.saturating_sub(left.start_layer);
                let right_len = right.end_layer.saturating_sub(right.start_layer);
                left_len
                    .cmp(&right_len)
                    .then_with(|| right.peer_id.cmp(&left.peer_id))
            });

        let Some(peer) = candidate else {
            let actual_start = request
                .cluster
                .peers
                .iter()
                .filter(|peer| peer.supports_inference && peer.start_layer > expected_start)
                .map(|peer| peer.start_layer)
                .min()
                .unwrap_or(expected_start);
            return Err(RoutingError::Gap {
                expected_start,
                actual_start,
            });
        };

        steps.push(RoutingStep {
            peer_id: peer.peer_id.clone(),
            start_layer: peer.start_layer,
            end_layer: peer.end_layer,
        });
        expected_start = peer.end_layer.saturating_add(1);
    }

    let result = RoutingPlan {
        api_version: ROUTING_API_VERSION,
        steps,
    };
    result.validate(total_layers)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use routing_contract::{ClusterSnapshot, PeerCapacity, RoutingRequest, ROUTING_API_VERSION};

    fn request(peers: Vec<PeerCapacity>) -> RoutingRequest {
        RoutingRequest {
            api_version: ROUTING_API_VERSION,
            cluster: ClusterSnapshot {
                total_layers: 4,
                peers,
            },
        }
    }

    #[test]
    fn plans_complete_contiguous_peers() {
        let result = plan(&request(vec![
            PeerCapacity {
                peer_id: "alpha".into(),
                start_layer: 0,
                end_layer: 1,
                supports_inference: true,
            },
            PeerCapacity {
                peer_id: "beta".into(),
                start_layer: 2,
                end_layer: 3,
                supports_inference: true,
            },
        ]))
        .unwrap();

        assert_eq!(result.steps.len(), 2);
        assert_eq!(result.steps[0].peer_id, "alpha");
        assert_eq!(result.steps[1].peer_id, "beta");
    }

    #[test]
    fn rejects_a_gap_between_available_peers() {
        let result = plan(&request(vec![
            PeerCapacity {
                peer_id: "alpha".into(),
                start_layer: 0,
                end_layer: 0,
                supports_inference: true,
            },
            PeerCapacity {
                peer_id: "beta".into(),
                start_layer: 2,
                end_layer: 3,
                supports_inference: true,
            },
        ]));

        assert_eq!(
            result,
            Err(routing_contract::RoutingError::Gap {
                expected_start: 1,
                actual_start: 2
            })
        );
    }

    #[test]
    fn chooses_lexical_peer_id_for_equal_ranges() {
        let result = plan(&request(vec![
            PeerCapacity {
                peer_id: "zulu".into(),
                start_layer: 0,
                end_layer: 3,
                supports_inference: true,
            },
            PeerCapacity {
                peer_id: "alpha".into(),
                start_layer: 0,
                end_layer: 3,
                supports_inference: true,
            },
        ]))
        .unwrap();

        assert_eq!(result.steps[0].peer_id, "alpha");
    }
}
