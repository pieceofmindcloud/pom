//! The pay rule (spec 9) and the payout formula (spec 7).

use crate::receipt::ReceiptClaims;

/// Layer-positions a receipt is paid for: full positions at weight 1,
/// speculative ones (verified batches and draft tokens, the latter only for
/// the draft layers) at `draft_weight`, cached positions not at all; scaled
/// by the admin's per-model weight.
pub fn layer_positions(r: &ReceiptClaims, draft_weight: f64, model_weight: f64) -> f64 {
    let layers = r.range[1].saturating_sub(r.range[0]) as f64;
    let full = (r.computed.prefill as f64 + r.computed.decode as f64) * layers;
    let spec = (r.computed.verify_accepted as f64 + r.computed.verify_rejected as f64) * layers;
    let draft = r.drafted.tokens as f64 * r.drafted.layers as f64;
    model_weight * (full + draft_weight * (spec + draft))
}

/// What a node is paid for a cycle: its earnings scaled down by the weighted
/// share of failed samples, times the penalty factor (spec 7). No samples,
/// no change.
pub fn payout(earnings: f64, w_pass: f64, w_fail: f64, penalty: f64) -> f64 {
    let total = w_pass + w_fail;
    if total <= 0.0 {
        return earnings;
    }
    earnings * (1.0 - penalty * w_fail / total).max(0.0)
}

/// The gateway fee of one bundle (spec 9).
pub fn gateway_fee(per_request: f64, per_prompt_token: f64, prompt_tokens: u32) -> f64 {
    per_request + per_prompt_token * prompt_tokens as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::tests::{claims_for, key};

    #[test]
    fn full_positions_pay_layers_and_cached_ones_nothing() {
        let mut r = claims_for(&key(), [10, 20], "b");
        r.computed.prefill = 100;
        r.computed.decode = 20;
        r.cached = 500;
        assert_eq!(layer_positions(&r, 0.5, 1.0), 1200.0);
        assert_eq!(layer_positions(&r, 0.5, 2.0), 2400.0);
    }

    #[test]
    fn speculative_positions_pay_draft_weight() {
        let mut r = claims_for(&key(), [0, 10], "b");
        r.computed = Default::default();
        r.computed.verify_accepted = 6;
        r.computed.verify_rejected = 4;
        r.drafted.tokens = 10;
        r.drafted.layers = 1;
        // (10 positions x 10 layers + 10 drafts x 1 layer) x 0.5
        assert_eq!(layer_positions(&r, 0.5, 1.0), 55.0);
    }

    #[test]
    fn one_failed_sample_of_thirty_costs_a_proportional_slice() {
        assert!((payout(300.0, 29.0, 1.0, 2.0) - 280.0).abs() < 1e-9);
        assert!((payout(300.0, 29.0, 1.0, 1.0) - 290.0).abs() < 1e-9);
        assert_eq!(payout(300.0, 0.0, 0.0, 2.0), 300.0);
        assert_eq!(payout(300.0, 1.0, 9.0, 2.0), 0.0);
    }

    #[test]
    fn penalty_above_one_makes_cheating_a_loss() {
        // A node faking a share f of its work saves f of its cost c.
        let (earnings, cost) = (100.0, 60.0);
        let honest = earnings - cost;
        for f in [0.1, 0.2, 0.5] {
            let cheat_at = |penalty: f64| payout(earnings, 1.0 - f, f, penalty) - cost * (1.0 - f);
            assert!(cheat_at(2.0) < honest, "f={f}");
            assert!(cheat_at(1.0) <= honest + 1e-9, "f={f}");
        }
    }

    #[test]
    fn gateway_fee_is_per_request_plus_per_prompt_token() {
        assert_eq!(gateway_fee(1.0, 0.01, 200), 3.0);
    }
}
