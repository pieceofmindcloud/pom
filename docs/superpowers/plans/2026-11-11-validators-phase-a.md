# Validators, phase A - implementation plan

Spec: `docs/superpowers/specs/2026-11-10-validators-design.md`, Part 1 with
`validators_see = tokens` (re-execution by token replay, as the verifier does
today). Phases B (trusted ends) and C (hidden-state re-execution) are later
plans.

Files and lines are from 2026-11-11; check `file:line` before relying on them.
Every task is TDD: failing test -> minimal implementation -> passing test ->
commit. Rust builds and tests of the node run in the dev container; never
`rustfmt` the POM tree; route tests use `gateway::testkit`; stage named paths.
Repositories: POM (`pom-salem`), the ledger plugin (`../pom-plugin-metering`),
`crates/metering-core` (in POM, a path dependency of the plugin).
Branches: POM `feat/validators` from `feat/visitor-charging`; plugin
`feat/validators` from `main`.

## Defaults taken for the spec's open questions

1. Validators per sample: 2 (`validators_per_sample`), and the ledger's admin
   decides a split when it can run the sample.
2. Wait bound: one cycle (`validation_wait_cycles = 1`).
3. Strikes: 3 per cycle pause a validator for the next cycle; removal stays
   the admin's decision (no automatic removal).
4. Pay from a share of worker pay only (no credits conversion in phase A).
5. Several validators per operator are allowed; the optional operator hint
   only keeps two of the same operator off one sample.
6. Not relevant to phase A (trusted ends are phase B).

## Deviation from the spec found while planning

**Canaries.** The spec mixes in samples corrupted with the calibration's
`skip:N` variants. A validator receives those as a re-execution request
marked `calibration` with its variant, so it could tell a canary from a real
sample and compute only canaries. And once validators return their own proofs
(spec decision 4) a lazy validator cannot fake a match anyway: what is left is
a validator copying a colluding worker's proofs. Phase A's canary is instead a
**reference sample**: a real receipt the ledger's admin re-runs itself, sent
to validators like any other; their proofs are compared with the admin's. A
copying validator is caught whenever its sample is a reference one, which it
cannot tell. The spec is updated in the same commit as task P5.

**Comparing proofs with proofs.** Today's TOPLOC check
(`toploc::check_chunk(proof, recomputed, params)`,
`crates/metering-core/src/toploc.rs:251`) compares a committed proof with the
**raw values** the verifier recomputed. Validators now send proofs, so the
ledger compares proof with proof: the top-k positions of both, and the values
at the positions they share. That comparison needs its own limits (task C2)
before it decides anything.

---

## Part 1 - `metering-core` (in POM)

### C1. Settings

`crates/metering-core/src/settings.rs` (`MeteringSettings`, line 7): add, all
`#[serde(default)]`:

- `validators_per_sample: u32` (default 2),
- `validator_rate: f64` (0.1, a validator's pay per layer position re-run, as
  a fraction of a worker's),
- `validator_share: f64` (0.05, cap on the cycle's worker pay that goes to
  validators),
- `validator_strikes: u32` (3),
- `validation_wait_cycles: u32` (1),
- `reference_rate: f64` (0.1, the share of samples the ledger's admin also
  re-runs as a reference).

`normalized()` clamps them (`validators_per_sample` 1..=5, shares 0..=0.5).

Tests: an older settings document reads with the defaults; `normalized`
clamps.

### C2. Proof against proof

`crates/metering-core/src/toploc.rs`: `compare_proofs(a: &[u8], b: &[u8],
params: &Params) -> Result<Verdict>`: decode both, intersect the top-k
positions, count exponent mismatches and mantissa differences on the shared
positions, and fail when the overlap is below `params.min_overlap` (new field,
default from C3) or a limit is crossed. Same `Verdict` shape as `check_chunk`.

Tests: identical proofs pass; proofs of the same values with small noise
pass; proofs of values from a skipped layer fail (fixtures built with
`prove_chunk` from synthetic rows, as the existing toploc tests do).

### C3. Limits for proof comparison, from a calibration

Not code first: run the plugin's `rpc.calibrate` on the live three-node
setup (`target/claude-live-metering`, memory note `live-metering-cpu-run`),
now returning the verifier's own proofs (task N2), and record honest vs
`skip:1` / `skip:4` proof-to-proof distances on a GPU and a CPU verifier.
Set `Params::for_engine("llama.cpp")`'s proof-comparison limits from it and
write the table in `docs/reports/gpu-work-metering-live-run.md`.

Until C3 lands, the ledger treats a proof comparison as **void**, never as a
failure (task P4).

### C4. Validator pay

`crates/metering-core/src/pay.rs`: `validator_pay(agreed_lp: f64, rate: f64)
-> f64` and `scale_to_share(pays: &[f64], worker_total: f64, share: f64) ->
Vec<f64>` (scales every validator's pay down when their sum passes `share`
of the cycle's worker pay). `StatementClaims`
(`crates/metering-core/src/receipt.rs:33`) gains optional
`validated_positions: f64` and `validator_strikes: u32`
(`#[serde(default, skip_serializing_if = ...)]`): a released node reads a
statement without them.

Tests: the cap scales proportionally; nothing above the share is paid; a
statement without the new fields opens.

---

## Part 2 - the node (POM)

### N1. The validator capability and admission

- `routing/pipeline.rs`: `CAP_METERING_VALIDATOR = "metering/validator-v1"`.
- `utils/config.rs`: `Config.validator: bool` (`#[serde(default)]`), set by
  the admission of a validator; announced through `network/network.rs`
  `with_delegation` (rename `capabilities_for_role`) on a worker whose config
  says so.
- Admission: `config::EnrollmentRole` stays as it is. The admission request
  (`LobbyRequest::Admission`, `network/network_lobby.rs`) gains optional
  `validator: bool`; `AdmissionRecord` (`network_lobby.rs:329`) gains
  `validator: bool` (`#[serde(default)]`); the grant
  (`AdmissionGrant`) carries it back so the joining node sets
  `Config.validator`. `accepted_enrollments` gains nothing: a network accepts
  validators when it accepts workers.

Tests: a worker with `validator` announces the capability, a plain worker
does not; an admission record and a grant from a released node read with
`validator: false`; a request asking `validator` is recorded so.

### N2. Re-execution returns proofs

`backend/metering_verify.rs`:

- The request (`ReexecuteRequest`) gains `mode: "verdict" | "proofs"`
  (`#[serde(default)]` = `verdict`, the path released ledgers use).
- In `proofs` mode the job computes the audited stage's output rows as today
  (`run`, the llama.cpp path; plugin engines stay void) and returns
  `{"proofs": [hex...], "chunk": n}`, the stage's TOPLOC chunks computed with
  `toploc::prove_chunk` over the same chunking the worker used, **without
  receiving the worker's chunks** (the request carries none in this mode).
- `node.reexecute` refuses `proofs` mode on a node that is neither an admin
  nor announcing `metering/validator-v1` (`node_op` caller checks stay as they
  are: only admins ask).

Tests: `proofs` mode on the synthetic model used by the existing
`metering_verify` tests returns chunks equal to `prove_chunk` of the
recomputed rows; a request in `proofs` mode with chunks attached is refused
(a validator must not see them); `verdict` mode unchanged.

### N3. Validators are never planned

`routing/pipeline.rs` planner input and `backend/deploy/survey.rs`: a peer
announcing `metering/validator-v1` is not a deploy target and not a pipeline
candidate; the deploy screen's peer strip leaves it out
(`apps/frontend/src/routes/DeployModel.tsx`, the strip's filter). Its models
still count for the ledger (task P2), so the survey keeps its inventory under
`validators`, not `machines`.

Tests: the planner skips a validator holding the only copy of a range (the
plan fails with the existing hole message); the survey lists it under
`validators`.

### N4. Screens on the node

- Admission (`routes/NetworkChat.tsx` lobby thread, `routes/PublicNetworks.tsx`
  "Ask to join"): "Join as a validator" beside the worker role, with what a
  validator sees (prompts and answers of sampled requests, phase A) and needs
  (the model on disk, a GPU optional).
- `routes/NetworkSettings.tsx` admitted machines: a "validator" badge.
- `routes/MyMachine.tsx`: a validator sees its role and its last cycle's
  validation pay (read from its statement, `peer.metering.statement`).
- Guides: `docs/guides/screens/network-chat.md`, `networks.md`,
  `my-machine.md` (en, pt-BR); a concept paragraph in
  `docs/guides/concepts/profiles.md`.

Tests: the admission request carries `validator`; the badge renders;
i18n keys in both languages.

---

## Part 3 - the ledger plugin (`../pom-plugin-metering`)

### P1. Who can validate

`src/worker.rs` `refresh` (line 37): read the live validators from the node
(`node.roles` gains `validator: bool` per node, from the capability; POM side
in `backend/metering.rs` `roles_of`, line 677) with the models each holds
(`node.validators` op: id, models, gpu flag, operator hint). Keep the list
per tick.

Tests (`ledger_tests.rs`, fake node): validators listed with their models; a
node without the capability is not one.

### P2. Assignment

`src/ledger.rs`: `assign(sample, validators, settings, secret) -> Vec<String>`:
`validators_per_sample` validators holding the receipt's model, drawn with
`sample::draw` keyed by the receipt id (deterministic for the ledger,
unpredictable for anyone without the secret), never the audited node, never
two with the same operator hint, never one paused (P5). Fewer available:
returns what there is.

Tests: never the worker; distinct operators; a paused validator skipped;
deterministic for a fixed secret.

### P3. Running a sample on several validators

`src/ledger.rs` `check_receipt` (line 673) becomes `check_sample`:

1. Build the request (`build_request`, unchanged) and a `proofs` copy without
   the worker's chunks.
2. Send it to each assigned validator (`node.reexecute`, `mode: proofs`) and,
   for a reference sample (`sample::selected(secret, "ref:<rid>", cyc,
   reference_rate)`) or when no validator is available, to the admin itself.
   Jobs run in parallel and are polled as today (`REEXECUTE_POLLS`).
3. Record each validator's result in a new table `validations(rid, validator,
   kind, outcome, detail, lp, at)`.

The single-verifier path (`MeteringSettings.verifier` set, or no validators)
stays as it is, in `verdict` mode.

Tests: two validators asked in parallel; a validator that does not answer is
void for that validator only; the request sent to validators carries no
chunks.

### P4. Deciding a sample

`decide(worker_chunks, results: &[(validator, proofs)], reference:
Option<proofs>, params) -> Decision`:

- Group the validators' proof sets by agreement (`compare_proofs`, C2, every
  chunk).
- The reference, when present, is the truth: a validator disagreeing with it
  takes a strike.
- Otherwise the majority group is the truth; a lone dissenter takes a
  strike; two validators that disagree with no reference make the sample
  void, unless the admin can run it now (then it is a reference).
- The worker's committed chunks against the truth's proofs: pass or fail, as
  today's sample outcome.
- Before C3's limits exist, `compare_proofs` results are recorded but every
  decision is void.

Tests: the five cases of spec decision 5 (agree and match, agree and differ,
one dissenter, split without reference, split resolved by the admin), plus a
validator copying the worker's chunks caught by a reference sample.

### P5. Strikes and pausing

`validations` rows with `kind = strike` count per validator per cycle; past
`validator_strikes` the validator is paused for the next cycle (no
assignment) and the overview flags it. Update the spec's canary paragraph
(see "Deviation" above) in POM in the same change.

Tests: the third strike pauses for exactly the next cycle; strikes reset.

### P6. Pay and statements

`close_cycle` (line 430): workers as today; then each validator's
`validated_positions` = the layer positions of the samples where its result
agreed with the decision; `validator_pay`, scaled by `scale_to_share` against
the cycle's total worker payout; worker payouts reduced by what the validators
receive (the share comes out of worker pay, spec decision 6). A validator gets
a signed statement like any node (`sign` closure), with `validated_positions`
and `validator_strikes`.

Tests: validator pay proportional to agreed work; the share cap holds;
workers' total plus validators' total equals the old workers' total;
statements carry the new fields.

### P7. Waiting for validators

`close_cycle` holds a cycle while it has samples still `waiting` (no
validator and no admin could run them) and the cycle is younger than
`validation_wait_cycles` past its grace; past it, waiting samples become void
and the cycle closes as today.

Tests: a cycle waits one cycle then closes; a validator coming back during
the wait resolves the samples.

### P8. The plugin's screens

`ui/`: the metering overview lists validators (models, GPU, samples run,
agreed, strikes, paused), per cycle the samples checked by how many
validators, waiting and void; a validator's own view (`rpc.mine`) shows its
validations and pay. i18n in both catalogs; `tests/contract.rs` passes.

---

## Part 4 - docs and the live check

- `docs/subsystems/`: metering is documented across `inference.md`,
  `http-api.md` and `security-model.md` (no metering subsystem file yet);
  add a "Validators" section where metering's verifier is described, in
  `security-model.md` say validators see sampled prompts in phase A, in
  `http-api.md` the new node ops, and link it from `payments.md`; folder `AGENTS.md`
  files of every folder touched, the plugin's `AGENTS.md` and `README.md`.
- Live check in `target/claude-live-metering`: A (admin, ledger), B and C
  workers, two validators on CPU (`pom-ll-v`, own `POM_HOME` each, holding
  the model): a cycle closes with every sample checked by two validators and
  both validators paid; a validator started with a patched build that returns
  another stage's proofs collects strikes and is paused; with both validators
  stopped, the cycle waits one cycle and then closes on receipts and
  challenges.

## Done when

- A network can admit validators; they are never planned; each sample is
  checked by two of them (or the admin), their proofs compared with each
  other and the worker's; disagreeing validators lose pay and are paused;
  validators are paid from a capped share of worker pay; a cycle without
  validators waits one cycle at most.
- A network without validators behaves exactly as before, and released nodes
  and ledgers keep working (`verdict` mode, optional fields).
- C3's calibration is recorded and its limits are in the code.
