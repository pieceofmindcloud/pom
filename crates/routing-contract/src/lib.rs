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

impl RoutingRequest {
    pub fn validate(&self) -> Result<(), RoutingError> {
        if self.api_version != ROUTING_API_VERSION {
            return Err(RoutingError::UnsupportedApiVersion {
                received: self.api_version,
            });
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

    #[test]
    fn an_inventory_keeps_overlapping_ranges_for_distinct_models() {
        let inventory = ModelInventory {
            peer_id: "no1".into(),
            capabilities: vec![
                ModelCapability {
                    model_id: "qwen35@r1".into(),
                    start_layer: 0,
                    end_layer: 2,
                    supports_inference: true,
                },
                ModelCapability {
                    model_id: "ornith@r7".into(),
                    start_layer: 0,
                    end_layer: 2,
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
