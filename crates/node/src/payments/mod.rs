// Prepaid credit, ported from `feat/x402_research` (plan
// `2026-10-13-x402-prepaid-credit.md`) for visitor charging
// (`docs/superpowers/plans/2026-11-09-visitor-charging.md`). Wired for visitor
// charging (`crate::delegation`, `runtime.rs`); parts of the original core
// (refunds, the per-gateway allowance, minimum funding) are kept for the key
// accounts and the x402 facilitator and are not used yet.
#![allow(dead_code, unused_imports)]

mod account;
mod book;
mod ledger;
mod pricing;
pub(crate) mod runtime;
mod x402;

pub use account::{key_account, network_account, AccountId};
pub use book::{AccountState, BalanceBook};
pub use ledger::{LedgerWriter, PaymentWrite};
pub use pricing::{bill_micro, hold_micro, min_funding_micro, DeploymentPricing};
pub use x402::{Challenge402, GateError, PaymentState, Reservation};

#[cfg(test)]
mod tests;
/// The tables of the prepaid credit (`ensure_schema` in `backend/db.rs`).
/// `key_id` holds an account id (`AccountId`): `key:<api key id>` or
/// `net:<visitor network principal key>`; a bare id from before is a key.
pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS x402_accounts (
        key_id TEXT PRIMARY KEY,
        balance_micro INTEGER NOT NULL DEFAULT 0,
        updated_at TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS x402_grants (
        key_id TEXT NOT NULL,
        gateway_node_id TEXT NOT NULL,
        allowance_micro INTEGER NOT NULL,
        granted_at TEXT NOT NULL,
        expires_at TEXT NOT NULL,
        PRIMARY KEY (key_id, gateway_node_id)
    );
    CREATE TABLE IF NOT EXISTS x402_ledger (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        key_id TEXT NOT NULL,
        kind TEXT NOT NULL CHECK (kind IN ('grant', 'settle', 'refund')),
        amount_micro INTEGER NOT NULL,
        deploy_id TEXT,
        detail_json TEXT,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS x402_ledger_key_idx
        ON x402_ledger(key_id, created_at DESC);
    -- On a visitor: what it spent on each network it visited.
    CREATE TABLE IF NOT EXISTS x402_spend (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        host_network TEXT NOT NULL,
        account TEXT NOT NULL,
        model TEXT,
        charged_micro INTEGER NOT NULL,
        balance_micro INTEGER,
        created_at TEXT NOT NULL
    );
    -- On the principal: visitors asking for credit.
    CREATE TABLE IF NOT EXISTS x402_credit_requests (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        account TEXT NOT NULL,
        node_id TEXT NOT NULL,
        amount_micro INTEGER NOT NULL,
        note TEXT,
        status TEXT NOT NULL DEFAULT 'pending',
        created_at TEXT NOT NULL,
        decided_at TEXT
    );
";
