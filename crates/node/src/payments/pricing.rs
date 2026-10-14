use serde::{Deserialize, Serialize};

/// Micro-credit price pair attached to a deployment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeploymentPricing {
    pub price_in_micro_per_1k: i64,
    pub price_out_micro_per_1k: i64,
    /// Admin floor; never below the derived worst-case hold.
    #[serde(default)]
    pub min_funding_micro: i64,
}

fn non_negative(value: i64) -> i64 {
    if value < 0 {
        0
    } else {
        value
    }
}

fn ceil_mul_div_1000(a: i64, b: i64) -> i64 {
    let a = non_negative(a);
    let b = non_negative(b);
    let numerator = (a as i128) * (b as i128) + 999;
    let quotient = numerator / 1000;
    if quotient > i64::MAX as i128 {
        i64::MAX
    } else {
        quotient as i64
    }
}

fn floor_mul_div_1000(a: i64, b: i64) -> i64 {
    let a = non_negative(a);
    let b = non_negative(b);
    let quotient = (a as i128) * (b as i128) / 1000;
    if quotient > i64::MAX as i128 {
        i64::MAX
    } else {
        quotient as i64
    }
}

/// Admission hold: a true upper bound for the request.
///
/// `effective` is the request ceiling, not one continuation leg.
pub fn hold_micro(pricing: &DeploymentPricing, prompt_est: i64, effective: i64) -> i64 {
    ceil_mul_div_1000(prompt_est, pricing.price_in_micro_per_1k)
        .saturating_add(floor_mul_div_1000(effective, pricing.price_out_micro_per_1k))
}

/// Settlement: executed token counts, with cached prompt tokens not billed.
pub fn bill_micro(
    pricing: &DeploymentPricing,
    prompt_tokens: i64,
    cached_tokens: i64,
    completion_tokens: i64,
) -> i64 {
    let billed_prompt = prompt_tokens
        .saturating_sub(non_negative(cached_tokens))
        .max(1);
    floor_mul_div_1000(billed_prompt, pricing.price_in_micro_per_1k)
        .saturating_add(floor_mul_div_1000(completion_tokens, pricing.price_out_micro_per_1k))
}

/// Minimum funding for a deployment: the worst live-session hold, raised by the
/// admin floor when the admin asks for a larger one.
pub fn min_funding_micro<I>(pricing: &DeploymentPricing, sessions: I) -> i64
where
    I: IntoIterator<Item = (i64, i64)>,
{
    let derived = sessions
        .into_iter()
        .map(|(prompt_est, effective)| hold_micro(pricing, prompt_est, effective))
        .max()
        .unwrap_or(0);
    derived.max(non_negative(pricing.min_funding_micro))
}