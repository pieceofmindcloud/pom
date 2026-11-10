use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug, Default)]
pub struct AccountState {
    pub balance_micro: i64,
    pub reserved_micro: i64,
}

/// Gateway-local mirror of the principal's balances plus this gateway's
/// remaining allowance lease. Zero-latency: pure in-memory.
#[derive(Debug)]
pub struct BalanceBook {
    accounts: Mutex<HashMap<String, AccountState>>,
    allowance_remaining_micro: Mutex<i64>,
}

impl Default for BalanceBook {
    fn default() -> Self {
        Self {
            accounts: Mutex::new(HashMap::new()),
            // The principal merchant has no gateway lease. Secondary gateways
            // replace this with the allowance granted by the principal.
            allowance_remaining_micro: Mutex::new(i64::MAX),
        }
    }
}

impl BalanceBook {
    /// Admission: min(balance, allowance) - reserved >= hold.
    /// `None` means the book is unavailable (a poisoned lock): the caller
    /// must fail closed, never answer with a 402 challenge.
    pub fn try_reserve(&self, key_id: &str, hold: i64) -> Option<bool> {
        if hold < 0 {
            return Some(false);
        }
        if hold == 0 {
            return Some(true);
        }

        let allowance = match self.allowance_remaining_micro.lock() {
            Ok(allowance) => *allowance,
            Err(_) => return None,
        };

        let Ok(mut accounts) = self.accounts.lock() else {
            return None;
        };
        let state = accounts.get(key_id).cloned().unwrap_or_default();
        let available = state.balance_micro.min(allowance);
        let Some(free) = available.checked_sub(state.reserved_micro) else {
            return Some(false);
        };
        if free < hold {
            return Some(false);
        }

        let state = accounts.entry(key_id.to_string()).or_default();
        state.reserved_micro = state.reserved_micro.saturating_add(hold);
        Some(true)
    }

    pub fn release(&self, key_id: &str, hold: i64) {
        let hold = hold.max(0);
        if hold == 0 {
            return;
        }
        if let Ok(mut accounts) = self.accounts.lock() {
            if let Some(state) = accounts.get_mut(key_id) {
                state.reserved_micro = state.reserved_micro.saturating_sub(hold);
            }
        }
    }

    /// Settlement releases the hold and deducts the executed bill. A bill larger
    /// than the hold is allowed: the executed cost can be under the worst case,
    /// never over the request ceiling.
    pub fn settle(&self, key_id: &str, hold: i64, bill: i64) {
        let hold = hold.max(0);
        let bill = bill.max(0);
        if hold == 0 && bill == 0 {
            return;
        }
        if let Ok(mut accounts) = self.accounts.lock() {
            let state = accounts.entry(key_id.to_string()).or_default();
            state.reserved_micro = state.reserved_micro.saturating_sub(hold);
            state.balance_micro = state.balance_micro.saturating_sub(bill);
        }
    }

    pub fn grant(&self, key_id: &str, delta_micro: i64) {
        if let Ok(mut accounts) = self.accounts.lock() {
            let state = accounts.entry(key_id.to_string()).or_default();
            state.balance_micro = state.balance_micro.saturating_add(delta_micro);
        }
    }

    pub fn set_allowance(&self, allowance_micro: i64) {
        if let Ok(mut allowance) = self.allowance_remaining_micro.lock() {
            *allowance = allowance_micro.max(0);
        }
    }

    pub fn balance(&self, key_id: &str) -> i64 {
        self.accounts
            .lock()
            .ok()
            .and_then(|accounts| accounts.get(key_id).map(|state| state.balance_micro))
            .unwrap_or(0)
    }

    pub fn reserved(&self, key_id: &str) -> i64 {
        self.accounts
            .lock()
            .ok()
            .and_then(|accounts| accounts.get(key_id).map(|state| state.reserved_micro))
            .unwrap_or(0)
    }

    /// Every account the book knows, sorted by id.
    pub fn accounts(&self) -> Vec<(String, AccountState)> {
        let mut all: Vec<_> = self
            .accounts
            .lock()
            .map(|a| a.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        all.sort_by(|a, b| a.0.cmp(&b.0));
        all
    }

    pub fn allowance(&self) -> i64 {
        self.allowance_remaining_micro
            .lock()
            .map(|allowance| *allowance)
            .unwrap_or(0)
    }

    /// Test-only: poison the allowance lock the way a panic while holding it would.
    #[cfg(test)]
    pub(crate) fn poison_allowance_lock_for_test(&self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.allowance_remaining_micro.lock().unwrap();
            panic!("simulate a payments panic");
        }));
    }
}