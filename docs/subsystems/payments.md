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

## Prices

A deployment's price is `RuntimeParams.pricing` (`{in_micro_per_1k,
out_micro_per_1k}`), set in the deploy inspector. It lives in each version, so
a price change forks a version and reactivating one brings its price back.
When a record is published (`deploy::lifecycle::publish_record`), the active
version's price is written into `payments.prices` for the model's label: on
the principal directly, from another admin through
`POST /api/internal/payments/price`. Absent leaves the network's price as it
is; zero and zero removes it. The network's price table, which every entry node
reads, can also be edited on the Payments screen.

## The member gateway

Under `member_gateway*`, a visitor hands `/v1/chat/completions`,
`/v1/completions`, `/v1/messages` and `/api/generate` (after its own API key
check, `visitor_layer`) to an entry node with `POST /v1/gateway/delegate`, a
signed peer call. The entry node accepts only a visitor caller (`409
plan_locally` otherwise), runs the request through its own router marked
`Delegated`, and streams the answer back. Members announce
`gateway/delegate-v1`. Under `member_gateway_only` the inference port refuses
a visitor (`403 visitor_must_use_gateway`), and visitors learn only the entry
nodes the policy allows (`Network::set_visitor_view`, kept from the settings
every 5 s): a `PeerList` to a visitor leaves the others out, and a node that
is not an allowed entry node neither announces to visitors nor answers their
`PeerRequest`.

## Accounts

The account is the visitor's home network: `net:<home principal key>`, proven
by a home statement (compact token `<slug>-home-network/v1`, signed by the
home principal, naming the visitor's node key, checked against the caller's
transport key; `crate::home_statement`). A node that is its home network's
principal signs its own (a day). A member of a larger home network gets one
from its principal (`POST /api/internal/home-statement`, a week) while it is
home, renewed every hour past half its life, and keeps it in `pom.db`
(`home_statement`) to show while it visits.

The statement also travels with the admission request (`LobbyRequest::Admission.home`,
optional): the host records the account on the admission record
(`AdmissionRecord.account`), and an approval may grant it credit
(`AdmissionDecision.grant_micro`). An account otherwise opens on the first
request. The Payments screen lists each account's visitors from the admission
records.

## The gate

Before generation the entry node holds the worst case (prompt plus the
request's output limit at the model's price). Refusals answer in the vocabulary
of the route the client called (`delegation::speak`: OpenAI `error{message,type,code}`,
Anthropic `{type:"error",error:{type,message}}`, `/api/generate` `{error,code}`),
with the x402 `accepts` envelope (scheme `pom-credit`, `payTo` the principal)
and the account beside it: `402 payment_required` when the account cannot
cover the request, `503 payment_unavailable` when the book cannot be read,
`403 home_statement_required` without a statement. At the generation summary
the executed tokens are settled before the summary reaches the client, so the
answer carries `x_pom.payment {account, charged_micro, balance_micro}` (also
`payment` on `/api/generate`); the rest of the hold is released, and a failed
request releases it all. A settlement may take a balance below zero, which
blocks the next admission. On an entry node other than the principal,
`balance_micro` is what is left of its lease.

The principal holds the authoritative book and ledger (`x402_*` tables). Other
entry nodes admit against a lease the principal grants
(`/api/internal/payments/lease`) and report settlements
(`/api/internal/payments/settle`); with the principal away, leases run down
and requests are refused.

## The visitor's side

`/v1/models` lists each charged model's `pricing` (OpenRouter's `prompt` and
`completion` per token, in credits, plus the micro-credit figures). A Chat
request refused with 402 says the network's credit ran out and links to the
Credit page.


The visitor reads `x_pom.payment` at the end of each relayed answer
(`delegation::tap_payment`) and records it in `x402_spend` per host network.
`/config/credit` (`apps/frontend/src/routes/MyCredit.tsx`) shows the balance,
the prices, what this machine spent, and asks for credit
(`POST /api/ui/payments/request` -> an entry node's `/v1/gateway/credit-request`
-> the principal's `x402_credit_requests`). The admin approves (a grant) or
declines it on the Payments screen.

## Routes

- `GET /api/ui/payments` (principal): switch, prices, accounts with their
  visitors, credit requests, income by cycle, visitors that would be cut off
  (`cut_off`), last 200 ledger rows.
- `POST /api/ui/payments/grant` (principal): `{account, amount_micro, note?}`.
- `POST /api/ui/payments/decide` (principal): `{id, decision: approve|decline, amount_micro?}`.
- `GET /api/ui/payments/balance`, `POST /api/ui/payments/request` (local, a visitor's own).
- Between nodes: `/v1/gateway/{delegate,balance,credit-request}` (a visitor to
  an entry node), `/api/internal/payments/{lease,settle,balance,price,credit-request}`
  (a member to the principal), `/api/internal/home-statement` (a member to its
  home principal).

The admin screen is `/config/payments` (`apps/frontend/src/routes/Payments.tsx`).

## Compatibility (for the release notes)

- Visitors announce `gateway/client-v1`. A visitor of an earlier release does
  not delegate, so a network running `member_gateway_only` (required to
  charge) cuts it off; the Payments screen lists such visitors in sight and
  asks twice before turning charging on.
- Everything else is additive: optional fields on network settings, the
  admission request and record, `RuntimeParams.pricing`, `GenSummary` and
  `/v1/models` (`pricing`); new tables and routes. A network that does not
  charge behaves as before.

## Income beside payouts

A settlement whose request was metered names the bundle and cycle in the
ledger row's detail (`{"bundle","cycle"}`). `GET /api/ui/payments` returns
`cycles`: income per cycle and per account, beside the local metering
plugin's `rpc.overview` for that cycle (statements' payouts once closed,
earnings so far while open, `null` without the plugin), and the income
charged with metering off. Payouts are in the metering ledger's units, not
credits.

## Limits

- A home network's principal transfer opens a new account: the statement's
  earlier principals are claimed by its signer and cannot be checked by the
  host, so they are not used to move an account.
- A member's statement cannot be renewed while it visits; it lasts a week.
- Funding is the admin's (grants, approved credit requests); a payment
  provider (`pom-plugin-x402`) is later work.
