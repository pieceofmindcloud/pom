# Charging visitors for inference

Date: 2026-11-09 - Status: draft for iteration

## Problem

A network's income comes from the visitors that use its models. GPU work
metering (`2026-11-08-gpu-work-metering-design.md`) pays the machines that
did the work; nothing yet charges the visitors for it. Three facts shape the
answer:

- **A visitor plans its own requests today** (`direct`): it is the gateway of
  its requests and talks to every hop. A gateway that charges itself cannot be
  trusted to.
- **A prepaid credit core already exists, unmerged**, on `feat/x402_research`
  (`crates/node/src/payments/`, `docs/subsystems/payments.md` there, plan
  `2026-10-13-x402-prepaid-credit.md`): in-memory holds at admission, settlement
  from the generation's executed token counts, per-deployment prices for input
  and output tokens, the principal as the only merchant, allowance leases for
  other gateways, a durable ledger, and an x402-shaped 402 answer used only at
  the funding boundary. Its accounts are API keys.
- **The member gateway proposal** (`2026-11-07-member-gateway-planning-design.md`)
  makes a visitor hand each request to a member entry node, which plans and runs
  it. That entry node is a member, so it can charge.

## Decisions taken

- Build on the x402 prepaid credit core.
- Prepaid credit: a visitor network funds a balance; requests run against it.
- Enforce at the member gateway: charging needs visitors to delegate.
- Price per token (input and output, per deployment); the account is the
  **visitor's network** (from the metering spec).

## Goals

- Every request of a visitor of a charging network is admitted against the
  balance of the visitor's network, held at admission and settled from the
  executed token counts, with no network call or disk I/O in the inference
  path (the x402 core's constraint).
- A visitor cannot use the network's models without going through an entry
  node of the network, so it cannot avoid the charge.
- All the visitors of one network draw on one account; the visitor network
  proves which network a visitor belongs to, and no IP is ever an identity.
- The network's admin sets prices, funds accounts (phase 1: grants), and sees
  balances and the ledger; a visitor sees its network's balance and why a
  request was refused.
- Charges and metering payouts read the same requests, so a network can tell
  its income from its costs.
- Off by default; a network that does not charge behaves as today.

## Non-goals

- On-chain settlement or a payment provider in phase 1 (the x402 plan's
  `pom-plugin-x402` facilitator is a later phase and plugs into the same
  funding boundary).
- Charging members (admins, workers) for their own requests. The x402 core's
  API key accounts stay available for that, but are not part of this task.
- Refunds for poor answers, disputes, invoicing documents.

## Decisions

### 1. The account is the visitor's network

Every node has a network of its own (`bootstrap_local_network`), with a trust
chain whose principal key names it. The **account id is the visitor network's
principal key** (`net:<pubkey>`, the key, never an address or a name). A
visitor shows which network it belongs to with a **home statement**: a
compact token (header `<slug>-home-network/v1`) signed by its home network's
principal, naming the visitor node's key, the home network's cluster id, and
an expiry (a day, renewed by the visitor's node while it visits). A node that
is the principal of its own network (the common case: one machine, one
network) signs its own statement.

The home statement travels with the admission request and with every
delegated request. The host checks the signature and that the visitor node in
the statement is the caller (mutual TLS proves the caller's key). A visitor
whose home network changes principal (a principal transfer) presents a
statement signed by the new principal; the host follows the home network's
chain transfer and keeps the account (open question 2).

### 2. Admission binds a visitor to an account

When an admin approves a visitor, the approval names the account: the
visitor's home network. The first visitor of a home network opens its account
(balance zero, or an initial grant the admin types in the approval); later
visitors of the same home network join it with no new account. The admission
list shows the account beside each visitor; an admin can cap one visitor's
share of its account (open question 4).

### 3. Charging requires delegation

A network that charges (`payments.enabled` on, with at least one priced
deployment) must run `inference_access = member_gateway_only`: the workers
refuse visitors on the inference port and the tunnel, and visitors learn only
entry nodes. Turning charging on switches the policy, after showing the
visitors that do not announce `gateway/delegate-v1` and would be cut off. A
visitor's `direct` request to a worker of such a network is refused with
`visitor_must_use_gateway`.

The member gateway spec is implemented as part of this task (phase B): the
`inference_access` and `entry_nodes` policy, `POST /v1/gateway/delegate`, the
visitor's path (entry node choice, stream relay, cancellation) and the
workers' refusal.

### 4. The gate runs on the entry node

The entry node runs a delegated request through the same handler as a local
one, with the x402 gate (the core's Phase D) before generation:

- the account is the delegating visitor's network (from the home statement;
  a local client's API key stays free unless the key accounts are switched on);
- the hold is the core's `hold_micro` with the request's effective output
  limit (the inviolable client ceiling, never one leg);
- under-funded: `402 payment_required` with the core's `accepts` envelope,
  carried back to the visitor, whose own route answers its client with the
  same 402 in the route's vocabulary;
- the book unavailable: `503 payment_unavailable` (fail closed).

Settlement happens at the terminal `GenSummary` on the entry node, with the
executed counts (cached prompt tokens not billed), and releases the unspent
hold. A cancelled or failed request settles what was executed.

### 5. Balances, leases and the merchant

The principal is the only merchant and the only writer of the authoritative
ledger (the core's decision). An entry node that is not the principal admits
against an **allowance lease** the principal grants it per account, spends
locally, and reports settlements back (the core's Phase F). When a lease runs
low the entry node asks for more before it runs out; when the principal is
away, leases run down and the requests of that account are refused once they
do (fail closed). A secondary admin entry node behaves like any other.

### 6. Funding

Phase 1 is internal credit: the admin grants credit to an account (after
being paid outside POM), from the Payments screen. The 402 envelope keeps the
x402 `accepts` shape with the internal scheme `pom-credit`, so the later
`pom-plugin-x402` facilitator is a drop-in for top-up by the visitor itself.

### 7. Prices

Per deployment, in micro-credits per 1000 input and output tokens (the core's
`DeploymentPricing`, carried on the deployment record; a price change forks a
version). A deployment without a price is free even when charging is on. A
visitor's `/v1/models` shows each model's prices.

### 8. What the visitor sees

- Its network's balance on the host, through its entry node (a member peer
  call answered from the entry node's book, or the principal's), on its
  dashboard and in `x_pom.payment` of each answer: the charge, the balance
  after it.
- A 402 with the amount it lacks and the merchant (`pk:<principal>`).
- Its own node keeps a local record of what it spent per host network.

### 9. Link with GPU work metering

Every metered bundle already names the consumer (`Consumer.visitor_network`).
The settlement writes the bundle id beside the ledger row, so the admin's
cycle view shows, for each cycle, income per visitor network and the payouts
of the metering ledger: the network's margin. Both read the same request; a
request with no bundle (metering off) is still charged.

## Compatibility

- Additions only: policy fields, a token kind, a route, a capability, optional
  answer fields, new tables (the core's `x402_*`), a field on the deployment
  record (the core's `DEPLOYMENT_VERSION = 3`, which reads older records).
- A released visitor cannot use a charging network (it does not delegate);
  the settings screen lists such visitors before charging is turned on, and the
  release notes say so.
- Charging off: nothing changes for anyone.

## Phases

A. **Bring the x402 core forward**: rebase `crates/node/src/payments/` from
   `feat/x402_research` onto this branch, keep its tests, generalize the
   account id to `key:<id>` / `net:<pubkey>`.
B. **Member gateway**: as its spec, `member_gateway_only` at least.
C. **Home statements and accounts at admission**.
D. **Gate and settlement on the entry node**, 402 relayed to the visitor.
E. **Leases** for entry nodes other than the principal.
F. **Screens**: prices on the deploy screen, the Payments screen (accounts,
   grants, ledger), the visitor's balance; guides.
G. Later: `pom-plugin-x402` top-up; key accounts for members.

## Open questions

1. Credit unit: an abstract credit, or a named currency per network?
2. A home network's principal transfer: follow the chain automatically, or
   ask the host admin to confirm the account moves?
3. A minimum balance or a negative floor: the core lets a settlement go
   negative (it blocks the next admission); keep that, or hold a margin?
4. Per-visitor caps inside a network account: needed in phase 1?
5. Free allowance per visitor network (a trial), or none?

## Out of scope

Payment providers and chains, invoices and taxes, charging members, refunds
and disputes.
