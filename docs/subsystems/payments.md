# Subsystem: prepaid credit (x402)

Optional, admin-controlled payment for inference. The consumer (a client key or
an infer node's chat key) pre-funds a balance; generation runs against that
balance with **no payment call in the inference path**. The x402 402-challenge
shape appears only at the funding boundary, never per generation.

**Status (2026-10-14):** Phases A and B landed - the core module
`crates/node/src/payments/` (pricing math, balance book, gate, 402 envelope,
durable ledger writer) with 26 unit tests, and the `x402_accounts`,
`x402_grants`, `x402_ledger` tables in `pom.db`. It is **not wired yet**: no
route charges, no toggle in `pom.json`, no screen. The remaining phases (gateway
gate, per-deployment pricing, settlement, grants/leases, admin UI, the
`pom-plugin-x402` facilitator) are the plan
`docs/superpowers/plans/2026-10-13-x402-prepaid-credit.md`. Until Phase D puts
the gate in the gateway, the module is complete but inert, and
`payments/mod.rs` carries a module-level `#![allow(dead_code, unused_imports)]`
that must be removed when the wiring lands.

## The model

- **x402 runs only at the funding boundary.** A request never gets a 402
  challenge/retry in the pipeline. The client tops up a credit balance; the
  gateway admits or refuses against that balance in memory.
- **The consumer pays; serving peers are never charged.** The hop supply routes
  (`/v1/prefill`, `/v1/step`, `/v1/link`, `/v1/relay-result`,
  `/v1/remote-stream-result`) stay ungated - they are supply, authenticated by
  the `Ticket` / internal-node marker as today.
- **The principal node is the only merchant** and the only writer of the
  authoritative ledger. A secondary gateway enforces locally against a
  principal-granted **allowance lease** so the hot path makes no network call.
- **Account identity is the API key** (`key_id`). An infer node that consumes
  inference pays through its own chat key (`admin::node_chat_key`,
  `pom-<node_id>`). An IP is never an identity: `payTo` carries `pk:<node_id>`.
- **Phase 1 is pure internal credit**: no chain, no facilitator, no
  `X-PAYMENT`. The 402 body keeps the standard `accepts` envelope with an
  internal scheme (`pom-credit`) so phase 2 (`pom-plugin-x402`) is a drop-in.
- **Disabled by default; fail closed.** With the toggle off the gate is a no-op.
  If the payment state cannot be consulted the request is refused (503), never
  served for free - the same posture as `enterprise_required` in `reqlog.rs`.

## Money math (`pricing.rs`)

All amounts are `i64` micro-credits (`1 unit = 1e-6` of the credit). No
floating point in money math. A deployment carries a `DeploymentPricing`
(`price_in_micro_per_1k`, `price_out_micro_per_1k`, an admin `min_funding_micro`
floor); `pricing: None` means free even when the network toggle is on.

```
hold  = ceil(prompt_est * price_in / 1000) + effective * price_out / 1000
bill  = (prompt_tokens - cached_tokens).max(1) * price_in / 1000
      + completion_tokens * price_out / 1000
min_funding = max over live sessions of hold, raised by the admin floor
```

The hold is a true upper bound because the requested ceiling is inviolable: it
uses `OutputTokenLimit.effective` (the request total), **not** `leg` -
`max_new_tokens` is one leg of the gateway's continuation loop, and holding per
leg under-reserves a multi-leg request. `prompt_est` comes from
`gateway::generation::estimate_tokens`; settlement uses the executed counts the
terminal `GenSummary` reports, with cached prompt tokens not billed (floor 1).

## The book (`book.rs`)

`BalanceBook` is the gateway-local mirror of the principal's balances plus this
gateway's remaining allowance lease. Pure in-memory, zero latency.

- **Admission:** `min(balance, allowance) - reserved >= hold`
  (`try_reserve`). It returns `Option<bool>`: `None` means the book is
  unavailable (a poisoned lock) and the gate maps it to `GateError::Unavailable`
  - a broken book must never masquerade as a 402 challenge telling the client
  to pay.
- **Settlement:** releases the hold and deducts the executed bill. A bill
  larger than the hold is allowed (the executed cost can be under the worst
  case, never over the ceiling); a negative balance is recorded, not rejected -
  it blocks the next admission.
- **Release:** an abandoned or truncated stream returns the unspent hold.
- **Grant / allowance:** the principal writes balances; each gateway holds a
  lease (`set_allowance`) it spends against locally.

## The gate (`x402.rs`)

`PaymentState` holds `enabled`, `principal_node_id`, the `BalanceBook` and a
`PriceTable` (per `deploy_id`). `gate` / `gate_deployment` compute the hold and
reserve it, or return `GateError::Challenge(Challenge402)` when under-funded,
or `GateError::Unavailable` when the book is broken. The 402 body:

```json
{"error": "payment_required",
 "accepts": [{"scheme": "pom-credit", "payTo": "pk:<principal>",
   "maxAmountRequired": <hold>,
   "metadata": {"hold_micro", "balance_micro", "min_funding_micro",
     "price_in_micro_per_1k", "price_out_micro_per_1k"}}]}
```

Phase D answers this as HTTP 402 at the gateway (a sibling of the 413/503
output-limit refusal in `gateway/http.rs`), once per request, after the output
limit is computed and before generation; `GateError::Unavailable` becomes 503
`payment_unavailable`.

## Files

- `crates/node/src/payments/mod.rs` - module root, re-exports, the temporary
  unwired allowance.
- `crates/node/src/payments/pricing.rs` - `DeploymentPricing`, `hold_micro`,
  `bill_micro`, `min_funding_micro`.
- `crates/node/src/payments/book.rs` - `AccountState`, `BalanceBook`.
- `crates/node/src/payments/ledger.rs` - `PaymentWrite`, `LedgerWriter` (the
  bounded writer mirroring `reqlog.rs`), `load_balances`, `load_grant`.
- `crates/node/src/payments/x402.rs` - `PriceTable`, `Reservation`,
  `Challenge402`, `GateError`, `PaymentState`.
- `crates/node/src/payments/tests.rs` - the unit proofs, including the poisoned
  book failing closed and the ledger surviving a reopen.

## Persistence

The three tables live in `pom.db` (`ensure_schema` in `backend/db.rs`):
`x402_accounts` (the durable balance mirror), `x402_grants` (the
principal-granted allowance leases, keyed `(key_id, gateway_node_id)` with an
`expires_at`), and `x402_ledger` (the append-only audit trail the principal
reconciles against). Money is fixed-point micro units (`INTEGER`).

`LedgerWriter` is the only writer: the request path enqueues a `PaymentWrite`
(grant, settle or refund) on a bounded channel and never blocks; the dedicated
thread batches the queue into one transaction per burst, and each write appends
the ledger row **and** moves `x402_accounts.balance_micro` together. A full
queue logs the loss, it does not stall a generation. Reserved holds are never
persisted: a restart ends every live generation, so `load_balances` rebuilds the
book from settled balances with nothing reserved, and `load_grant` reads only an
unexpired lease.

## What is not in the runtime yet

The gateway gate (Phase D), `Deployment.pricing` (Phase C), settlement at
`GenSummary` (Phase E), grants/leases/reconciliation (Phase F), the toggle and
the Payments screen (Phase G), and `pom-plugin-x402` (Phase H). The startup call
of `load_balances`/`load_grant` waits for the Phase D wiring. The design
decisions behind them - principal-only merchant, internal credit,
per-deployment pricing, consumer pays - are locked in the plan.