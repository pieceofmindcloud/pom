# Validators, and keeping prompts on trusted machines

Date: 2026-11-10 - Status: draft for iteration

## Problem

GPU work metering (`2026-11-08-gpu-work-metering-design.md`) pays the machines
of a network for the layers they ran, and samples their work by re-execution:
at each cycle's close the metering ledger (a plugin on an admin) picks
receipts, has a **verifier** re-run them (`node.reexecute`) and compares the
TOPLOC fingerprints with the ones the worker committed. Today there is one
verifier: the ledger's admin, or one machine named in
`MeteringSettings.verifier`. Three limits follow:

- **Capacity.** One machine re-runs every sample of every worker. A busy or
  absent verifier leaves samples void, and void samples prove nothing.
- **Trust in one machine.** A single verifier's verdict is final. Nothing
  checks it.
- **Exposure.** Re-execution replays the request from its token ids, so
  whoever verifies reads every sampled prompt and answer. A worker that holds
  layer 0 reads every prompt too, sampled or not.

Charging visitors (`2026-11-09-visitor-charging-design.md`) makes both money
and prompts matter more: visitors pay, and their prompts cross machines the
visitor does not know.

## Decisions taken

- A **validator** is a machine admitted to a network to re-run sampled work.
  It never serves inference. It needs the model on its own disk and enough
  memory for one range at a time; a GPU is optional (re-execution on CPU was
  calibrated against GPU work, report
  `docs/reports/gpu-work-metering-live-run.md`).
- Validators may be far from the workers and reached through a relay:
  re-execution is a background job, not part of any request.
- The admin sends each sample to several validators when it cannot or does
  not want to verify alone, compares the validators' results with each other
  and with the worker's, and pays workers and validators when they agree.
- Validators are paid a small share for the work they re-ran.
- Validators are optional: without them visitors are still served; only the
  workers' payouts wait (within a bound, decision 7).
- **U-shaped placement**: a deployment may keep its first and last layers on
  trusted machines (admins), so the machines in the middle, and the
  validators that check them, see hidden states but no tokens.

## Goals

- Many independent validators per network, each sample checked by several of
  them, chosen at random, unaware of each other.
- A validator cannot earn by answering without computing, and a validator
  that disagrees with the others is found and stops being paid.
- A network can choose who may see prompts: validators, or admins only.
- A deployment can keep prompts and answers off every machine but its
  trusted ones, at the cost of one more network crossing per token.
- Nothing changes for a network that does not admit validators or use the
  U-shape.

## Non-goals

- Cryptographic privacy. Hidden states can be partly decoded back to text,
  most easily after the first layers. The U-shape hides prompts from casual
  view and from any machine that only stores what it receives; it does not
  defeat an attacker who studies the hidden states. The product must say so.
- Validators for plugin engines (Strata). Their output is not reproducible
  enough for TOPLOC today (`metering.verify_plugin_engines` stays off); they
  get validators once an engine is calibrated or gains a deterministic audit
  mode.
- Paying validators in money outside the network's metering units (see
  decision 6 for where the pay comes from).

## Part 1 - Validators

### 1. The role: a worker that validates

A validator is admitted as a **worker with the `validator` capability**: its
membership token says `worker`, and its announce carries
`metering/validator-v1`. The planner never places inference on a node that
announces it, and the deploy screen does not offer it as a target.

Why not a new `EnrollmentRole`: the role is a value inside membership tokens
and admission records, which released nodes parse. A value they do not know
would break their reading of the whole list. A capability is additive
(compatibility policy, root `AGENTS.md`).

The admission screen offers "validator" as a kind of worker request; the
admin sees it as such and can accept many. A secondary admin may admit
validators like any worker.

A validator announces what it can check: the models it holds (the same
inventory a worker announces) and whether it runs a GPU. It downloads models
itself (`model_provider_backend`), as no weights travel between nodes.

### 2. What a validator receives, by exposure

The network chooses one of two settings (`metering.validators_see`):

- **`tokens`** (validators are trusted with prompts): a validator may check
  any stage. It replays the sample from the token ids, as the verifier does
  today.
- **`hidden_states`** (default): a validator checks **middle stages only**,
  from the hidden state the stage received, never from tokens (decision 3).
  The head and the tail are checked by admins, which hold those layers under
  the U-shape anyway (Part 2), or by whatever admin runs the ledger.

Whatever the setting, the admission screen tells a candidate validator what
it will see, and the network's information tells visitors who can read their
prompts.

### 3. Hidden-state re-execution

To check a middle stage from its input:

- The stage keeps, for each request, the input tensor it received, encoded
  as it arrived (f16, f8 or int4), until the cycle closes and its samples are
  drawn, then discards it. About 10 KB per token per stage for a 27B model in
  f16, so roughly 10 MB for a 1000-token request; the stage keeps them in its
  metering store with the cycle's retention, and only for requests in a
  metered cycle.
- The validator receives that tensor, loads the stage's layer range from its
  own copy of the model, runs it, and computes the TOPLOC fingerprint of its
  output.
- **The input is checked before it is trusted**: its digest must equal the
  output digest the previous stage signed for the same request (the link
  digests metering already records, `crate::metering`). A worker cannot hand
  the validator an input of its choice without the previous stage's
  signature on it.

This needs a calibration like the token replay had: honest runs from the
stored tensor against runs that skip layers, on GPU and CPU validators,
through each wire format. Until it passes, `hidden_states` samples count as
void.

### 4. Validators return proofs, not verdicts

Today `node.reexecute` returns a verdict: the verifier compares its own run
with the worker's fingerprint and says pass or fail. A validator that wanted
to earn without computing could answer "pass" every time.

A validator instead returns **its own TOPLOC chunks** for the sample, and
never sees the worker's. The ledger compares:

- each validator's chunks with each other validator's, with the calibrated
  limits (`toploc::Params::for_engine`);
- the validators' consensus with the worker's committed chunks.

A validator cannot produce chunks that match the others without running the
layers.

### 5. Assignment, agreement and canaries

- **How many**: each sample goes to `metering.validators_per_sample`
  validators (default 2), chosen at random among those that hold the model
  and are live, never the worker itself, never two validators announcing the
  same operator hint. The admin running the ledger counts as one when it
  checks.
- **Unaware of each other**: a validator is told only its sample. Results go
  to the ledger, which answers nothing about other validators.
- **Agreement**:
  - The validators agree and the worker matches: the sample passes.
  - The validators agree and the worker differs: the sample fails (the
    worker's penalty, as today).
  - One validator differs from the rest: the sample is decided by the rest,
    and that validator's result is a **strike**: no pay for it, and a
    validator past `metering.validator_strikes` in a cycle gets no samples
    for the next one and is flagged to the admin.
  - No majority (two validators, two answers): the ledger's admin re-runs it
    itself if it can; otherwise the sample is void.
- **Canaries**: the ledger mixes in samples it corrupted on purpose (the
  calibration's `skip:N` variants, which the ledger can build). A validator
  that passes a canary takes a strike. Canaries cost validators compute and
  are not paid.

### 6. Pay

A validator's work is measured like a worker's: the layer positions it
re-ran (`layer_positions`, the metering unit), paid at
`metering.validator_rate` of a worker's layer position (default 0.1). It
earns only for results that agreed with the decision of their sample, never
for canaries or strikes. Its pay appears in the cycle statement it gets like
any machine.

Where the pay comes from: a **share of the cycle's worker pay**, taken
before it is split (`metering.validator_share`, a cap, default 5 %), so the
network's metering totals do not change and no conversion to visitor credits
is needed. A network that charges visitors can later pay it from income
instead; that needs the credits-to-metering-unit rate decided first (open
question 4).

### 7. When validators are missing

Validators make the network safer; they are not needed to serve. Without
enough validators for a sample:

- the admin running the ledger checks it itself, if it can;
- otherwise the cycle's statements **wait**, for up to
  `metering.validation_wait_cycles` (default 1), while samples are retried as
  validators come back;
- past that bound the cycle closes as today: unchecked samples are void and
  workers are paid on receipts and challenges. A network never stops paying
  its workers because no validator came.

The Payments and metering screens show, per cycle, how many samples were
checked, by how many validators, and how many waited or went void.

## Part 2 - Keeping the ends on trusted machines (U-shape)

### 8. The placement

A deployment may set **`trusted_ends`**: the range holding layer 0 (the
embedding and the first layers) and the range holding the last layer (the
final layers and the output head) must run on trusted machines: admins of the
chain. The middle ranges may run on any worker.

The pipeline already allows it: its order is the layers' order, one machine
may serve several ranges, and the planner and the deploy screen accept a
machine holding both ends. What is new is the rule that enforces it:

- the deploy screen offers "Keep prompts on trusted machines" and marks the
  end ranges as admin-only;
- the planner refuses a plan whose end ranges are not on trusted machines
  (`untrusted_end`), at deploy and per request, including when it fails over
  to a replica;
- the entry nodes must be trusted too, since the visitor's request reaches
  the entry node as text: under `trusted_ends` a worker is not an entry node
  for that model, whatever `entry_nodes` says.

### 9. What the middle sees

- Every prompt token, and every token generated later, enters through the
  head and leaves through the tail. Middle workers receive only hidden states,
  in both directions, for the whole conversation.
- The head should hold more than one layer (`trusted_ends.head_layers`,
  default 4): the first hidden state that leaves it is then harder to decode
  than the raw embedding.
- The lossy wire formats (f8, int4) add noise but are not a privacy measure.

### 10. What it costs

- **One more crossing per token.** The data goes out from the head to the
  workers and back to the tail, which is the same trusted machine or another
  one. Over the internet that is one more round trip per token; the deploy
  screen shows the estimate from the measured RTTs before deploying.
- **Room on the trusted machine** for both end ranges, the embedding table
  and the output head, plus their compute on every token.
- **A single point of failure** unless several trusted machines hold the end
  ranges as replicas; the planner treats them as alternatives as it does any
  replica.

### 11. How the two parts fit

Under `trusted_ends` with `validators_see = hidden_states`:

- the trusted machines check the head and the tail (they run those layers);
- validators check the middle stages from hidden states;
- no machine outside the trusted ones reads a prompt or an answer.

The network's information shows visitors: "Prompts and answers stay on the
network's admins; other machines see intermediate data only", with a link to
what that does and does not mean.

## Compatibility

- Additive only: a capability (`metering/validator-v1`), optional fields on
  metering settings and on the deployment record (`trusted_ends`), a new
  metering store table for input tensors, new ledger operations
  (`node.reexecute_hidden`, results as chunks). The existing `node.reexecute`
  verdict path stays for the single verifier and for released ledgers.
- A released node never becomes a validator (it does not announce the
  capability) and keeps verifying as today if named in
  `MeteringSettings.verifier`.
- A released worker in the middle of a `trusted_ends` deployment works: it
  only ever sees hidden states. A released worker cannot keep input tensors,
  so its stages are checked by token replay (admins only) or left void.

## Phases

A. **Validators with token replay** (`validators_see = tokens`): the
   capability and admission, assignment to several validators, results as
   chunks, agreement, strikes, canaries, pay, the wait bound. Uses the
   existing re-execution.
B. **U-shape**: `trusted_ends` in the planner, the deploy screen and entry
   node selection; head and tail samples routed to admins.
C. **Hidden-state re-execution**: input tensors kept per metered request,
   digest check against the previous stage, `node.reexecute_hidden`, a live
   calibration; then `validators_see = hidden_states` becomes the default.
D. Later: validators for plugin engines after their calibration; validator
   pay from visitor income.

## Open questions

1. Validators per sample: 2 by default, or 3 so a single dissent never needs
   the admin?
2. The wait bound: one cycle, or a time (for example 24 h) independent of the
   cycle length?
3. Strikes: how many before a validator is paused, and should the admin be
   able to remove a validator automatically?
4. The rate between visitor credits and metering units, if validators (and
   later workers) are to be paid from income.
5. Should a validator's operator be able to run several validators, or should
   admission ask for one per operator (the operator hint of decision 5 is
   only a hint)?
6. Under `trusted_ends`, is a secondary admin trusted with the ends, or only
   the principal?

## Out of scope

Cryptographic privacy for prompts (homomorphic encryption, secure enclaves),
paying validators outside the network, validators for engines that cannot be
re-executed reproducibly.
