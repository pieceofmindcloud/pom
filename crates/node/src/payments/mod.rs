// Phase A of docs/superpowers/plans/2026-10-13-x402-prepaid-credit.md: the
// module is complete but not wired yet - the gateway gate and the settlement
// hook (Phase B) are what consume this surface. Remove the allowance when the
// wiring lands.
#![allow(dead_code, unused_imports)]

mod book;
mod pricing;
mod x402;

pub use book::{AccountState, BalanceBook};
pub use pricing::{bill_micro, hold_micro, min_funding_micro, DeploymentPricing};
pub use x402::{Challenge402, GateError, PaymentState, Reservation};

#[cfg(test)]
mod tests;