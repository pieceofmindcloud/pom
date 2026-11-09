# Metering GPU work: signed work receipts and probabilistic re-execution

Date: 2026-11-08 - Status: draft for iteration

## Problem

Workers and admins lend their GPUs to a network and the network wants to pay
them for the work their GPUs did. Today nothing a node reports about its own
work can be trusted for money:

- `backend/reqlog.rs` and the logs plugin count requests and tokens per peer,
  but the counts come from the node that wants to be paid, or from the gateway
  alone, with nothing the hops signed.
- Nothing ties a hop's output to the layer range and the model it claims to
  serve. A hop could run a smaller quantization, skip layers, or return junk,
  and bill for the full range.
- Token budgets (`backend/quota.rs`) govern consumption by clients, not
  production by machines.

Paying for work makes all three worth cheating on. This spec defines what a
node signs about the work it did, how that record reaches a ledger, and how the
ledger checks a random sample of it by doing the work again.

### Why not zero-knowledge proofs

Evaluated and rejected for this phase (see the analysis in the session that
produced this spec): zkVMs (SP1, RISC Zero) prove RISC-V programs, not GPU
kernels, and running a layer range inside one would cost 10^4 to 10^6 times the
native compute; zkML (EZKL and similar) works on small ONNX graphs and needs
minutes to hours per token for a 7B-70B model. ZK may come back later only to
prove the ledger's own arithmetic if settlement goes on-chain. Hiding the data
from the worker is not possible either way: the worker must read the hidden
state to compute on it. Data privacy from workers is out of scope (see the end).

## Goals

- Every unit of work that will be paid is backed by a record the worker signed
  **and** the gateway countersigned, so neither alone can invent work.
- Consecutive hops attest the same bytes crossed between them, so a claim
  cannot be stitched from work that never connected.
- A random, unpredictable sample of requests is re-executed by a node other
  than the one audited; a hop whose output does not match within tolerance
  loses that period's earnings and is flagged.
- Released nodes keep serving with no change. Metering is optional: a node
  without it serves as today and is simply not paid.
- The hot path pays at most a hash and a small summary per frame; no extra
  network round trip per token.

## Non-goals

- Pricing, currency, payment rails and on-chain settlement. The ledger produces
  per-node, per-period statements; how they are paid is the network's business.
- Staking and slashing beyond forfeiting unpaid earnings.
- Privacy of prompts from workers (needs GPU TEEs, a later tier).
- Metering tensor-parallel shards in the audit (they are receipted, see 9).

## Threat model

| Threat | Who | Covered by |
|---|---|---|
| Inflating token or request counts | a worker | gateway countersignature (2, 4) |
| Claiming work for requests that never happened | a worker | gateway countersignature, link digests (3) |
| Serving a cheaper model or quantization, skipping layers, returning junk | a worker | re-execution (6) against output summaries (3) |
| Lying about what a neighbour sent | a worker | both neighbours sign the same digest (3) |
| A gateway and a worker inventing traffic together | gateway + worker | partly: tie to client accounting (7); full defence out of scope |
| Replaying a receipt into another period or network | anyone | receipt id, network id, period, single submission (5) |

An admin running the ledger is trusted, as it is today with the request log.

## Decisions

### 1. Capability and switch

- A node that can produce receipts announces `metering/receipt-v1` beside its
  other inference capabilities. Old nodes do not announce it and are shown as
  "not metered".
- `NetworkSettings` gains `metering.enabled` (default `false`) and
  `metering.audit_rate` (default `0.02`). Only the principal changes them, as
  with every network setting. With metering off, no receipt is built and
  nothing on the hot path changes.
- The ledger lives in a new **metering plugin** (`pom-plugin-metering`),
  installed on admins, following the logs plugin pattern: the node collects and
  forwards, the plugin verifies, stores, aggregates and audits. Per-plugin
  licensing applies unchanged.

### 2. The work receipt

One receipt per **(request, hop)**: per KV session on that hop, closed when the
session ends. A compact token signed with the node key, using `sign_compact`
(`crates/node/src/membership.rs`) with its own header `pom-work-receipt/v1`, so
it cannot pass for a membership token or a relay pass.

Claims (draft):

| Field | Meaning |
|---|---|
| `rid` | receipt id (uuid) |
| `net` | network id |
| `req` | gateway request / trace id |
| `node` | issuing node id |
| `mtok` | `jti` of the membership token the node held (admins: none) |
| `gw` | gateway node id |
| `model` | model id and the digest of the GGUF (or slice manifest) served |
| `range` | `[first, last)` layers executed for this session (sub-range per hop, `inference.md` 3.8) |
| `engine` | engine and version (`llama.cpp`, a plugin engine) |
| `computed` | positions computed: `prefill`, `decode`, `draft` (speculative/MTP) |
| `cached` | positions reused from the prefix cache, not computed |
| `in_chain` / `out_chain` | link digests (3) |
| `summ` | digest of the output summaries kept for audit (3) |
| `t0` / `t1` | session open and close, node clock |
| `ms` | compute time, informational, never billed |

Billed unit, decided by the ledger, not the node: **layer-positions** =
`computed positions x layers in range`, weighted per model by the admin
(dense vs MoE active parameters). Cached positions are not billed. Whether
rejected draft positions are billed is open (question 3).

### 3. Link digests and output summaries

Each hop keeps two running digests per session, BLAKE3 over every frame item
it handled, in order:

- `in_chain = H(in_chain || positions || H(item bytes received))`
- `out_chain = H(out_chain || positions || H(item bytes sent))`

The bytes are the wire bytes (after encoding, so f8/i4 are covered as sent).
For a `frame/v1-batch` frame with several items, each session hashes only its
own item. Since the wire does not change bytes between hops, hop `n`'s
`out_chain` must equal hop `n+1`'s `in_chain`. The head's input is token ids:
its `in_chain` hashes the token ids, and the gateway computes the same value.
The tail's output is the sampled tokens.

For audit, each hop also keeps a compact **output summary** per computed
position, taken from the f32 output before wire encoding: the top-k indices by
magnitude and their values rounded to bf16 (k = 8 initially, ~48 bytes per
position). Re-execution compares against these (6). The summaries stay on the
node in a bounded store (`metering_summaries`, unified SQLite) for the
retention window (default 24 h) and only `summ`, their digest, goes in the
receipt. TOPLOC-style polynomial encoding is a candidate replacement for the
summary format (question 1); the receipt only carries a digest, so swapping it
needs no contract change beyond a version.

### 4. Collection: receipts ride the session release

No new frame field and no per-token call. The gateway already posts the
session lifecycle (`release`, `rollback`) to every step of the plan
(`backend/gateway/generation.rs`). A metered hop answers `release` with an
optional `receipt` field. The gateway then:

1. checks each receipt's signature, node, range against its plan, and that
   `out_chain[n] == in_chain[n+1]` and the head's `in_chain` matches the token
   ids it sent;
2. builds a **work bundle**: the plan, every receipt, the digest of the prompt
   and output token ids, the client API key identity (non-secret, as in the
   request log) and its own counts;
3. countersigns the bundle (`pom-work-bundle/v1`) and submits it to the metering
   plugin through `peer.metering.submit` (the signed plugin peer call,
   `plugin_host_api.rs`), to its own plugin first, else to each live admin
   running it, like `quota.rs` does.

A missing or invalid receipt does not fail the request; the bundle records the
hop as unmetered with the reason. A release that never arrives (crash,
timeout) leaves no receipt: that work is unpaid, and the hop may resend its
receipt on its next contact (`peer.metering.late`) for the gateway to attach,
within the retention window.

The gateway keeps the prompt and output token ids of each submitted bundle for
the retention window, so an audit can be run (6). They stay on the gateway, in
the same store and under the same access as the request log.

### 5. The ledger (metering plugin)

- Verifies the gateway signature, every receipt signature, membership of every
  node at `t0` (chain or valid token not revoked), link continuity, and that a
  `rid` was never seen before.
- Aggregates layer-positions per node per **period** (default one UTC day).
- At period close, after the audit window, issues a **period statement** per
  node: totals, the Merkle root of the accepted receipts, audit outcomes,
  signed by the admin. The node can fetch it and the proof that each of its
  receipts is in the root.
- Screens: per-node earnings by period, unmetered work and why, audits.

### 6. Probabilistic re-execution

**Selection.** After a bundle is accepted, the plugin selects it for audit with
probability `audit_rate`, raised for new nodes, nodes with a recent failure and
nodes with outlier counts. Selection is `HMAC(admin_secret, rid) < rate`, made
after submission, so no hop can know at compute time whether it will be checked.

**Re-execution.** The auditor fetches the token ids from the gateway and runs
one **teacher-forced prefill** of prompt + generated tokens with the same model
digest, capturing the output at each audited hop's layer boundary. One batched
prefill reproduces every position of every hop, decode included, so an audit
costs about one prefill of the whole conversation, not a replay of the
generation. Only computed positions are compared (cached ones have no summary).

**Who audits.** The admin running the plugin if it holds the model; otherwise a
verifier chosen at random among members that hold the needed ranges, never the
audited node nor the other hops of the same bundle, through a new peer
operation `peer.metering.reexecute`. A verifier's work is itself receipted and
paid at the audit rate.

**Comparison.** The auditor fetches the audited hop's summaries
(`peer.metering.summaries`), checks them against `summ`, and compares per
position: overlap of top-k index sets and relative error of the values, with
thresholds calibrated per model, KV cache type and backend (question 2).
Batched vs incremental execution and different GPUs do not give bit-equal
results, which is why the comparison is tolerant and why exact hashes are only
used for links.

**Outcomes.**

- `pass`.
- `fail`: the hop's period statement is withheld, the admin is alerted, its
  audit rate goes to 100% for the next N bundles; repeated failure leads to the
  existing signed revocation, decided by an admin.
- `inconclusive`: token ids or summaries no longer available, engine cannot
  capture the boundary. Not a failure, but a hop that cannot produce its
  summaries within the retention window forfeits that bundle.

### 7. Gateway honesty

A gateway and a worker together can invent traffic. Partial defence for this
phase: the ledger reconciles bundles with the request log and the client API
key accounting that the logs plugin already holds; a bundle with no matching
client request is not paid. Bundles whose gateway is a visitor are not paid
(members plan under `2026-11-07-member-gateway-planning-design.md`). A full
defence needs paying clients to sign their requests, deferred.

### 8. Engine requirements

- Capture the f32 output of a range before wire encoding: already there, the
  `wire` module encodes from it.
- For auditors: run a prefill with output capture at given layer boundaries.
  Sub-range execution (`inference.md` 3.8) gives this for llama.cpp; plugin
  engines (Strata, vLLM) need an `engine.*` capability for it, and until they
  announce it, audits of their hops are `inconclusive`.

### 9. Modes and special cases

- Ring, direct reply, remote token stream and hub all end with the same
  release, so collection is mode-independent. To verify: that every mode posts
  `release` to every step today.
- Tensor parallelism: each rank receipts its shard with a `shard` field; audit
  of TP shards is out of scope for phase 1.
- Speculative/MTP: draft positions counted in `computed.draft`, audited only on
  accepted positions.
- `idle` reloads and thermal throttling change nothing: only computed
  positions are billed.

## Compatibility

All additions are optional: a capability, an optional field in the `release`
answer, new plugin peer operations, a new store table and new settings with
defaults. Old gateways ignore the `receipt` field; old workers are unmetered.
No inference frame changes.

## Phases

1. Receipts, link digests, bundles and the ledger with statements; no audit.
   Counts become trustworthy against single-party inflation.
2. Output summaries, retention, re-execution by an admin holding the model.
3. Verifier selection among members, adaptive audit rates, plugin engines.

## Open questions

1. Summary format: top-k + bf16 values, or TOPLOC; k and positions sampled vs
   all positions.
2. Tolerance thresholds and how to calibrate them (a calibration run per
   model and backend on deploy?).
3. Are rejected speculative drafts paid?
4. Gateway compensation: is routing and the head's tokenization paid?
5. Retention window and storage cost of summaries on busy nodes.
6. Should the client's accounting be signed by the client for paid networks?

## Out of scope

Zero-knowledge proofs of inference, TEE attestation and prompt privacy from
workers, on-chain settlement, staking and slashing, pricing and payment rails,
auditing tensor-parallel shards.
