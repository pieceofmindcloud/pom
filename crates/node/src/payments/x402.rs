use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::book::BalanceBook;
use super::pricing::{hold_micro, DeploymentPricing};

#[derive(Default)]
pub struct PriceTable {
    prices: Mutex<HashMap<String, DeploymentPricing>>,
}

impl PriceTable {
    pub fn set(&self, deploy_id: &str, pricing: DeploymentPricing) {
        if let Ok(mut prices) = self.prices.lock() {
            prices.insert(deploy_id.to_string(), pricing);
        }
    }

    pub fn get(&self, deploy_id: &str) -> Option<DeploymentPricing> {
        self.prices
            .lock()
            .ok()
            .and_then(|prices| prices.get(deploy_id).cloned())
    }

    pub fn remove(&self, deploy_id: &str) {
        if let Ok(mut prices) = self.prices.lock() {
            prices.remove(deploy_id);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Reservation {
    pub key_id: String,
    pub hold_micro: i64,
}

#[derive(Clone, Debug)]
pub struct Challenge402 {
    pub body: Value,
    pub hold_micro: i64,
    pub balance_micro: i64,
}

impl Challenge402 {
    pub fn new(
        principal_node_id: &str,
        pricing: &DeploymentPricing,
        hold_micro: i64,
        balance_micro: i64,
    ) -> Self {
        let body = json!({
            "error": "payment_required",
            "accepts": [{
                "scheme": "pom-credit",
                "payTo": format!("pk:{principal_node_id}"),
                "maxAmountRequired": hold_micro,
                "metadata": {
                    "hold_micro": hold_micro,
                    "balance_micro": balance_micro,
                    "min_funding_micro": pricing.min_funding_micro,
                    "price_in_micro_per_1k": pricing.price_in_micro_per_1k,
                    "price_out_micro_per_1k": pricing.price_out_micro_per_1k
                }
            }]
        });
        Self {
            body,
            hold_micro,
            balance_micro,
        }
    }
}

#[derive(Debug)]
pub enum GateError {
    Challenge(Challenge402),
    Unavailable,
}

pub struct PaymentState {
    enabled: bool,
    principal_node_id: String,
    book: BalanceBook,
    pricing: PriceTable,
}

impl PaymentState {
    pub fn new(enabled: bool, principal_node_id: impl Into<String>) -> Self {
        let state = Self {
            enabled,
            principal_node_id: principal_node_id.into(),
            book: BalanceBook::default(),
            pricing: PriceTable::default(),
        };
        if state.enabled {
            state.book.set_allowance(i64::MAX);
        }
        state
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn principal_node_id(&self) -> &str {
        &self.principal_node_id
    }

    pub fn book(&self) -> &BalanceBook {
        &self.book
    }

    pub fn pricing(&self) -> &PriceTable {
        &self.pricing
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if enabled {
            self.book.set_allowance(i64::MAX);
        }
    }

    pub fn set_allowance(&self, allowance_micro: i64) {
        self.book.set_allowance(allowance_micro);
    }

    pub fn grant(&self, key_id: &str, delta_micro: i64) {
        self.book.grant(key_id, delta_micro);
    }

    pub fn gate(
        &self,
        key_id: &str,
        pricing: Option<&DeploymentPricing>,
        prompt_est: i64,
        effective: i64,
    ) -> Result<Reservation, GateError> {
        if !self.enabled {
            return Ok(Reservation {
                key_id: key_id.to_string(),
                hold_micro: 0,
            });
        }

        let Some(pricing) = pricing else {
            return Ok(Reservation {
                key_id: key_id.to_string(),
                hold_micro: 0,
            });
        };

        let hold = hold_micro(pricing, prompt_est, effective);
        if hold == 0 {
            return Ok(Reservation {
                key_id: key_id.to_string(),
                hold_micro: 0,
            });
        }

        match self.book.try_reserve(key_id, hold) {
            Some(true) => Ok(Reservation {
                key_id: key_id.to_string(),
                hold_micro: hold,
            }),
            Some(false) => {
                let balance = self.book.balance(key_id);
                Err(GateError::Challenge(Challenge402::new(
                    &self.principal_node_id,
                    pricing,
                    hold,
                    balance,
                )))
            }
            None => Err(GateError::Unavailable),
        }
    }

    pub fn gate_deployment(
        &self,
        key_id: &str,
        deploy_id: &str,
        prompt_est: i64,
        effective: i64,
    ) -> Result<Reservation, GateError> {
        let pricing = self.pricing.get(deploy_id);
        self.gate(key_id, pricing.as_ref(), prompt_est, effective)
    }

    pub fn settle(&self, key_id: &str, hold_micro: i64, bill_micro: i64) {
        self.book.settle(key_id, hold_micro, bill_micro);
    }

    pub fn release(&self, key_id: &str, hold_micro: i64) {
        self.book.release(key_id, hold_micro);
    }
}