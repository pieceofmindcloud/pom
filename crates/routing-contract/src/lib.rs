//! Versioned data contract for routing providers.

use serde::{Deserialize, Serialize};

/// Version negotiated by a node and any routing provider.
pub const ROUTING_API_VERSION: u16 = 1;

/// A model range a peer can make available to the routing control plane.
///
/// A peer can publish several entries, including overlapping ranges for the
/// same model and ranges belonging to entirely different model revisions.
/// `model_id` is an opaque, stable model revision identifier, never a UI label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapability {
    pub model_id: String,
    /// Inclusive first transformer layer for compatibility with the current
    /// node wire format. A future wire version can move to an exclusive end.
    pub start_layer: u32,
    pub end_layer: u32,
    /// Total transformer layers in this model revision. Optional preserves
    /// interoperability with peers that announced the first inventory format.
    #[serde(default)]
    pub total_layers: Option<u32>,
    pub supports_inference: bool,
}

/// The complete model inventory exposed by one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInventory {
    pub peer_id: String,
    pub capabilities: Vec<ModelCapability>,
}

/// Information about a peer that routing providers may use. It deliberately
/// excludes transport clients and runtime handles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerCapacity {
    pub peer_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub supports_inference: bool,
}

/// Snapshot supplied to a routing provider for a single planning decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSnapshot {
    pub total_layers: u32,
    pub peers: Vec<PeerCapacity>,
}

/// Versioned request accepted by routing providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingRequest {
    pub api_version: u16,
    pub cluster: ClusterSnapshot,
}

impl PeerCapacity {
    /// A faixa que este peer diz servir faz sentido no modelo declarado.
    ///
    /// `total_layers == 0` significa "o cluster ainda não sabe quantas camadas
    /// o modelo tem" (ver `catalog.rs`: o total é dedução, não número), então
    /// só a ordenação é conferida nesse caso; passar do total é recusado assim
    /// que existe um total.
    pub fn validate(&self, total_layers: u32) -> Result<(), RoutingError> {
        let out_of_bounds = total_layers > 0 && self.end_layer >= total_layers;
        if self.end_layer < self.start_layer || out_of_bounds {
            return Err(RoutingError::InvalidRange {
                start_layer: self.start_layer,
                end_layer: self.end_layer,
                total_layers,
            });
        }
        Ok(())
    }
}

impl ModelCapability {
    /// Mesma checagem para o que um peer anuncia sobre um modelo, contra o
    /// `total_layers` que ele mesmo declara (quando declara).
    pub fn validate(&self) -> Result<(), RoutingError> {
        let total = self.total_layers.unwrap_or(0);
        let out_of_bounds = total > 0 && self.end_layer >= total;
        if self.end_layer < self.start_layer || out_of_bounds {
            return Err(RoutingError::InvalidRange {
                start_layer: self.start_layer,
                end_layer: self.end_layer,
                total_layers: total,
            });
        }
        Ok(())
    }
}

impl RoutingRequest {
    /// Confere a versão **e** o snapshot.
    ///
    /// Conferir só a versão deixava um snapshot malformado (peer com
    /// `end_layer < start_layer`, ou faixa além de `total_layers`) chegar
    /// inteiro ao provider: `RoutingPlan::validate` pega os mesmos problemas na
    /// saída, mas só depois de o provider já ter confiado na entrada, e o erro
    /// então aponta para o plano em vez de para o peer que mentiu.
    pub fn validate(&self) -> Result<(), RoutingError> {
        if self.api_version != ROUTING_API_VERSION {
            return Err(RoutingError::UnsupportedApiVersion {
                received: self.api_version,
            });
        }
        for peer in &self.cluster.peers {
            peer.validate(self.cluster.total_layers)?;
        }
        Ok(())
    }
}

/// One ordered range in a routing plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingStep {
    pub peer_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
}

/// Ordered, full model coverage returned by a routing provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingPlan {
    pub api_version: u16,
    pub steps: Vec<RoutingStep>,
}

impl RoutingPlan {
    pub fn validate(&self, total_layers: u32) -> Result<(), RoutingError> {
        if self.api_version != ROUTING_API_VERSION {
            return Err(RoutingError::UnsupportedApiVersion {
                received: self.api_version,
            });
        }
        if total_layers == 0 {
            return Err(RoutingError::EmptyModel);
        }

        let mut expected_start = 0;
        for step in &self.steps {
            if step.start_layer != expected_start {
                return Err(RoutingError::Gap {
                    expected_start,
                    actual_start: step.start_layer,
                });
            }
            if step.end_layer < step.start_layer || step.end_layer >= total_layers {
                return Err(RoutingError::InvalidRange {
                    start_layer: step.start_layer,
                    end_layer: step.end_layer,
                    total_layers,
                });
            }
            expected_start = step.end_layer.saturating_add(1);
        }

        if expected_start != total_layers {
            return Err(RoutingError::Incomplete {
                expected_end: total_layers - 1,
                actual_end: expected_start.saturating_sub(1),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingError {
    UnsupportedApiVersion {
        received: u16,
    },
    EmptyModel,
    Gap {
        expected_start: u32,
        actual_start: u32,
    },
    InvalidRange {
        start_layer: u32,
        end_layer: u32,
        total_layers: u32,
    },
    Incomplete {
        expected_end: u32,
        actual_end: u32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(peers: Vec<PeerCapacity>) -> RoutingRequest {
        RoutingRequest {
            api_version: ROUTING_API_VERSION,
            cluster: ClusterSnapshot {
                total_layers: 32,
                peers,
            },
        }
    }

    fn peer(start_layer: u32, end_layer: u32) -> PeerCapacity {
        PeerCapacity {
            peer_id: "alpha".into(),
            start_layer,
            end_layer,
            supports_inference: true,
        }
    }

    #[test]
    fn a_well_formed_snapshot_validates() {
        assert_eq!(snapshot(vec![peer(0, 15), peer(16, 31)]).validate(), Ok(()));
    }

    /// Faixa invertida é entrada malformada, e recusá-la só na saída do
    /// provider aponta o erro para o plano em vez de para o peer que mentiu.
    #[test]
    fn an_inverted_range_is_refused_on_the_way_in() {
        assert_eq!(
            snapshot(vec![peer(16, 4)]).validate(),
            Err(RoutingError::InvalidRange {
                start_layer: 16,
                end_layer: 4,
                total_layers: 32,
            })
        );
    }

    #[test]
    fn a_range_past_the_model_is_refused_on_the_way_in() {
        assert_eq!(
            snapshot(vec![peer(0, 15), peer(16, 40)]).validate(),
            Err(RoutingError::InvalidRange {
                start_layer: 16,
                end_layer: 40,
                total_layers: 32,
            })
        );
    }

    /// Total desconhecido (`0`) é estado legítimo do cluster: a checagem de
    /// limite espera saber o total, a de ordenação não.
    #[test]
    fn an_unknown_total_still_refuses_an_inverted_range() {
        let mut req = snapshot(vec![peer(0, 15)]);
        req.cluster.total_layers = 0;
        assert_eq!(req.validate(), Ok(()));

        req.cluster.peers = vec![peer(9, 2)];
        assert!(req.validate().is_err());
    }

    #[test]
    fn a_capability_checks_its_own_declared_total() {
        let cap = |start_layer, end_layer, total_layers| ModelCapability {
            model_id: "qwen35@r1".into(),
            start_layer,
            end_layer,
            total_layers,
            supports_inference: true,
        };
        assert_eq!(cap(0, 15, Some(32)).validate(), Ok(()));
        assert!(cap(0, 32, Some(32)).validate().is_err());
        assert!(cap(15, 0, None).validate().is_err());
        assert_eq!(cap(0, 15, None).validate(), Ok(()));
    }

    #[test]
    fn an_inventory_keeps_overlapping_ranges_for_distinct_models() {
        let inventory = ModelInventory {
            peer_id: "no1".into(),
            capabilities: vec![
                ModelCapability {
                    model_id: "qwen35@r1".into(),
                    start_layer: 0,
                    end_layer: 2,
                    total_layers: Some(7),
                    supports_inference: true,
                },
                ModelCapability {
                    model_id: "ornith@r7".into(),
                    start_layer: 0,
                    end_layer: 2,
                    total_layers: Some(11),
                    supports_inference: true,
                },
            ],
        };

        let encoded = serde_json::to_string(&inventory).unwrap();
        let decoded: ModelInventory = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.capabilities.len(), 2);
        assert_ne!(
            decoded.capabilities[0].model_id,
            decoded.capabilities[1].model_id
        );
    }

    fn two_step_plan() -> RoutingPlan {
        RoutingPlan {
            api_version: ROUTING_API_VERSION,
            steps: vec![
                RoutingStep {
                    peer_id: "alpha".into(),
                    start_layer: 0,
                    end_layer: 15,
                },
                RoutingStep {
                    peer_id: "beta".into(),
                    start_layer: 16,
                    end_layer: 31,
                },
            ],
        }
    }

    #[test]
    fn contiguous_plan_validates() {
        assert_eq!(two_step_plan().validate(32), Ok(()));
    }

    #[test]
    fn plan_with_gap_is_rejected() {
        let mut plan = two_step_plan();
        plan.steps[1].start_layer = 17;

        assert_eq!(
            plan.validate(32),
            Err(RoutingError::Gap {
                expected_start: 16,
                actual_start: 17,
            })
        );
    }

    #[test]
    fn plan_json_round_trip_preserves_api_version() {
        let encoded = serde_json::to_string(&two_step_plan()).unwrap();
        let decoded: RoutingPlan = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.api_version, ROUTING_API_VERSION);
        assert_eq!(decoded, two_step_plan());
    }
}
