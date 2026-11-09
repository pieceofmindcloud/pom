//! `NetworkSettings.metering` (spec 1). Changed by the principal only and
//! fanned out with the rest of the network settings.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeteringSettings {
    /// Off: no bundle nonce is minted, no receipt is built, the hot path is
    /// unchanged.
    pub enabled: bool,
    /// Payout cycle length in seconds, numbered from `cycle::METERING_EPOCH_MS`.
    pub cycle_secs: u64,
    /// Closed cycles kept besides the current one; at least 1.
    pub retain_cycles: u32,
    /// Receipts re-executed per node at cycle close.
    pub samples_per_node: u32,
    /// Random challenges per node per cycle.
    pub challenges_per_node: u32,
    /// Weight of each failed sample against pay; above 1 makes cheating a loss.
    pub penalty_factor: f64,
    /// Pay weight of a speculative position, one value for the network.
    pub draft_weight: f64,
    /// Node id of a dedicated verifier machine; `None`: the ledger admin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verifier: Option<String>,
    pub gateway_fee: GatewayFee,
    /// Grace period after a cycle ends before it is frozen, for late receipts.
    pub grace_secs: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayFee {
    pub per_request: f64,
    pub per_prompt_token: f64,
}

impl Default for MeteringSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            cycle_secs: 7 * 86_400,
            retain_cycles: 2,
            samples_per_node: 30,
            challenges_per_node: 5,
            penalty_factor: 2.0,
            draft_weight: 0.5,
            verifier: None,
            gateway_fee: GatewayFee::default(),
            grace_secs: 3_600,
        }
    }
}

impl MeteringSettings {
    /// The settings with every field in range: an edited or hand-written
    /// block never breaks the ledger.
    pub fn normalized(mut self) -> Self {
        self.cycle_secs = self.cycle_secs.max(3_600);
        self.retain_cycles = self.retain_cycles.max(1);
        self.samples_per_node = self.samples_per_node.max(1);
        if !self.penalty_factor.is_finite() || self.penalty_factor < 0.0 {
            self.penalty_factor = 2.0;
        }
        if !self.draft_weight.is_finite() {
            self.draft_weight = 0.5;
        }
        self.draft_weight = self.draft_weight.clamp(0.0, 1.0);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_block_is_metering_off_with_the_defaults() {
        let s: MeteringSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, MeteringSettings::default());
        assert!(!s.enabled);
    }

    #[test]
    fn out_of_range_values_are_normalized() {
        let s = MeteringSettings {
            cycle_secs: 0,
            retain_cycles: 0,
            samples_per_node: 0,
            penalty_factor: f64::NAN,
            draft_weight: 3.0,
            ..Default::default()
        }
        .normalized();
        assert_eq!((s.cycle_secs, s.retain_cycles, s.samples_per_node), (3_600, 1, 1));
        assert_eq!((s.penalty_factor, s.draft_weight), (2.0, 1.0));
    }
}
