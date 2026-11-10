# Charging visitors - implementation plan

Spec: `docs/superpowers/specs/2026-11-09-visitor-charging-design.md`.
Builds on: the x402 prepaid credit core (ported, `crates/node/src/payments/`,
its original plan `2026-10-13-x402-prepaid-credit.md` on `feat/x402_research`)
and the member gateway proposal
(`docs/superpowers/specs/2026-11-07-member-gateway-planning-design.md`).

Files and lines are from 2026-11-09; check `file:line` before relying on them.
Every task is TDD: failing test -> minimal implementation -> passing test ->
commit. Rust builds and tests run in the dev container; never `rustfmt` the
tree; route tests use `gateway::testkit`, never real UDP; stage named paths.
Branch: `feat/visitor-charging` (on `feat/gpu-work-metering`).

## Defaults taken for the spec's open questions

Change any of these before the task that uses it:

1. Credit unit: abstract micro-credits (`i64`, `1 = 1e-6` credit), no currency.
2. A home network's principal transfer: the account follows the home chain
   automatically (the new principal's statement names the old key in its
   transfer history; the host re-keys the account).
3. Negative floor: as the core does - a settlement may take a balance below
   zero, which blocks the next admission. No extra margin.
4. Per-visitor caps inside a network account: not in phase 1.
5. Free allowance: none.

## Status

- [x] A.1 Port the core (`4c2460f8`): `payments/{mod,book,ledger,pricing,x402,tests}.rs`
  from `feat/x402_research`, tables from `payments::SCHEMA`, 26 tests passing
  on today's code.
- [x] A.2 Account kinds: `payments/account.rs` (`net:<pubkey>`, `key:<id>`, a
  bare id reads as a key).
- [x] B.1-B.3, B.5 (inference port), C.1 (self-signed), D.1-D.3, E (leases), F (Payments screen),
  live check passed. Deviations and what remains: `docs/subsystems/payments.md`, "Not yet done".

---

## Phase A - the core, made ready

### A.3 Account ids through the book and the ledger

The core keys everything by a `&str` it calls `key_id`. Keep the column, pass
account ids (`AccountId::as_string`) everywhere; rename parameters
`key_id -> account` in `book.rs`, `ledger.rs`, `x402.rs`.

Tests (`payments/tests.rs`): a network account and a key account with the same
suffix are distinct balances; a ledger row written with a bare id (from the
original branch) loads into the key account.

### A.4 `Deployment.pricing`

`models/deployment.rs` (`DEPLOYMENT_VERSION = 2`, line ~39): add
`pricing: Option<DeploymentPricing>` per version, bump to 3, migrate a v2
record on load (no pricing). Port the branch's commit c87a6581 ("per-deployment
pricing on the record - a price change forks a version") onto today's
`upsert_version` / `RecordPlan`.

Tests: a v2 record loads with `pricing: None`; a price change appends a version;
reactivating a version restores its price; equal params and equal price reuse
the version.

---

## Phase B - member gateway

Implements `2026-11-07-member-gateway-planning-design.md`, decisions 1-5. Only
what charging needs is required (`member_gateway_only`); the softer
`member_gateway` comes with it at little cost.

### B.1 Policy

`utils/config.rs` `NetworkSettings`: `inference_access`
(`direct` | `member_gateway` | `member_gateway_only`, default `direct`, new
networks from `bootstrap_local_network` start at `member_gateway`) and
`entry_nodes` (`admins` | `admins_and_workers`). Fanned out with the other
network settings (already: `internal_network_settings`).

Tests: missing fields read `direct` / `admins`; round trip; the fan-out carries
them.

### B.2 Capability and entry list

`routing/pipeline.rs`: `CAP_GATEWAY_DELEGATE = "gateway/delegate-v1"`,
announced by a member whose role the policy lets serve as entry node. The
visitor keeps a list of live entry nodes ordered by measured RTT and recent
failures (reuse `route_plan` measurements).

Tests: announce only when the role is allowed; the list orders by RTT and
demotes a failed node.

### B.3 `POST /v1/gateway/delegate`

`backend/gateway/http.rs`, scope `AnyMember` with the handler checking the
caller is a visitor with a valid membership token (`409 plan_locally`
otherwise). Body: the route (`/v1/chat/completions`, `/v1/completions`,
`/v1/messages`, `/api/generate`), the client body, the visitor's request id,
and (phase C) the home statement. The entry node runs the route's handler with
itself as gateway and streams the same answer back. Reached through the routed
control call or the streamed tunnel (`network/infer_tunnel.rs`,
`INFER_TUNNEL_STREAM_PROTOCOL`).

Tests (testkit): a visitor's delegated chat completion answers as a local one,
streaming and blocking; a non-visitor caller gets 409; closing the stream
cancels the generation and releases the sessions.

### B.4 The visitor's path

`backend/gateway/generation.rs` / `openai.rs` / `messages.rs`: on a visitor
whose network says `member_gateway*`, the request goes to the first entry node
and the answer is relayed as is; failover to the next only when the request was
never delivered; `503 no_entry_node` with what was tried. Under
`member_gateway` and no entry node announcing the capability, plan locally (old
networks).

Tests: forward and relay; failover before delivery only; 503; local planning
fallback under `member_gateway` only.

### B.5 Workers refuse visitors

`inference/infer_server/` and the tunnel: under `member_gateway_only`, a
visitor caller gets `visitor_must_use_gateway`. Discovery: workers do not send
their announce to visitors and a `PeerList` to a visitor lists entry nodes only.

Tests: the policy matrix (`direct`, `member_gateway` with an old visitor,
`member_gateway_only`); a visitor never receives a worker in an announce or a
`PeerList` under `member_gateway_only` (`Network::testkit`).

### B.6 Records

`reqlog.rs`: `gateway` (entry node) on the entry node's row, `delegated_to` on
the visitor's; `x_pom.gateway` in the answer.

---

## Phase C - visitor accounts

### C.1 The home statement

New `crates/node/src/home_statement.rs`: claims `{node, node_key, net
(home cluster id), principal, principal_key, iat, exp}`, compact token with
header `<slug>-home-network/v1` (`membership::sign_compact` /
`open_compact`). Issued by the home network's principal for a member node; a
node that is its own principal signs its own. TTL one day; the visitor's node
renews it at two thirds, from its home principal (`POST
/api/internal/home-statement`, member scope on the home network).

Tests: issue and open; refused when the signer is not the home chain's
principal; refused for another node's key; expired refused; a self-statement of
a single-machine network opens.

### C.2 Statement on admission and on delegation

The admission ask (`webui/lobby.rs` `AdmissionAsk`, line ~296) and every
delegated request carry the visitor's current home statement. The host checks
it against the caller's key (mutual TLS, `TransportPeer`) and keeps the
account id `net:<principal_key>`.

Tests: a statement for another node is refused; the account id is derived from
the principal key, not the node.

### C.3 Accounts at admission

`webui/lobby.rs` `AdmissionDecision` (line ~592) / `membership.rs`
`apply_decision` (line ~260): an approval names the visitor's account; the first
visitor of a home network opens it, with an optional initial grant (written as
a `grant` through `LedgerWriter`, principal only). The admission list shows the
account and its balance.

Tests: two visitors of one home network share one account; a grant at approval
is a ledger row and a balance.

### C.4 Principal transfer

When a statement's principal key differs from the account's but the statement's
chain history names the account's key as a previous principal, the host
re-keys the account (`net:<new>`) and records it in the ledger detail.

Tests: re-key on transfer; an unrelated key opens a new account.

---

## Phase D - the gate and settlement on the entry node

### D.1 Payment state on the gateway

`GatewayState`: `payments: Arc<PaymentState>`, `payments_ledger:
Arc<LedgerWriter>`; built in `main.rs` and the testkit; `load_balances` at
startup on the principal. `NetworkSettings.payments.enabled` (principal only,
default off). Remove the `#![allow(dead_code)]` in `payments/mod.rs` when this
lands.

### D.2 The gate on delegated requests

In the delegate handler (B.3), after the output limit is computed and before
generation: account = the delegating visitor's network; price = the
deployment's pricing; `gate_deployment` -> reservation, `402
payment_required` (the core's envelope) or `503 payment_unavailable`. A local
client's request is not gated (key accounts are out of scope). The visitor's
route turns a delegated 402/503 into its own vocabulary (`openai.rs`,
`messages.rs`, `/api/generate`).

Tests (testkit): funded visitor served and charged; unfunded gets 402 with the
envelope, relayed to the visitor's client; a broken book gives 503; a free
deployment is not gated; a non-delegated local request is not gated.

### D.3 Settlement

At the terminal `GenSummary` of a delegated request
(`spawn_logged_generation_with_stops`, `generation.rs` line ~3439, where the
caller and the summary meet): `bill_micro` with executed counts (cached prompt
tokens not billed), settle through the book and `LedgerWriter` (`settle`),
release the unspent hold; a cancelled or failed request settles what ran. The
ledger row's detail names the metering bundle id when there is one
(`MeterCollect`), linking income to payouts.

Tests: settle equals the executed bill; abandoned stream releases the hold; a
bill above the hold is recorded (balance may go negative) and blocks the next
admission.

### D.4 What the visitor sees

`x_pom.payment {charged_micro, balance_micro, account}` on answers; `GET
/api/ui/payments/balance` on the visitor (asks its entry node,
`peer.payments.balance` answered from the book or the principal); a local spend
record per host network on the visitor.

---

## Phase E - leases for entry nodes other than the principal

Port the branch's Phase F design: the principal grants per (account, entry
node) allowance leases with an expiry (`x402_grants`); an entry node admits
against `min(balance, allowance) - reserved`, reports settlements to the
principal in batches (signed peer call), and asks for more below a
low-water mark. Principal away: leases run down, then refusal (fail closed).

Tests: an entry node admits within its lease and refuses past it; the
principal's balance reconciles the reported settlements; an expired lease
admits nothing.

---

## Phase F - screens and docs

- Deploy screen: prices per deployment (in, out per 1000 tokens).
- Payments screen (admin): accounts (visitor networks, their visitors),
  balances, grant, ledger with bundle links, the cycle's income beside the
  metering payouts.
- Network settings: "Charge visitors" (needs `member_gateway_only`; lists
  visitors that would be cut off).
- Visitor dashboard and Chat: balance, last charge, the 402 explained.
- `/v1/models` on a visitor: each model's prices.
- Guides (en, pt-BR), `docs/subsystems/payments.md` (ported and updated),
  `http-api.md`, `security-model.md` (home statements, delegation),
  `inference.md` (who gates), folder `AGENTS.md`s.

## Phase G - later

`pom-plugin-x402` top-up by the visitor (the 402 `accepts` envelope is already
x402-shaped); key accounts for members.

## Done when

- A visitor of a charging network can only use it through an entry node, is
  charged per token from its network's account, and gets a 402 when the
  account runs dry; an admin funds accounts and sees income per visitor network
  beside the metering payouts.
- A network that does not charge behaves exactly as before.
- Live check in the sealed two/three-node setup (`target/claude-live-metering`):
  a visitor node delegates, is charged, runs dry, is refused, is funded, is
  served.
