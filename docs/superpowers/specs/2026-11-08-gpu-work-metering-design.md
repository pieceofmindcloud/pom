# Metering GPU work: signed work receipts and probabilistic re-execution

Date: 2026-11-08 - Status: draft for iteration (revision 2: operator decisions
on verification timing, drafts, gateway pay and retention)

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
ledger checks it: random challenges during a payout cycle and a sampled
re-execution when the cycle closes.

### Why not zero-knowledge proofs

Evaluated and rejected for this phase: zkVMs (SP1, RISC Zero) prove RISC-V
programs, not GPU kernels, and running a layer range inside one would cost
10^4 to 10^6 times the native compute; zkML (EZKL and similar) works on small
ONNX graphs and needs minutes to hours per token for a 7B-70B model. ZK may
come back later only to prove the ledger's own arithmetic if settlement goes
on-chain. Hiding the data from the worker is not possible either way: the
worker must read the hidden state to compute on it. Data privacy from workers
is out of scope (see the end).

## Goals

- Every unit of work that will be paid is backed by a record the worker signed
  **and** the gateway countersigned, so neither alone can invent work.
- Consecutive hops attest the same bytes crossed between them, so a claim
  cannot be stitched from work that never connected.
- During a payout cycle the admin challenges nodes at random; at the end of the
  cycle the admin (or a dedicated verifier machine) re-executes a random sample
  of the cycle's work. A node that fails either loses that cycle's earnings
  and is flagged.
- Pay follows the compute actually done, including rejected speculative drafts
  when they cost a full forward pass.
- A gateway can be paid for routing without other members learning how much.
- Everything kept for metering is bounded in payout cycles.
- Released nodes keep serving with no change. Metering is optional: a node
  without it serves as today and is simply not paid.
- The hot path pays at most a hash and a small summary per frame; no extra
  network round trip per token.

## Non-goals

- Pricing, currency, payment rails and on-chain settlement. The ledger produces
  per-node, per-cycle statements; how they are paid is the network's business.
- Staking and slashing beyond forfeiting unpaid earnings.
- Privacy of prompts from workers (needs GPU TEEs, a later tier).
- Auditing tensor-parallel shards (they are receipted, see 10).

## Threat model

| Threat | Who | Covered by |
|---|---|---|
| Inflating token or request counts | a worker | gateway countersignature (2, 4) |
| Claiming work for requests that never happened | a worker | gateway countersignature, link digests (3) |
| Serving a cheaper model or quantization, skipping layers, returning junk | a worker | challenges (6) and re-execution (7) against output summaries (3) |
| Discarding summaries to dodge audits | a worker | challenges during the cycle (6) |
| Lying about what a neighbour sent | a worker | both neighbours sign the same digest (3) |
| Fabricating requests with a colluding gateway | gateway + worker | not worth it: a fabricated bundle passes audit only if the work was really done (8) |
| Replaying a receipt into another cycle or network | anyone | receipt id, network id, cycle, single submission (5) |
| Members learning a gateway's earnings | any member | private statements, no gateway id in hop receipts (9) |

An admin running the ledger is trusted, as it is today with the request log.

## Decisions

### 1. Capability, switch and payout cycle

- A node that can produce receipts announces `metering/receipt-v1` beside its
  other inference capabilities. Old nodes do not announce it and are shown as
  "not metered".
- `NetworkSettings` gains a `metering` block, changed only by the principal:

| Field | Default | Meaning |
|---|---|---|
| `enabled` | `false` | with metering off, no receipt is built and nothing on the hot path changes |
| `cycle` | `7d` | payout cycle length, set by the admin; cycles are numbered from a fixed epoch so every node agrees which cycle a timestamp falls in |
| `retain_cycles` | `2` | how many closed cycles of receipts, summaries and token ids are kept (11) |
| `audit_rate` | `0.02` | share of the cycle's bundles re-executed at cycle close (7) |
| `challenge_rate` | per node per cycle, e.g. `5` | random challenges sent during the cycle (6) |
| `verifier` | the ledger admin | node id of a dedicated verifier machine, optional |
| `gateway_fee` | `0` | gateway compensation rule (9) |

- The ledger lives in a new **metering plugin** (`pom-plugin-metering`),
  installed on admins, following the logs plugin pattern: the node collects and
  forwards, the plugin verifies, stores, aggregates, challenges and audits.
  Per-plugin licensing applies unchanged.

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
| `cyc` | payout cycle number at `t0` |
| `bnd` | opaque bundle nonce handed by the gateway with the session; not the gateway's id (9) |
| `node` | issuing node id |
| `mtok` | `jti` of the membership token the node held (admins: none) |
| `model` | model id and the digest of the GGUF (or slice manifest) served |
| `range` | `[first, last)` layers executed for this session (sub-range per hop, `inference.md` 3.8) |
| `engine` | engine and version (`llama.cpp`, a plugin engine) |
| `computed` | positions run through `range`: `prefill`, `decode`, `verify_accepted`, `verify_rejected` (speculative positions checked by this hop) |
| `drafted` | draft tokens this hop generated with a draft head (MTP `nextn`), with the draft layers used |
| `cached` | positions reused from the prefix cache, not computed |
| `in_chain` / `out_chain` | link digests (3) |
| `summ` | Merkle root of the output summaries kept for audit (3) |
| `t0` / `t1` | session open and close, node clock |
| `ms` | compute time, informational, never billed |

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
magnitude and their values rounded to bf16 (k = 8 initially, about 48 bytes per
position). The summaries are leaves of a Merkle tree per receipt and only the
root, `summ`, goes in the receipt, so a challenge (6) can ask for a few
positions and check them against the signed root without moving all of them.
They stay on the node in a bounded store (`metering_summaries`, unified
SQLite), dropped by cycle (11). TOPLOC-style encoding is a candidate
replacement for the summary format (question 1); the receipt only carries a
root, so swapping it needs a version, not a contract change.

### 4. Collection: receipts ride the session release

No new frame field and no per-token call. The gateway already posts the
session lifecycle to every step of the plan (`release_sessions` and
`rollback_sessions` in `backend/gateway/generation.rs`, `/v1/session/release`).
A metered hop answers `release` with an optional `receipt` field. The gateway
then:

1. checks each receipt's signature, node, range against its plan, its `bnd`,
   and that `out_chain[n] == in_chain[n+1]` and the head's `in_chain` matches
   the token ids it sent;
2. builds a **work bundle**: the plan, every receipt, the Merkle root of the
   prompt and output token ids, the client API key identity (non-secret, as in
   the request log) and its own counts;
3. countersigns the bundle (`pom-work-bundle/v1`) and submits it to the metering
   plugin through `peer.metering.submit` (the signed plugin peer call,
   `plugin_host_api.rs`), to its own plugin first, else to each live admin
   running it, like `quota.rs` does.

A missing or invalid receipt does not fail the request; the bundle records the
hop as unmetered with the reason. A release that never arrives (crash,
timeout) leaves no receipt: that work is unpaid, unless the hop resends its
receipt on its next contact (`peer.metering.late`) for the gateway to attach
before the cycle closes.

The gateway keeps the prompt and output token ids of each submitted bundle for
the retention period (11), so a challenge or an audit can re-execute it. They
stay on the gateway, in the same store and under the same access as the
request log.

### 5. The ledger (metering plugin)

- Verifies the gateway signature, every receipt signature, membership of every
  node at `t0` (chain or valid token not revoked), link continuity, that `cyc`
  matches `t0`, and that a `rid` was never seen before.
- Accrues **layer-positions** per node per cycle (pay rule in 9).
- Runs challenges during the cycle (6) and the audit at its close (7).
- After the audit, issues a **cycle statement** per node: totals, the Merkle
  root of the accepted receipts, challenge and audit outcomes, signed by the
  admin. Only that node and the ledger admins can read it (9); the node can
  fetch the proof that each of its receipts is in the root.
- Screens: for an admin, earnings per node per cycle, unmetered work and why,
  challenges and audits; for any other node, its own statements only.

### 6. Challenges during the cycle

At random times during the cycle, the admin challenges a node about one of its
accepted receipts from this cycle (`peer.metering.challenge`), chosen with
`HMAC(admin_secret, rid || cycle) < rate` and spread so a node cannot tell when
the next one comes. A challenge asks for the summaries of a few random
positions of that receipt plus their Merkle paths, and the node must answer
within a deadline (e.g. 60 s, it is a local read).

- The node proves it **still holds** what it signed: a node that discarded its
  summaries to make audits inconclusive fails here, while the cycle is still
  open.
- For a small share of challenges, the admin also re-executes those positions
  (as in 7, prefill of only that request), which turns a challenge into an
  early audit; a cheater found mid-cycle stops accruing at once, not at the
  close.
- Outcomes: `answered`, `matched` (when re-executed), `failed` (wrong path,
  wrong summary, mismatch) and `timeout`. A timeout is retried; repeated
  timeouts count as a failure.

### 7. Re-execution at cycle close

When the cycle closes, the ledger stops accepting bundles for it, after a grace
period for late receipts, and the admin or the dedicated verifier
(`metering.verifier`) re-executes a sample of the cycle's bundles.

**Selection.** Each bundle is selected with probability `audit_rate`, raised for
new nodes, nodes with a challenge failure in the cycle and nodes with outlier
counts. Selection is `HMAC(admin_secret, rid)` drawn at close, so no hop can
know at compute time whether it will be checked.

**Re-execution.** The verifier fetches the token ids from the gateway
(`peer.metering.tokens`) and runs one **teacher-forced prefill** of prompt +
generated tokens with the same model digest, capturing the output at each
audited hop's layer boundary. One batched prefill reproduces every position of
every hop, decode included, so an audit costs about one prefill of the whole
conversation, not a replay of the generation. Only computed positions are
compared (cached ones have no summary).

**Who verifies.** The admin running the plugin, or the dedicated verifier
machine the admin named, which must hold the model ranges it checks. It is
never one of the hops of the bundle. Its work is not billed as inference.

**Comparison.** The verifier fetches the audited hop's summaries
(`peer.metering.summaries`), checks them against `summ`, and compares per
position: overlap of top-k index sets and relative error of the values, with
thresholds calibrated per model, KV cache type and backend (question 2).
Batched and incremental execution and different GPUs do not give bit-equal
results, which is why the comparison is tolerant and why exact hashes are only
used for links.

**Outcomes.**

- `pass`.
- `fail`: the node's cycle statement is withheld, the admin is alerted and the
  node is challenged and audited at 100% in the next cycle; repeated failure
  leads to the existing signed revocation, decided by an admin.
- `inconclusive`: token ids or summaries no longer available, or the engine
  cannot capture the boundary. A hop that cannot produce summaries it signed
  forfeits that bundle; a gateway that cannot produce token ids forfeits its
  own fee for it, and the hops are not penalized.

### 8. Fabricated requests

A gateway and a worker together can send requests nobody asked for. To be paid,
such a bundle must pass audit, and passing audit requires summaries matching
the real model's output on those tokens, which means the GPU really did the
work. So the work is real; the only question is whether the network wants to
pay for demand it did not get. That is a pay policy, not a proof problem: an
admin can restrict pay to bundles whose client API key identity matches the
keys the network bills (the logs plugin already records it), and bundles whose
gateway is a visitor are never paid (members plan under
`2026-11-07-member-gateway-planning-design.md`). Signed client requests are
not needed.

### 9. Pay rule and gateway compensation

**Layer-positions.** A hop is paid for `positions x layers` it actually ran,
weighted per model by the admin (dense vs MoE active parameters):

- `prefill`, `decode` and `verify_accepted` positions over the hop's range;
- `verify_rejected` positions are paid the same: a rejected draft still went
  through the full range in the verification pass, so it cost a full forward;
- `drafted` tokens are paid only for the draft layers that produced them (MTP
  drafts come from the `nextn` block, one layer, not the whole model), so a
  cheap draft never pays as a full token;
- `cached` positions are not paid.

The rule is "pay for layers executed per position", so a new speculative
method falls into it by reporting what it ran.

**Gateway fee.** The gateway is paid by `metering.gateway_fee`, a fixed amount
per request plus per prompt token tokenized, set by the admin. To keep it
private from other members:

- hop receipts carry the opaque `bnd`, not the gateway's id; only the bundle,
  which goes to the ledger alone, names the gateway;
- the fee accrues only in the gateway's own cycle statement, readable only by
  that node and the ledger admins; the ledger publishes no network-wide totals,
  rankings or per-node figures to members;
- the statement shows total earnings, not a separate gateway line, unless the
  node itself asks for the breakdown.

A hop still knows a given node called it (mutual TLS), as it does today; it
learns nothing about how many requests that node routes elsewhere or what it is
paid.

### 10. Modes and special cases

- Ring, direct reply, remote token stream and hub all end with the same
  release, so collection is mode-independent. To verify: that every mode posts
  `release` to every step today.
- Tensor parallelism: each rank receipts its shard with a `shard` field and is
  paid its share; auditing shards is out of scope for phase 1.
- `idle` reloads and thermal throttling change nothing: only computed positions
  are paid.

### 11. Retention in cycles

Everything kept for metering is dropped by cycle, never by age: receipts and
summaries on the hops, token ids on the gateways, bundles and statements in the
ledger are kept for the current cycle plus the last `retain_cycles` closed
ones. `retain_cycles` is at least 1, so the cycle being verified is never
dropped before its audit ends. A node drops a cycle's data when it falls out of
the window, without being asked.

## Compatibility

All additions are optional: a capability, an optional field in the `release`
answer, new plugin peer operations, new store tables and new settings with
defaults. Old gateways ignore the `receipt` field; old workers are unmetered.
No inference frame changes.

## Phases

1. Receipts, link digests, bundles, cycles and the ledger with private
   statements; no checking. Counts become trustworthy against single-party
   inflation.
2. Output summaries, challenges during the cycle and re-execution at cycle
   close by the admin.
3. Dedicated verifier machine, adaptive challenge and audit rates, gateway fee,
   plugin engines.

## Open questions

1. Summary format: top-k + bf16 values, or TOPLOC; k, and all positions or a
   sample.
2. Tolerance thresholds and how to calibrate them (a calibration run per model
   and backend on deploy?).
3. Weight of batched verification: a rejected draft verified in a batch costs
   less wall time than a decode step on a memory-bound GPU. Is "same layers,
   same pay" right, or should batched positions be discounted?
4. Engine support for capturing a layer boundary in plugin engines (Strata,
   vLLM): until they announce it, their audits are inconclusive. Is that
   acceptable for paying them?
5. A cycle statement withheld for one failed audit: forfeit the whole cycle, or
   only the work since the last passed check?

## Out of scope

Zero-knowledge proofs of inference, TEE attestation and prompt privacy from
workers, on-chain settlement, staking and slashing, pricing and payment rails,
signed client requests, auditing tensor-parallel shards.
