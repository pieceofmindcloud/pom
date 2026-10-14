# x402 Prepaid Credit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an admin require consumers (including inference nodes) to pay for
inference, settled fast enough to add **zero latency to the inference path**,
billed on input + output tokens, using the x402 402-challenge wire shape at the
funding boundary only.

**Architecture:** x402 runs only at the funding boundary (top-up), never per
generation. Inference runs against a prepaid balance held in memory on the
gateway. The **principal node is the only merchant** and the only writer of the
authoritative ledger; a secondary gateway enforces locally against a
principal-granted **allowance lease** so the hot path makes no network call. A
request reserves a worst-case **hold** at admission and settles the executed
token counts at the terminal `GenSummary`. Phase 1 is **pure internal credit**
(no chain, no facilitator, no `X-PAYMENT` verification); the 402 body keeps the
standard `accepts` envelope so phase 2 (the `pom-plugin-x402` facilitator) is a
drop-in. Pricing is **per deployment**. The **consumer pays**; serving peers are
never charged.

**Tech Stack:** Rust 2021, Tokio, Axum, rusqlite (shared `PomDb`), serde JSON,
React/TypeScript, Vitest.

**Spec:** none yet - the four design decisions are locked below and embedded in
this plan. A companion `specs/2026-10-13-x402-prepaid-credit-design.md` may be
written before implementation if the operator wants the "why" separated.

## Design decisions (locked by the operator)

1. **Merchant scope:** the principal node only. Secondary gateways enforce
   against a granted allowance; only the principal holds the truth and serves
   top-up.
2. **Phase-1 rail/asset:** pure internal credit. No chain, no facilitator, no
   signature verification. The 402 `accepts` entry uses an internal scheme
   (`pom-credit`).
3. **Price granularity:** per deployment. A deployment with `pricing: None` is
   free even when the network toggle is on.
4. **Direction:** the consumer pays. Serving peers are never charged; the hop
   supply routes stay ungated.

## Global Constraints

- **No new network call or disk I/O in the inference hot path.** Admission is an
  in-memory check; settlement is detached from the pipeline and off the decode
  loop.
- **Gate once per request at the gateway, never per hop.** `/v1/prefill`,
  `/v1/step`, `/v1/link`, `/v1/relay-result`, `/v1/remote-stream-result` stay
  ungated (they are supply, authenticated by `Ticket` / internal-node marker as
  today).
- **The hold is a true upper bound** because the requested ceiling is inviolable.
  It uses `OutputTokenLimit.effective` (the request total), **not** `leg`
  (`max_new_tokens` is one leg of the continuation loop - holding per leg
  under-reserves multi-leg requests).
- **Fixed-point micro units** for every amount (`1 unit = 1e-6` of the credit).
  No floating point in money math.
- **Disabled by default.** With the toggle off the gate is a no-op and no paid
  deployment exists.
- **Fail closed** like `enterprise_required`: if the payment state cannot be
  consulted, a paid request is refused, not served for free.
- **No legacy-protocol compatibility code** (product pre-release). The `v1` of
  `frame/v1-batch` is the first current contract.
- **Roles enforced by nodes, not screens.** The toggle and grant are principal
  policy; enforcement lives in the gateway, not the UI.
- **An IP is never an identity.** `payTo` and every account reference carry keys
  (`pk:<node_id>`, `key_id`).
- Documentation duty: every folder touched updates its `AGENTS.md` and the
  affected subsystem README in the same commit. New subsystem guide
  `docs/subsystems/payments.md`.

## Money math (single source of truth)

All amounts are `i64` micro-credits.

```
price_in_micro_per_1k   // micro-credits per 1000 input tokens
price_out_micro_per_1k  // micro-credits per 1000 output tokens

// Admission hold (worst case, upper bound):
hold = ceil(prompt_est * price_in_micro_per_1k / 1000)
     + effective        * price_out_micro_per_1k / 1000

// Settlement (executed counts, mirrors routing/fastest.rs:364 and
// usage.prompt_tokens_details.cached_tokens):
bill = (prompt_tokens - cached_tokens).max(1) * price_in_micro_per_1k / 1000
     + completion_tokens               * price_out_micro_per_1k / 1000

// min_funding (per deployment, derived unless admin raises it):
min_funding = max over live sessions of
    ceil(max_prompt_est * price_in_micro_per_1k / 1000)
  + max_effective       * price_out_micro_per_1k / 1000
```

`prompt_est` comes from `gateway::generation::estimate_tokens`; `effective` from
`gateway::budget::OutputTokenLimit.effective`. `ceil` on the input term so the
hold never under-reserves a partial thousand.

---

## Phase A - `payments/` core module (no wiring yet)

### A1. Module skeleton + hold/settle math (TDD)

- [x] Write failing unit tests in `crates/node/src/payments/tests.rs` for
  `hold_micro`, `bill_micro`, and `min_funding_micro` covering: exact
  thousand multiples, partial thousands (ceil), zero cached, cached == prompt
  (charge floor 1), and the `effective`-not-`leg` distinction.
- [x] Create `crates/node/src/payments/mod.rs` re-exporting `book`, `pricing`,
  `x402`.
- [x] Create `crates/node/src/payments/pricing.rs`:

```rust
/// Micro-credit price pair attached to a deployment.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DeploymentPricing {
    pub price_in_micro_per_1k: i64,
    pub price_out_micro_per_1k: i64,
    /// Admin floor; never below the derived worst-case hold.
    pub min_funding_micro: i64,
}

pub fn hold_micro(
    pricing: &DeploymentPricing,
    prompt_est: i64,
    effective: i64,
) -> i64 {
    let in_part = (prompt_est * pricing.price_in_micro_per_1k + 999) / 1000; // ceil
    let out_part = effective * pricing.price_out_micro_per_1k / 1000;
    in_part + out_part
}

pub fn bill_micro(
    pricing: &DeploymentPricing,
    prompt_tokens: i64,
    cached_tokens: i64,
    completion_tokens: i64,
) -> i64 {
    let billed_prompt = (prompt_tokens - cached_tokens).max(1);
    billed_prompt * pricing.price_in_micro_per_1k / 1000
        + completion_tokens * pricing.price_out_micro_per_1k / 1000
}
```

- [x] Run `cargo test -p node payments::` -> green. Commit.

### A2. `BalanceBook` + `HoldBook` (in-memory, TDD)

- [x] Failing tests: admission rule `balance - reserved >= hold`; settle moves
  `reserved -> spent`; a settle larger than the hold is allowed (executed can be
  under the worst case, never over the ceiling); negative balance is recorded,
  not rejected at settle time.
- [x] Create `crates/node/src/payments/book.rs`:

```rust
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct AccountState {
    pub balance_micro: i64,
    pub reserved_micro: i64, // sum of live holds
}

/// Gateway-local mirror of the principal's balances plus this gateway's
/// remaining allowance lease. Zero-latency: pure in-memory.
#[derive(Default)]
pub struct BalanceBook {
    accounts: Mutex<HashMap<String, AccountState>>, // key_id -> state
    allowance_remaining_micro: Mutex<i64>,          // lease granted to this gateway
}

impl BalanceBook {
    /// Admission: min(balance, allowance) - reserved >= hold.
    pub fn try_reserve(&self, key_id: &str, hold: i64) -> bool { /* ... */ }
    pub fn release(&self, key_id: &str, hold: i64) { /* ... */ }
    pub fn settle(&self, key_id: &str, hold: i64, bill: i64) { /* ... */ }
    pub fn grant(&self, key_id: &str, delta_micro: i64) { /* ... */ }
    pub fn set_allowance(&self, allowance_micro: i64) { /* ... */ }
}
```

- [x] Green. Commit.

### A3. `PaymentState` aggregate + 402 body (TDD)

- [x] Failing test: `PaymentState::gate(...)` returns `Ok(Reservation)` when the
  toggle is off (no-op) and when the deployment is free; returns
  `Err(Challenge402)` with a body carrying `accepts[]` (scheme `pom-credit`,
  `payTo = pk:<principal>`, `maxAmountRequired`, `metadata.{hold,balance,min_funding}`)
  when under-funded.
- [x] Create `crates/node/src/payments/x402.rs` (the 402 envelope) and
  `PaymentState` in `mod.rs` holding `enabled: bool`, `principal_node_id: String`,
  `book: BalanceBook`, `pricing: PriceTable`.
- [x] Green. Commit.

### Phase A delivered (2026-10-14)

The module landed with four deliberate differences from the sketches above:

- `PaymentState` lives in `x402.rs` (beside the gate it owns), not in `mod.rs`.
- `BalanceBook::try_reserve` returns `Option<bool>`: `None` means the book is
  unavailable (a poisoned lock) and `gate` maps it to `GateError::Unavailable`,
  which Phase D answers as `503 payment_unavailable`. A broken book must never
  masquerade as a 402 challenge telling the client to pay.
- `min_funding_micro` is a generic over `(prompt_est, effective)` pairs of the
  live sessions, raised by the admin floor.
- `payments/mod.rs` carries a module-level `#![allow(dead_code,
  unused_imports)]` with a comment: the module is complete but unwired until
  Phase D puts the gate in the gateway. Remove the allowance when the wiring
  lands.

22 tests green in `payments::`.

---

## Phase B - persistence (off the hot path)

### B1. Ledger tables in `backend/db.rs`

- [ ] Add to `ensure_schema` (beside `api_keys`):

```sql
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
    kind TEXT NOT NULL,          -- grant | settle | refund
    amount_micro INTEGER NOT NULL,
    deploy_id TEXT,
    detail_json TEXT,
    created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS x402_ledger_key_idx ON x402_ledger(key_id, created_at DESC);
```

- [ ] Test: schema is idempotent (`ensure_schema` twice). Commit.

### B2. Bounded writer for ledger writes

- [ ] Mirror the `reqlog.rs` pattern: an `mpsc::UnboundedSender<PaymentWrite>`
  handed to a dedicated writer thread that owns the `PomDb` connection and
  batches inserts. Settlement and grants enqueue; nothing blocks the request.
- [ ] Test: enqueue N writes, drain, assert rows. Commit.

### B3. Load balances at startup

- [ ] On gateway start (payments enabled), read `x402_accounts` into the
  `BalanceBook` and the gateway's own `x402_grants` row into
  `allowance_remaining_micro`. Commit.

---

## Phase C - per-deployment pricing

### C1. `DeploymentPricing` on the deployment record

- [ ] In `crates/node/src/models/deployment.rs`, add
  `pub pricing: Option<DeploymentPricing>` to `Deployment`. Persisted inside the
  existing `payload_json` column (no new column). Bump `DEPLOYMENT_VERSION` so a
  price change is a new version and a redeploy of an old version restores the old
  price.
- [ ] Test: serialize/deserialize round-trip; old payload without `pricing`
  deserializes as `None`. Commit.

### C2. Price lookup at admission is free

- [ ] `resolve_model` already resolves `<model>@<deploy_id>` to a `Deployment`;
  the gate reads `deployment.pricing` from the same resolution. No extra query.
  Commit after wiring in Phase D.

---

## Phase D - the gate (disabled by default)

### D1. Wire `PaymentState` into `GatewayState`

- [ ] Add `pub payments: Arc<payments::PaymentState>` to `GatewayState`
  (`state.rs:407`); construct it in `start_gateway_with_ready` from config +
  profile. Default `enabled = false`.
- [ ] Declare the module in `main.rs` beside the others:

```rust
#[path = "payments/mod.rs"]
mod payments;
```

- [ ] Compile check. Commit.

### D2. Admission gate as a sibling of the 413/503 refusal

- [ ] In `gateway/http.rs::handle_generate` (the `output_limit.rejection_reason()`
  413/503 branch, ~1474-1508), after the limit is computed and before generation,
  call `state.payments.gate(&caller, &deployment, prompt_est, effective)`. On
  `Err(Challenge402)` return `402` with the `accepts` body. On `Ok(reservation)`
  carry the `Reservation { key_id, hold }` into the generation task.
- [ ] Same insertion in `openai.rs` (`/v1/chat/completions`, `/v1/completions`),
  `messages.rs` (`/v1/messages`), and `decisions.rs` (`/v1/systemone`,
  `/v1/decisions`, `max_tokens=1`). `/v1/messages/count_tokens` stays free.
- [ ] Test (use `gateway::testkit`, never real UDP): toggle off -> request passes
  unchanged; toggle on + free deployment -> passes; toggle on + paid deployment +
  under-funded key -> `402` with `accepts`; funded key -> passes and a hold is
  recorded. Commit.

### D3. Fail closed

- [ ] If `PaymentState` is enabled but the book/principal state is unavailable,
  return `503` `payment_unavailable` (mirror `enterprise_required`). Test. Commit.

---

## Phase E - settlement at the terminal

### E1. Settle on `GenSummary`

- [ ] In `gateway/generation.rs` where the terminal `GenSummary` reaches
  `reqlog::complete` (`AuditCompletion { prompt_tokens, completion_tokens,
  cached_tokens }`), after the audit completes, call
  `book.settle(key_id, hold, bill_micro(...))` and enqueue an `x402_ledger`
  `settle` row. Detached from the decode loop.
- [ ] Test: a generation with known counts produces the expected `bill` and a
  ledger row; cached prefix reduces the billed prompt. Commit.

### E2. Abandoned / truncated streams

- [ ] On client disconnect or a cut inside a tool call, settle with the counts
  actually executed (the same counts `reqlog` records); release any unspent hold.
  Test both paths. Commit.

---

## Phase F - principal-only merchant: grants, leases, reconciliation

### F1. Grant fan-out

- [ ] `POST /api/ui/payments/grant` (AdminOnly, **principal only**) writes the
  ledger, updates `x402_accounts`, and fans the new balance to live gateways via
  the existing internal channel (`post_internal_json` / `send_to_gateway_over`,
  the path `validate_api_key_remote_parts` already uses). Test with testkit.
  Commit.

### F2. Allowance lease + renewal

- [ ] Principal grants each gateway an `allowance_micro` lease
  (`x402_grants`). Gateway spends locally against it (no round trip). A batched,
  off-path report (same cadence/channel as `AuditLog` replication and the
  30 s `spawn_trust_chain_keeper` shape in `backend/admin.rs`) reconciles spent
  amounts and renews the lease. When the allowance is exhausted the gateway
  `402`s and points the client at the principal. Commit.

### F3. Principal offline

- [ ] A secondary gateway keeps serving against its remaining allowance while the
  principal is down (no new grants, no reconciliation); when the allowance is
  gone, paid deployments `402` and unpaid ones are unaffected. Test. Commit.

---

## Phase G - toggle + admin UI

### G1. Toggle in config / network settings

- [ ] Add a `payments` block to `pom.json` (`utils/config.rs`) and to
  `NetworkSettings` (principal-owned, fanned out via `fanout_network_settings` in
  `webui/network.rs`): `enabled: bool`, `default_price_in_micro_per_1k`,
  `default_price_out_micro_per_1k`. Default `enabled = false`. Commit.

### G2. AdminOnly routes

- [ ] `GET /api/ui/payments` (balances, grants, ledger summary),
  `POST /api/ui/payments/grant`, `POST /api/ui/payments/refund` (admin action,
  not automatic on revocation). Register beside the other AdminOnly routes in
  `webui/routes/admin.rs`. Test. Commit.

### G3. Payments screen

- [ ] `apps/frontend/src/routes/` Payments screen: toggle, per-deployment price
  editor, grant/refund, balance table. Rebuild with `npm run build`. Commit.

---

## Phase H - `pom-plugin-x402` (phase 2, out of core)

- [ ] New plugin `pom-plugin-x402` loaded only by `pom-plugin-host` through the C
  ABI + versioned IPC (`crates/enterprise-protocol`), fail-closed like
  `enterprise_required`. It owns `X-PAYMENT` parsing, facilitator client, and
  aggregated batch settlement off the request path, exposed through
  `plugin_services.rs` (`host.services` + `POST /api/internal/plugins/:code/peer/:op`).
- [ ] Core keeps only the gate + balance book (per-request IPC would re-add the
  latency being removed). Commit.

---

## Phase I - documentation (same commits as the code)

- [ ] New `docs/subsystems/payments.md`: the lease model, hold/settle math,
  principal-only merchant, gate placement, fail-closed posture, phase-1 vs
  phase-2 rails.
- [ ] Update `docs/subsystems/http-api.md` (the 402 gate, `/api/ui/payments`,
  ungated supply routes), `inference.md` (settlement at `GenSummary`, the
  `effective`-not-`leg` hold), `deployment.md` (`DeploymentPricing` + version
  bump), `enterprise-licensing.md` (the `pom-plugin-x402` boundary).
- [ ] Update the affected folder `AGENTS.md` files (`crates/node/src/`,
  `backend/`, `models/`, `apps/frontend/src/routes/`, `docs/`).
- [ ] Add the new invariant to the root `AGENTS.md` "Invariants that hold
  everywhere": *x402 gates at the funding boundary only; the hold uses the
  request ceiling, never a leg; the principal is the only merchant.*

---

## Edge cases (covered by tests above)

| Case | Behavior |
|---|---|
| Toggle off | gate is a no-op; no paid deployment exists |
| Paid deployment, free key balance | `402` + `accepts` |
| Cached prefix | billed prompt = `prompt - cached`, floor 1 |
| Multi-leg continuation | hold uses `effective` (total), not `leg` |
| Abandoned stream | settle executed counts, release unspent hold |
| Cut inside a tool call | settle partial structured call, never discard |
| `/v1/systemone`, `/v1/decisions` | gated (`max_tokens=1` hold) |
| `/v1/messages/count_tokens` | free |
| RAG internal generation | billed to the originating caller key |
| Revoked key with balance | balance stays as owed credit; refund is an admin act |
| Principal offline | secondary gateway serves to allowance, then `402` |
| Infer node consumer | pays through `admin::node_chat_key` (`pom-<node_id>`) |

## Out of scope (phase 1)

- Chain settlement, facilitator, `X-PAYMENT` signature verification (phase 2,
  `pom-plugin-x402`).
- Gateway paying serving peers for layers (reverse receivable) - the operator
  chose consumer-pays.
- Automatic refund on key revocation.