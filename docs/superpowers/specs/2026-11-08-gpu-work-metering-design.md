# Metering GPU work: signed work receipts and probabilistic re-execution

Date: 2026-11-08 - Status: draft for iteration (revision 5: verifier reproduces
lossy wire formats; TOPLOC calibrated at the end of each layer range)

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
  cycle the admin (or a dedicated verifier machine) re-executes a fixed number
  of samples per node. A node is paid in proportion to the samples it passed,
  so one failure costs a share of the cycle, not all of it.
- Pay follows the compute actually done; speculative draft positions are paid
  less than ordinary tokens.
- Work done for visitors is paid work: visitors are the network's paying
  customers, and every bundle records the consumer so the next task (charging
  visitors) can bill from the same records.
- A gateway can be paid for routing without other members learning how much.
- Everything kept for metering is bounded in payout cycles.
- Released nodes keep serving with no change. Metering is optional: a node
  without it serves as today and is simply not paid.
- The hot path pays at most a hash and a TOPLOC proof per chunk of tokens; no extra
  network round trip per token.

## Non-goals

- Pricing, currency, payment rails and on-chain settlement. The ledger produces
  per-node, per-cycle statements; how they are paid is the network's business.
- Staking and slashing beyond forfeiting unpaid earnings.
- Privacy of prompts from workers (needs GPU TEEs, a later tier).
- Auditing tensor-parallel shards (they are receipted, see 11).

## Threat model

| Threat | Who | Covered by |
|---|---|---|
| Inflating token or request counts | a worker | gateway countersignature (2, 4) |
| Claiming work for requests that never happened | a worker | gateway countersignature, link digests (3) |
| Serving a cheaper model or quantization, skipping layers, returning junk | a worker | challenges (6) and re-execution (7) against TOPLOC proofs (3) |
| Discarding proofs to dodge audits | a worker | challenges during the cycle (6) |
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
| `retain_cycles` | `2` | how many closed cycles of receipts, proofs and token ids are kept (12) |
| `samples_per_node` | `30` | receipts re-executed per node at cycle close (7) |
| `penalty_factor` | `2.0` | how much each failed sample weighs against pay (7) |
| `draft_weight` | `0.5` | pay weight of a speculative position relative to an ordinary one, one value for the whole network (9) |
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
| `wire` | hidden-state format negotiated on the hop's input and output (`f16`, `f32`, `f8`, `i4`), so the verifier reproduces it (7) |
| `computed` | positions run through `range`: `prefill`, `decode`, `verify_accepted`, `verify_rejected` (speculative positions checked by this hop) |
| `drafted` | draft tokens this hop generated with a draft head (MTP `nextn`), with the draft layers used |
| `cached` | positions reused from the prefix cache, not computed |
| `in_chain` / `out_chain` | link digests (3) |
| `summ` | Merkle root of the TOPLOC chunk proofs kept for audit (3) |
| `t0` / `t1` | session open and close, node clock |
| `ms` | compute time, informational, never billed |

### 3. Link digests and TOPLOC proofs

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

For audit, each hop also keeps an **output summary** of what it computed: a
**TOPLOC proof** (Prime Intellect, "TOPLOC: A Locality Sensitive Hashing Scheme
for Trustless Verifiable Inference", 2025), taken from the f32 output before
wire encoding.

How TOPLOC works, as the paper describes it: the computed positions are cut
into chunks (32 tokens in the paper); for each chunk, the top-k entries by
magnitude of the hidden state are encoded as a polynomial that maps each
index to its value, which takes a few hundred bytes per chunk (about 258 bytes
per 32 tokens in the paper, under 10 bytes per token). The verifier recomputes
the hidden state, takes its own top-k, and accepts when enough indices overlap
and the exponent and mantissa differences of the matching values stay under
thresholds. The paper reports that these thresholds tell apart a different
model, a different quantization or altered prompts, while tolerating different
GPUs, attention kernels and tensor-parallel layouts, which is the
nondeterminism POM has to tolerate.

POM's adaptations:

- The paper proves the last hidden layer; POM proves each hop's output at its
  range boundary, so every hop of a pipeline carries its own proof. The tail
  proves the last hidden layer, as in the paper. Thresholds for intermediate
  boundaries must be calibrated (question 1).
- Chunks follow the positions a hop computed in one session, cached positions
  excluded; a session's last chunk may be short.
- Chunk size, k and thresholds are network constants of the metering plugin,
  versioned (`toploc/v1`), so all nodes and the verifier use the same ones.
- The reference implementation (`toploc`, Python with C++ kernels) must be
  ported to Rust for the node, or bound through its C++ core; the license is
  to be checked before reuse.

The chunk proofs are leaves of a Merkle tree per receipt and only the root,
`summ`, goes in the receipt, so a challenge (6) can ask for a few chunks and
check them against the signed root without moving all of them. They stay on
the node in a bounded store (`metering_proofs`, unified SQLite), dropped by
cycle (12). At under 10 bytes per token, a busy node keeps a cycle of proofs in
tens of megabytes.

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
   prompt and output token ids, its own counts, and the **consumer**: the
   client API key identity (non-secret, as in the request log) and, when the
   request came from a visitor, the visitor's node id and network. The
   consumer is what the visitor charging task will bill from;
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
the retention period (12), so a challenge or an audit can re-execute it. They
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
the next one comes. A challenge asks for the TOPLOC proofs of a few random
chunks of that receipt plus their Merkle paths, and the node must answer
within a deadline (e.g. 60 s, it is a local read).

- The node proves it **still holds** what it signed: a node that discarded its
  proofs to make audits inconclusive fails here, while the cycle is still
  open.
- For a small share of challenges, the admin also re-executes those chunks
  (as in 7, prefill of only that request), which turns a challenge into an
  early audit. A re-executed challenge counts as one of the node's samples for
  the cycle (7); a failed one also raises the node's sample count at close.
- Outcomes: `answered`, `matched` (when re-executed), `failed` (wrong path,
  wrong proof, mismatch) and `timeout`. A timeout is retried; repeated
  timeouts count as a failure.

### 7. Re-execution at cycle close

When the cycle closes, the ledger stops accepting bundles for it, after a grace
period for late receipts, and the admin or the dedicated verifier
(`metering.verifier`) re-executes a sample of the cycle's bundles.

**Selection.** For each node, the ledger draws `samples_per_node` receipts
(20 to 30 is the intended range) from the node's accepted receipts of the
cycle, by `HMAC(admin_secret, rid || cycle)` order, so no hop can know at
compute time which work will be checked. A node with fewer receipts has all of
them checked. Re-executed challenges from the cycle count toward the samples.
The admin may raise the count for new nodes or nodes with a failed challenge.
Each sample is weighted by the layer-positions it carries, so a node cannot
hide cheating in its large requests behind honest small ones.

**Re-execution.** The verifier fetches the token ids from the gateway
(`peer.metering.tokens`) and runs one **teacher-forced prefill** of prompt +
generated tokens with the same model digest, capturing the output at each
audited hop's layer boundary. One batched prefill reproduces every position of
every hop, decode included, so an audit costs about one prefill of the whole
conversation, not a replay of the generation. Only computed positions are
compared (cached ones have no proof).

**Lossy wire formats.** When a hop's input crossed the wire in a lossy format
(f8 or i4 groupwise, negotiated per hop), the verifier reproduces what the hop
really received: it runs the plan's ranges in order and, at each boundary,
encodes and decodes the hidden state with the same codec and format the
receipt names (`wire`), before feeding the next range. The alternative, one
clean f32 pass through the whole model, was rejected:

- Its saving is small: one pass instead of one per range, plus a codec step
  per boundary, against a budget of about 30 samples per node per cycle.
- It judges a hop by noise it did not cause. Each lossy boundary adds error to
  the input of every later hop, so an honest hop late in an i4 pipeline drifts
  from the clean recomputation more than TOPLOC tolerates, and fails.
- Loosening the thresholds to absorb that drift opens room for exactly the
  cheating the check exists to catch (a cheaper quantization, skipped layers).

The codec is the node's own deterministic CPU code (the `wire` module), so the
verifier's encode and decode match the real ones bit for bit for the same
input. With the default f16 the two options barely differ.

**Who verifies.** The admin running the plugin, or the dedicated verifier
machine the admin named, which must hold the model ranges it checks and run
**the same engine** as the audited hop (llama.cpp and Strata do not give the
same numbers, so each is compared against itself). It is never one of the hops
of the bundle. Its work is not billed as inference.

**Comparison.** The verifier fetches the audited hop's chunk proofs
(`peer.metering.proofs`), checks them against `summ`, and runs the TOPLOC check
per chunk against its own recomputed hidden state (3). A sample passes when
every chunk passes. Batched and incremental execution and different GPUs do
not give bit-equal results, which is why the comparison is TOPLOC's tolerant
one and why exact hashes are only used for links.

**Sample outcomes.**

- `pass`.
- `fail`: a chunk fails the TOPLOC check, or the node cannot produce
  proofs it signed.
- `void`: the sample cannot be judged for a reason that is not the node's
  fault (the gateway lost the token ids, the verifier lacks the range). It is
  replaced by another draw and does not count; a gateway that cannot produce
  token ids forfeits its own fee for that bundle.

**Payout.** With `w_pass` and `w_fail` the layer-position weights of the passed
and failed samples:

```
fail_share = w_fail / (w_pass + w_fail)
payout     = cycle_earnings x max(0, 1 - penalty_factor x fail_share)
```

One failure out of 30 costs a proportional slice, not the cycle. The penalty
factor is above 1 on purpose: with plain proportional pay (`penalty_factor = 1`)
a node that returns junk for 20% of its work loses 20% of its pay but also
saves 20% of its compute, so cheating would break even; any factor above 1
makes it a loss. The admin can set it to 1 if the tolerance proves too noisy
for honest nodes. A `fail_share` above a threshold (e.g. 0.3) also alerts the
admin, who may revoke the node through the existing signed revocation.

### 8. Fabricated requests

A gateway and a worker together can send requests nobody asked for. To be paid,
such a bundle must pass audit, and passing audit requires TOPLOC proofs matching
the real model's output on those tokens, which means the GPU really did the
work. So the work is real; the only question is whether the network wants to
pay for demand it did not get. That is a pay policy, not a proof problem: an
admin can restrict pay to bundles whose consumer is someone the network bills
(a visitor, or an API key the logs plugin records). Once visitors are charged,
the problem mostly solves itself: a fabricated request names a consumer who
pays for it. Signed client requests are not needed.

**Visitors.** Work done for a visitor is paid like any other work; visitors are
where the network's income comes from. Two cases:

- Under `member_gateway` (`2026-11-07-member-gateway-planning-design.md`) the
  visitor hands the request to a member entry node, which is the gateway: it
  countersigns the bundle and names the visitor as consumer.
- Under `direct`, the visitor plans and is the gateway itself. Its
  countersignature is accepted for the hops' pay: the visitor is the one who
  will be charged, so it has no reason to inflate the work. A visitor never
  earns the gateway fee.

### 9. Pay rule and gateway compensation

**Layer-positions.** A hop is paid for `positions x layers` it actually ran,
weighted per model by the admin (dense vs MoE active parameters):

- `prefill` and `decode` positions over the hop's range, weight 1;
- `verify_accepted` and `verify_rejected` positions, weight `draft_weight`
  (default 0.5): they go through the full range, but checked as a batch, which
  costs a memory-bound GPU much less than the same number of decode steps;
- `drafted` tokens, weight `draft_weight`, and only for the draft layers that
  produced them (MTP drafts come from the `nextn` block, one layer, not the
  whole model);
- `cached` positions are not paid.

Accepted drafts paying less does not discourage speculation: a node with MTP
produces more tokens per second, so it still earns more per hour.

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

### 10. Engines: `engine.verify`

Receipts, link digests and proofs are made by the node, not the engine, so
they work for any engine whose hidden state crosses the node. That holds for
llama.cpp and for plugin engines with stages: the stage link
(`engine-contract.md`, "Stages of a split model") hands each tensor to the node
as `f32`, which then carries it to the next machine, so the node hashes and
summarizes it the same way.

Re-execution needs one more thing from the engine: a teacher-forced prefill of
given tokens that returns the hidden state at the end of the loaded range for
the asked positions, instead of a sampled token (the tail returns its last
hidden layer, before the output head). The node then builds the TOPLOC proofs
itself, with the same code as on the hot path. This becomes an optional
operation in the engine contract:

- `engine.verify {tokens, positions}` -> the range output for those positions
  (`f32`, in the stage link tensor format), announced as the capability
  `engine/verify-v1`.
- llama.cpp: the node implements it from the existing prefill and sub-range
  execution (`inference.md` 3.8).
- Strata: nothing stops it. It already serves any range of the whole model
  (`stage_ranges`, `--stage-layers`), so a verifier loads the audited range
  and runs the prefill; what is missing is the operation that returns the
  range output for every position rather than serving a request. It is a small
  addition to `pom-plugin-strata`, on the same branch work as the stages.

Until an engine announces `engine/verify-v1`, its samples are `void` and its
nodes are paid on receipts alone, which the admin sees on the cycle screen.

### 11. Modes and special cases

- Ring, direct reply, remote token stream and hub all end with the same
  release, so collection is mode-independent. To verify: that every mode posts
  `release` to every step today.
- Tensor parallelism: each rank receipts its shard with a `shard` field and is
  paid its share; auditing shards is out of scope for phase 1.
- `idle` reloads and thermal throttling change nothing: only computed positions
  are paid.

### 12. Retention in cycles

Everything kept for metering is dropped by cycle, never by age: receipts and
proofs on the hops, token ids on the gateways, bundles and statements in the
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

1. Receipts, link digests, bundles with the consumer, cycles and the ledger
   with private statements; no checking. Counts become trustworthy against
   single-party inflation, and the visitor charging task can start from the
   bundles.
2. TOPLOC in Rust, proofs on the hot path, `engine.verify` for llama.cpp,
   challenges during the cycle and sampled re-execution at cycle close with
   proportional payout.
3. Dedicated verifier machine, `engine.verify` in `pom-plugin-strata`, gateway
   fee, adaptive sample counts.

## Next task: charging visitors

Decided here so the bundles carry what it needs; designed in its own spec.

- Visitors are charged **per token**: prompt tokens computed and generated
  tokens, at prices the network's admin sets per model (prompt and output
  priced apart, cached prompt tokens possibly cheaper). Not per layer-position:
  the visitor buys answers, and the split of a model across machines is the
  network's business.
- The **account is the visitor's network**, not the visitor node: every bundle
  whose consumer is a visitor records the visitor network's id (4), and all
  visitors of that network draw on one account.
- Hops are paid in layer-positions and visitors charged in tokens; the
  difference between what visitors pay and what the cycle pays out is the
  network's margin.

## Open questions

1. Calibrating TOPLOC at the end of a layer range. The paper sets its
   thresholds on the last hidden layer; POM checks each hop's output at the
   end of its range, so chunk size, k and thresholds must be measured there,
   per model family and engine, including late hops of lossy (f8, i4)
   pipelines. A calibration run on deploy is one way. The result also sets how
   high `penalty_factor` can safely be.

## Out of scope

Zero-knowledge proofs of inference, TEE attestation and prompt privacy from
workers, on-chain settlement, staking and slashing, pricing and payment rails,
signed client requests, auditing tensor-parallel shards.
