# Subsystem: payments (charging visitors)

A network may charge the visitors that use its models with prepaid credit.
Code: `crates/node/src/payments/` (the x402 prepaid credit core and
`runtime.rs`), `crates/node/src/backend/delegation.rs` (the member gateway),
`crates/node/src/home_statement.rs`. Spec:
`docs/superpowers/specs/2026-11-09-visitor-charging-design.md`.

## Settings

`NetworkSettings` (saved and fanned out like every network setting):

- `inference_access`: `direct` (default, as before), `member_gateway`,
  `member_gateway_only`.
- `entry_nodes`: `admins` (default) or `admins_and_workers`.
- `payments`: `{enabled, prices: {<model>: {in_micro_per_1k, out_micro_per_1k}}}`.
  `enabled` requires `member_gateway_only` (`validate_network_settings`). A
  model without a price is free. Amounts are micro-credits (`1 = 1e-6`).

## The member gateway

Under `member_gateway*`, a visitor hands `/v1/chat/completions`,
`/v1/completions`, `/v1/messages` and `/api/generate` (after its own API key
check, `visitor_layer`) to an entry node with `POST /v1/gateway/delegate`, a
signed peer call. The entry node accepts only a visitor caller (`409
plan_locally` otherwise), runs the request through its own router marked
`Delegated`, and streams the answer back. Members announce
`gateway/delegate-v1`. Under `member_gateway_only` the inference port refuses
a visitor (`403 visitor_must_use_gateway`).

## Accounts and the gate

The account is the visitor's home network: `net:<home principal key>`, proven
by a home statement (compact token `<slug>-home-network/v1`, signed by the
home principal, naming the visitor's node key, checked against the caller's
transport key). A charged model without a statement is `403
home_statement_required`.

Before generation the entry node holds the worst case (prompt plus the
request's output limit at the model's price): an under-funded account gets
`402 payment_required` with the x402-shaped `accepts` envelope (scheme
`pom-credit`, `payTo` the principal), passed through to the visitor's client
as is; an unreadable book is `503`. At the generation summary the executed
tokens are settled and the rest of the hold is released; a failed request
releases. A settlement may take a balance below zero, which blocks the next
admission.

The principal holds the authoritative book and ledger (`x402_*` tables). Other
entry nodes admit against a lease the principal grants
(`/api/internal/payments/lease`) and report settlements
(`/api/internal/payments/settle`); with the principal away, leases run down
and requests are refused.

## Routes

- `GET /api/ui/payments` (principal): switch, prices, accounts, last 200 ledger rows.
- `POST /api/ui/payments/grant` (principal): `{account, amount_micro, note?}`.
- `GET /api/ui/payments/balance` (local): this visitor's balance, asked of an
  entry node through `POST /v1/gateway/balance`.

The admin screen is `/config/payments` (`apps/frontend/src/routes/Payments.tsx`).

## Not yet done

Prices live in the network settings, not on the deployment record; accounts
open on the first request, not at admission; home statements are only
self-signed (a visitor that is its home network's principal); `x_pom.payment`
and the visitor-side spend record are not written; workers still announce to
visitors (no topology hiding); the visitor's own route does not translate a
402 into its vocabulary. Funding is admin grants only (`pom-plugin-x402` later).
