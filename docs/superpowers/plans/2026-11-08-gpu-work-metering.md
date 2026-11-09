# Metering GPU work - implementation plan

Spec: `docs/superpowers/specs/2026-11-08-gpu-work-metering-design.md`
(revision 5). Read it first: this plan names its sections as "spec N".

Files and lines are from 2026-11-08; check `file:line` before relying on them.
Every task is TDD: failing test -> minimal implementation -> passing test ->
commit. Rust builds and tests run in the dev container (no cargo on the host).
Do not run `rustfmt` on the tree. Route tests use `gateway::testkit`, never
real UDP. Stage named paths only when committing.

## Status (2026-11-09)

Local branches, not pushed: POM `feat/gpu-work-metering` (phases 1-2 and the
testable part of 3), `pom-plugin-metering` `main`. The spec's "Implementation
notes" record where the code departs from this plan (the nonce rides the
rewind/release, the head returns its token sequence, re-execution is a job).

- [x] 1.1-1.7 node side; 1.8 the ledger plugin; 1.9 docs
- [x] 2.1 Merkle, 2.2 TOPLOC port (MIT, Prime Intellect), 2.3 proofs on the hot
  path, 2.4 re-execution (decoded boundaries), 2.6 pay and payout, 2.7
  challenges and sampling
- [ ] 2.5 calibration at range ends (needs real-model runs; thresholds are the
  paper's)
- [x] 3.1 dedicated verifier, 3.3 private gateway fee
- [ ] 3.2 Strata `engine.verify` and metering of plugin-engine hops
- [x] 3.4 adaptive sample counts (new node or failed challenge: twice the samples)
- [ ] tail proofs (the tail keeps none today)

## Shape of the work

| Where | What |
|---|---|
| `crates/node/src/metering/` (new) | pure logic: cycles, receipt and bundle tokens, link chains, Merkle tree, TOPLOC, pay rule, payout, codec round trip |
| `crates/node/src/inference/infer_server/` | per-session meter: counts, link chains, TOPLOC proofs; the receipt in the `release` answer |
| `crates/node/src/backend/gateway/generation.rs` | collect receipts, check them, build and countersign the bundle, submit it |
| `crates/node/src/backend/metering.rs` (new) | node side of the plugin: submit, late receipts, peer ops the plugin calls on hops and gateways, retention |
| `crates/node/src/utils/config.rs` | `NetworkSettings.metering` |
| `pom-plugin-metering` (new private repo) | the ledger: verification, accrual, challenges, sampling, re-execution, payout, statements, screens |
| `pom-plugin-strata` | `engine.verify` (phase 3) |

Hashes use SHA-256 from the existing `sha2` dependency instead of the spec's
BLAKE3, to add no crate; Task 1.2 amends the spec. All signatures use the node
key through `membership::sign_compact` / `open_compact`.

---

## Phase 1 - receipts, bundles, cycles, ledger (no checking)

Goal: trustworthy counts against single-party inflation, and bundles carrying
the consumer for the visitor charging task.

### Task 1.1 - `NetworkSettings.metering`

Files: `crates/node/src/utils/config.rs` (`NetworkSettings`, line ~149).

Test (`config.rs` tests): an old `pom.json` with no `metering` loads with
metering off and the defaults; a saved block round-trips.

```rust
#[test]
fn metering_settings_default_off_and_round_trip() {
    let old: NetworkSettings = serde_json::from_str("{}").unwrap();
    assert!(!old.metering.enabled);
    assert_eq!(old.metering.cycle_secs, 7 * 86_400);
    assert_eq!(old.metering.retain_cycles, 2);
    assert_eq!(old.metering.samples_per_node, 30);
    let mut s = old.clone();
    s.metering.enabled = true;
    s.metering.draft_weight = 0.4;
    let back: NetworkSettings =
        serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back.metering, s.metering);
}
```

Implementation:

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MeteringSettings {
    pub enabled: bool,
    /// Payout cycle length; cycles are numbered from METERING_EPOCH.
    pub cycle_secs: u64,
    /// Closed cycles kept besides the current one; at least 1.
    pub retain_cycles: u32,
    pub samples_per_node: u32,
    pub challenges_per_node: u32,
    pub penalty_factor: f64,
    pub draft_weight: f64,
    pub verifier: Option<String>,
    pub gateway_fee: GatewayFee,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayFee { pub per_request: f64, pub per_prompt_token: f64 }
impl Default for MeteringSettings {
    fn default() -> Self {
        Self { enabled: false, cycle_secs: 7 * 86_400, retain_cycles: 2,
               samples_per_node: 30, challenges_per_node: 5,
               penalty_factor: 2.0, draft_weight: 0.5, verifier: None,
               gateway_fee: GatewayFee::default() }
    }
}
```

Add `#[serde(default)] pub metering: MeteringSettings` to `NetworkSettings`.
`retain_cycles` is clamped to `>= 1` on load. Only the principal may change it:
check that the existing network settings route already enforces principal-only
writes for the whole struct (`http-api.md`, `security-model.md`); add a test if
not. Docs: `http-api.md` settings section.

### Task 1.2 - `metering` module: cycles and link chains

Files: new `crates/node/src/metering/mod.rs`, `cycle.rs`, `chain.rs`;
`mod metering;` in `main.rs`. Amend spec 3 to SHA-256.

Tests:

```rust
#[test]
fn cycle_numbers_agree_from_a_fixed_epoch() {
    let c = 7 * 86_400;
    assert_eq!(cycle_of(METERING_EPOCH_MS, c), 0);
    assert_eq!(cycle_of(METERING_EPOCH_MS + c * 1000 - 1, c), 0);
    assert_eq!(cycle_of(METERING_EPOCH_MS + c * 1000, c), 1);
    let (start, end) = cycle_bounds_ms(1, c);
    assert_eq!(end - start, c * 1000);
}

#[test]
fn sender_out_chain_equals_receiver_in_chain() {
    let (mut out, mut inp) = (LinkChain::default(), LinkChain::default());
    for (pos, bytes) in [(0u32..32, b"a".as_slice()), (32..33, b"b")] {
        out.absorb(pos.clone(), bytes);
        inp.absorb(pos, bytes);
    }
    assert_eq!(out.digest(), inp.digest());
}

#[test]
fn a_changed_byte_or_position_breaks_the_chain() {
    let mut a = LinkChain::default(); a.absorb(0..1, b"x");
    let mut b = LinkChain::default(); b.absorb(0..1, b"y");
    let mut c = LinkChain::default(); c.absorb(1..2, b"x");
    assert_ne!(a.digest(), b.digest());
    assert_ne!(a.digest(), c.digest());
}

#[test]
fn head_in_chain_hashes_token_ids_like_the_gateway() {
    let mut hop = LinkChain::default(); hop.absorb_tokens(0, &[1, 2, 3]);
    assert_eq!(hop.digest(), LinkChain::of_tokens(0, &[1, 2, 3]).digest());
}
```

Implementation sketch:

```rust
pub const METERING_EPOCH_MS: u64 = 1_767_225_600_000; // 2026-01-01T00:00Z
pub fn cycle_of(ts_ms: u64, cycle_secs: u64) -> u64 {
    ts_ms.saturating_sub(METERING_EPOCH_MS) / (cycle_secs * 1000)
}

#[derive(Clone, Default)]
pub struct LinkChain([u8; 32]);
impl LinkChain {
    pub fn absorb(&mut self, positions: std::ops::Range<u32>, bytes: &[u8]) {
        let item = Sha256::digest(bytes);
        let mut h = Sha256::new();
        h.update(self.0); h.update(positions.start.to_le_bytes());
        h.update(positions.end.to_le_bytes()); h.update(item);
        self.0 = h.finalize().into();
    }
    pub fn absorb_tokens(&mut self, first_pos: u32, tokens: &[i32]) { /* bytes = LE i32 */ }
    pub fn digest(&self) -> String { hex::encode(self.0) }
}
```

### Task 1.3 - receipt and bundle tokens

Files: `metering/receipt.rs`, `metering/bundle.rs`.

Headers: `pom-work-receipt/v1`, `pom-work-bundle/v1` (derive the prefix from
`brand` like the other compact tokens). Claims as spec 2 / spec 4, plus the
`wire` field (`{ "in": "f16", "out": "f8" }`).

Tests:

```rust
#[test]
fn a_receipt_opens_only_with_its_header_and_its_signer() {
    let key = SigningKey::generate(&mut OsRng);
    let token = sign_receipt(&sample_claims(&key), &key).unwrap();
    assert_eq!(open_receipt(&token).unwrap().rid, sample_claims(&key).rid);
    // A membership token or a bundle header does not open as a receipt.
    assert!(open_receipt(&membership_like_token(&key)).is_err());
    let mut forged = token.clone(); forged.push('A');
    assert!(open_receipt(&forged).is_err());
}

#[test]
fn receipt_claims_name_no_gateway() {
    let json = serde_json::to_value(sample_claims(&SigningKey::generate(&mut OsRng))).unwrap();
    assert!(json.get("gw").is_none());
    assert!(json.get("bnd").is_some());
}

#[test]
fn bundle_records_the_visitor_network_as_consumer() { /* Consumer { api_key_id, visitor_node, visitor_network } */ }
```

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReceiptClaims {
    pub rid: String, pub net: String, pub cyc: u64, pub bnd: String,
    pub node: String, #[serde(skip_serializing_if = "Option::is_none")] pub mtok: Option<String>,
    pub model: ModelRef, pub range: [u32; 2], pub engine: String,
    pub wire: WireFormats, pub computed: Computed, pub drafted: Drafted,
    pub cached: u32, pub in_chain: String, pub out_chain: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub summ: Option<String>, // phase 2
    pub t0: u64, pub t1: u64, pub ms: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Computed { pub prefill: u32, pub decode: u32, pub verify_accepted: u32, pub verify_rejected: u32 }
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Drafted { pub tokens: u32, pub layers: u32 }
```

`open_receipt` checks the signature by the key the claims name (`node` maps to
its public key through `node_id == SHA256(pubkey)`, as the TLS check does).

### Task 1.4 - the per-session meter in the inference server

Files: `inference/infer_server/prefix.rs` (`Session`, line ~403;
`InferState`, line ~852), `step.rs`, `prefill.rs`, `step_blocking.rs`,
`direct.rs`, `inference/link.rs`, `inference/plugin_stage_link.rs`.

Add `meter: Option<SessionMeter>` to `Session`, created on session open when
`InferState.metering` (new field, `Option<MeteringContext>` with the network
id, cycle length, node key handle and model ref) is `Some`.

```rust
pub(crate) struct SessionMeter {
    pub bnd: String, pub t0: u64,
    pub computed: Computed, pub drafted: Drafted, pub cached: u32,
    pub in_chain: LinkChain, pub out_chain: LinkChain,
    pub wire: WireFormats, pub compute_ms: u64,
}
```

The gateway hands `bnd` in the prefill frame meta as an optional
`metering_bundle` field (old peers ignore unknown meta fields - verify in
`wire::decode_inference_request`); the head forwards it down the chain in the
same meta. A session opened with no `bnd` gets no meter.

Hook points (one call each, behind `if let Some(m) = session.meter`):

| Event | Where (check) | Meter call |
|---|---|---|
| input item decoded | `step.rs:47`, `:72`, `:450`; `prefill.rs:126`; `direct.rs:584`, `:622`, `:768`; `link.rs` batch decode; head: token ids | `in_chain.absorb(positions, item_bytes)` / `absorb_tokens` |
| output item encoded | `step_blocking.rs:488`, `:658`, `:698`; `step.rs:334`, `:394`, `:499`; `direct.rs:904`; `plugin_stage_link.rs:101` | `out_chain.absorb(positions, item_bytes)` |
| prefill computed | prefill path, after the batch runs | `computed.prefill += n`, `cached += reused` |
| decode step | step path | `computed.decode += 1` |
| speculative round | `/v1/session/rollback` handler | `verify_accepted += accept_count`, `verify_rejected += batch_width - accept_count` |
| MTP draft | where `next_draft` is produced | `drafted.tokens += k`, `drafted.layers = n_nextn` |

For a multi-item frame, absorb only this session's item slice.

Tests (in `infer_server` tests, with the existing fake engine helpers):

```rust
#[test]
fn a_session_with_a_bundle_nonce_counts_prefill_decode_and_cache() { ... }
#[test]
fn a_session_without_a_nonce_has_no_meter_and_no_cost() { ... }
#[test]
fn rollback_splits_verified_positions_into_accepted_and_rejected() { ... }
#[test]
fn only_the_sessions_item_of_a_batch_frame_enters_its_chain() { ... }
```

And one lab test (`direct_lab.rs` or `link_lab.rs` style, fake peers): two
hops, the head's `out_chain` equals the tail's `in_chain` after a prefill and
three steps, in ring, direct and link-stream modes.

### Task 1.5 - receipt in the `release` answer

Files: `infer_server/server.rs` (`handle_release`, line ~1990; health caps,
line ~1555); `routing/pipeline.rs` (new `CAP_METERING_RECEIPT =
"metering/receipt-v1"`); `network/network.rs` `self_announce` and the gateway
local peer entry (`gateway/state.rs`, near `relay/v2-direct`), kept in sync as
the comment in `health_body` asks.

Before `s.finish(sid)`, take the session's meter, build `ReceiptClaims`
(`t1 = now`, `cyc = cycle_of(t0)`), sign, and add `"receipt": token` to the
JSON answer. The TP replica path (`replicate_session_call`) does not receipt
(rank 0 does, with `shard`; spec 11).

Tests:

```rust
#[tokio::test]
async fn release_of_a_metered_session_answers_a_signed_receipt() { ... }
#[tokio::test]
async fn release_of_an_unmetered_session_answers_as_before() {
    // exactly {released, free_slots, max_sessions}: old gateways see no change
}
#[test]
fn the_capability_is_announced_only_with_metering_enabled() { ... }
```

### Task 1.6 - gateway: collect, check, bundle, countersign

Files: `backend/gateway/generation.rs` (`release_sessions`, line ~2685; its
callers at ~2321, ~3063, ~3076; `post_session_lifecycle`, line ~2475).

- At plan time, when metering is enabled and this node is not a visitor or is
  a visitor in `direct` mode (spec 8), mint `bnd` (random 16 bytes, hex) and
  put it in the prefill meta.
- `release_sessions` returns `Vec<(node_id, Option<String>)>` (the receipt
  field of each answer) instead of `()`; the three callers pass it to
  `metering::bundle::assemble`.
- First verify the open item from the spec: every reply mode (remote stream,
  direct, ring, hub) reaches `release_sessions` for every step. Add a lab test
  per mode asserting each fake peer got a release; fix any mode that does not,
  and note the result in `inference.md`.

```rust
pub fn assemble(plan: &PlanView, bnd: &str, receipts: &[(String, Option<String>)],
                tokens: &TokenLog, consumer: Consumer) -> BundleClaims
```

Checks, each failure recording the hop as unmetered with a reason (spec 4):
signature, `node` matches the step, `range` matches the step, `bnd` matches,
`out_chain[n] == in_chain[n+1]`, head `in_chain == LinkChain::of_tokens(prompt)`.

Tests (pure, in `metering/bundle.rs`):

```rust
#[test] fn a_consistent_chain_of_receipts_is_accepted() { ... }
#[test] fn a_broken_link_marks_both_ends_unmetered_with_the_reason() { ... }
#[test] fn a_receipt_claiming_another_range_is_refused() { ... }
#[test] fn a_missing_receipt_leaves_the_others_metered() { ... }
```

### Task 1.7 - node side of the plugin: submit, late receipts, token log

Files: new `backend/metering.rs`, modeled on `backend/quota.rs` (sources:
own plugin first via `plugins.query_audit`, else each live admin through
`control_post_json_routed` to `/api/internal/plugins/metering/peer/<op>`).

- `submit(bundle_token)`: async, off the request path (`tokio::spawn`), bounded
  queue with retry; never delays the client's answer.
- Token log: the gateway stores prompt + output token ids per bundle in a new
  unified-store table `metering_tokens(bundle_id, cycle, tokens BLOB)`
  (`2026-09-02-pom-db-unificado-design.md` rules).
- Peer ops the plugin calls on this node (registered so `plugin_peer_call`
  routes them to the node, not to a plugin): `metering.tokens` (gateway: the
  token ids of a bundle, admins only), `metering.late` (hop: resend a receipt).
- Retention: on start and on each cycle change, delete rows whose cycle is
  older than `current - retain_cycles` (spec 12).

Tests (`gateway::testkit`):

```rust
#[tokio::test] async fn submit_goes_to_the_local_plugin_first_then_admins() { ... }
#[tokio::test] async fn a_slow_ledger_never_delays_the_answer() { ... }
#[tokio::test] async fn token_log_drops_cycles_outside_the_window() { ... }
#[tokio::test] async fn only_an_admin_may_read_a_bundles_tokens() { ... }
```

### Task 1.8 - `pom-plugin-metering` scaffold and ledger

New private repository, created with the `creating-pom-plugins` skill
(scaffold script, contract tests). Phase 1 scope:

- `peer.metering.submit`: open the bundle (gateway signature), open every
  receipt, check membership at `t0` (via the host API's peer roster and the
  trust chain; ask the node for a membership check op if the host API lacks
  one), `cyc` vs `t0`, `rid` uniqueness; store.
- Accrual of layer-positions per node per cycle with the pay rule (Task 2.6
  ships the formula; phase 1 accrues raw counts).
- Gateway fee accrual (spec 9) into the gateway's own total.
- Cycle close: after a grace period, freeze the cycle; statement = totals +
  Merkle root of accepted receipts (Task 2.1's tree), signed through
  `POST /api/plugin-host/sign`.
- Access: a node reads only its own statements (`peer.metering.statement`);
  the ledger admins read all. No network-wide totals exposed to members.
- Screens (`pom-plugin-ui/v1`): admin cycle overview, per-node detail,
  unmetered work and reasons; member "my earnings".
- Retention by cycle.

Contract tests in the plugin repo cover each op's refusals.

### Task 1.9 - docs for phase 1

Same commit as the last code task of the phase: `inference.md` (receipt in
release, `metering_bundle` meta, the mode check result), `http-api.md`
(settings, release answer), `security-model.md` (receipt and bundle tokens),
`enterprise-licensing.md` (the plugin), `crates/node/src/inference/AGENTS.md`
and `crates/node/AGENTS.md` (new module), guide for the new screens.

---

## Phase 2 - TOPLOC, `engine.verify`, challenges, sampled re-execution

### Task 2.1 - Merkle tree

`metering/merkle.rs`: SHA-256 binary tree, leaf/node domain separation, proof
generation and verification.

```rust
#[test] fn every_leaf_proves_against_the_root() { ... }
#[test] fn a_proof_for_another_leaf_or_root_fails() { ... }
#[test] fn odd_leaf_counts_and_a_single_leaf_work() { ... }
```

### Task 2.2 - TOPLOC in Rust

`metering/toploc.rs`. First: check the license of the reference `toploc`
implementation and read the paper's exact encoding; record both in the module
doc. Port the encoder and the checker (or bind the C++ core if the license and
build allow; porting is preferred, it keeps the binary dependency-free).

```rust
pub struct ToplocParams { pub chunk: u32, pub k: u32, pub thresholds: Thresholds }
pub fn prove_chunk(hidden: &[f32], width: usize, params: &ToplocParams) -> Vec<u8>;
pub fn check_chunk(proof: &[u8], recomputed: &[f32], width: usize, params: &ToplocParams) -> Verdict;
```

Tests: golden vectors from the reference implementation (proof bytes and
verdicts for fixed inputs); a recomputation with small f32 noise passes; a
hidden state from different weights fails; a short last chunk works.

### Task 2.3 - proofs on the hot path

`SessionMeter` gains `proofs: ChunkBuffer` fed with the f32 output before wire
encoding at the output hook points of Task 1.4 (the tensor is in hand before
`wire::encode*`). Full chunks go to a new store table
`metering_proofs(rid, chunk_index, cycle, proof BLOB)`; at release the
receipt's `summ` is the Merkle root of its chunks. Measure: proof cost per
token on the CUDA server and a Mac, recorded in a dated report under
`docs/reports/`; the budget is "no visible change in tok/s".

Tests: proofs are written only for computed positions; `summ` equals the root
of the stored chunks; the table drops by cycle.

### Task 2.4 - `engine.verify` for llama.cpp and the decoded re-execution

- Capability `engine/verify-v1`.
- Node op `metering.reexecute { model_digest, plan_ranges, wire_formats,
  tokens, audited: [{ hop, positions }] }`, accepted from an admin only, run on
  the verifier: runs the plan's ranges in order with sub-range execution
  (`inference.md` 3.8), and between ranges applies
  `wire::decode(wire::encode(meta, tensor, dtype))` with each boundary's
  format from the receipts (spec 7, decoded input); returns TOPLOC verdicts for
  the audited hops' chunks.
- The verifier refuses if its engine or model digest differs from the
  receipt's.

```rust
#[test] fn codec_round_trip_matches_the_hops_received_bytes() {
    // encode with f8 on one side, round trip on the verifier: identical f32
}
#[tokio::test] async fn reexecution_of_an_honest_two_hop_bundle_passes() { ... }
#[tokio::test] async fn a_hop_that_skipped_layers_fails() { ... }
#[tokio::test] async fn a_hop_with_a_lower_quantization_fails() { ... }
#[tokio::test] async fn a_late_hop_in_an_i4_pipeline_passes_with_decoded_input() { ... }
```

The last three need a small real model in the dev container (follow the
two-node test procedure kept in the operator's notes); mark them `#[ignore]`
for CI and run them by hand, recording results in the report.

### Task 2.5 - calibration at the end of a layer range

Script under `scripts/` plus a dated report: for each supported model family
and engine, run honest pipelines (different GPUs, f16/f8/i4 wires, late hops)
and dishonest ones (lower quant, skipped layers, other model), at range ends;
pick chunk, k and thresholds; record the false-fail rate of honest runs,
which bounds `penalty_factor`. The plugin ships the chosen `toploc/v1`
constants.

### Task 2.6 - pay rule and payout (pure, shared by node tests and plugin)

`metering/pay.rs`:

```rust
pub fn layer_positions(r: &ReceiptClaims, draft_weight: f64, model_weight: f64) -> f64 {
    let layers = (r.range[1] - r.range[0]) as f64;
    let full = (r.computed.prefill + r.computed.decode) as f64 * layers;
    let spec = (r.computed.verify_accepted + r.computed.verify_rejected) as f64 * layers;
    let draft = r.drafted.tokens as f64 * r.drafted.layers as f64;
    model_weight * (full + draft_weight * (spec + draft))
}
pub fn payout(earnings: f64, w_pass: f64, w_fail: f64, penalty: f64) -> f64 {
    let total = w_pass + w_fail;
    if total == 0.0 { return earnings; }
    earnings * (1.0 - penalty * w_fail / total).max(0.0)
}
```

```rust
#[test] fn cached_positions_are_not_paid() { ... }
#[test] fn speculative_positions_pay_draft_weight() { ... }
#[test] fn one_failed_sample_of_thirty_costs_a_proportional_slice() {
    assert!((payout(300.0, 29.0, 1.0, 2.0) - 280.0).abs() < 1e-9);
}
#[test] fn penalty_above_one_makes_cheating_a_loss() { ... }
```

The plugin depends on this logic: either publish it as a small public crate
(`crates/metering-core`, preferred, no node internals) or copy it with a
shared test vector file. Decide at the start of the task; moving `cycle`,
`chain`, `receipt`, `merkle`, `toploc`, `pay` into `crates/metering-core` from
the start is cleaner if the plugin is written in Rust.

### Task 2.7 - challenges and sampling in the plugin

- Node op `metering.proofs { rid, chunks }` (hop: chunk proofs + Merkle paths,
  admins only, deadline 60 s).
- Plugin: challenge scheduler (random times, `challenges_per_node` per cycle,
  HMAC selection, spec 6); a share re-executed through `metering.reexecute`.
- Cycle close: draw `samples_per_node` per node by HMAC order, replace `void`
  samples, weight by layer-positions, compute `payout` (Task 2.6), alert above
  the `fail_share` threshold, statement with challenge and audit outcomes.

Plugin contract tests: deterministic draw for a given secret; `void` replaced;
fewer receipts than samples checks all; a timeout retried then failed.

### Task 2.8 - docs for phase 2

`inference.md` (proofs, verify capability), `security-model.md`, a guide for
the audit screens, `crates/node/src/inference/AGENTS.md`, the calibration
report.

---

## Phase 3 - dedicated verifier, Strata, gateway fee, adaptive counts

- 3.1 `metering.verifier` honored: the plugin sends re-execution to that node;
  it must hold the ranges and the engine (refusal otherwise, sample `void`).
- 3.2 `pom-plugin-strata`: `engine.verify` returning the range output for
  given positions with `--stage-layers`; announce `engine/verify-v1`; the node
  routes `metering.reexecute` to the plugin engine for its hops (use the
  `pom-inference-engine` skill, `engine-contract.md`).
- 3.3 Gateway fee in statements, private (spec 9): a test that a member's
  statement read never returns another node's figures.
- 3.4 Adaptive sample counts for new nodes and nodes with failed challenges.
- 3.5 Docs and guides.

## Done when

- Phase 1: a three-node pipeline in the dev container produces bundles with
  linked receipts in every reply mode; the ledger shows per-node totals and a
  signed statement at cycle close; old nodes in the same network serve
  unchanged and show as not metered.
- Phase 2: a deliberately dishonest hop (lower quant) fails re-execution and
  its payout drops by the formula; honest late hops of an i4 pipeline pass.
- Phase 3: a Strata hop is verified; a dedicated verifier runs the audits.
