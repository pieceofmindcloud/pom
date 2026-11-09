# GPU work metering: live run on real nodes

Date: 2026-11-09. Code: POM branch `feat/gpu-work-metering`, plugin
`pom-plugin-metering` `main`, both local.

## Setup

Three `pom` debug builds in one sealed container (`--network none`, no GPU,
CPU only), each with its own `POM_HOME`: machine A (principal, gateway and
ledger with the metering plugin installed through the dev rebuild), B and C
(workers). Model: a 27B IQ3_S GGUF of 65 layers served as virtual slices of
the whole file (`POM_VIRTUAL_SLICES=1`). Metering on with a 5-minute cycle,
30 s grace, 2 samples and 3 challenges per node.

## What was checked

1. **Two hops, f16 wire** (A `0..31`, B `32..64`). The first run found two
   defects, both fixed before the results below:
   - the head check compared the head's input chain with its whole computed
     sequence, generated tokens included (it now stops after the prefilled
     prompt positions);
   - the output link hashed the reply frame, while in ring, direct and link
     modes a hop encodes a separate frame for the next hop: the output link
     now hashes what the relay encoders forward (`metering::note_forward`),
     and hidden-state links no longer carry positions.
   After the fixes both receipts were metered and linked, the bundle reached
   the ledger through the outbox, and at cycle close the head's sample was
   re-executed on A (layers 0-31 cut from the whole file, the head's token
   sequence run on CPU) and **passed**. Each node got a signed statement with
   full payout (head 1152 layer-positions, tail 1188).
2. **Three hops, f8 wire** (A `0..23`, C `24..47`, B `48..64`). All three
   receipts metered with f8 links; a challenge on C passed; at cycle close
   C's sample was re-executed through the reproduced f8 boundary (layers 0-23,
   `wire::decode(wire::encode(.., f8))`, then 24-47) and **passed** with the
   paper's bf16 TOPLOC thresholds.

## Not covered

- The tail keeps no proofs (it outputs tokens), so its work was paid on
  receipts alone.
- No dishonest node was run live; the failure paths are covered by unit
  tests (TOPLOC on other weights and coarse quantization, ledger failures,
  missing proofs, wrong roots).
- Threshold calibration at range ends across model families and GPUs (plan
  task 2.5) still needs a dedicated run.

## Strata (split plugin engine), 2026-11-09

Two machines, one GPU each (RTX 3060, RTX 5060 Ti), Flash-Next IQ3_XXS split
`0..23` / `24..47` over an f8 stage link, metering on, the ledger on the
first machine.

- **Receipts: confirmed.** Every turn produced linked receipts on both stages
  (`out_chain` of stage 0 equal to `in_chain` of stage 1 over f8), the
  gateway collected them after the stream, and the ledger accepted them and
  paid both stages in the cycle statements.
- **Found and fixed:** Flash-Next's engine reads a prompt as verify windows
  (`'W'`/`'V'`), not prompt pieces; the earlier stage now takes the later
  stage's phase tag (`?phase=prefill`) to meter such windows as prompt. Decode
  windows are proved only for their kept rows (at the commit), so rejected
  drafts are never proved.
- **Found:** Strata answers a prompt piece with prompt rows (10240 floats per
  token on Flash-Next: 4 hyper-connection streams of 2560) and a window with
  window rows (12804). The verifier reads the sequence both ways and accepts
  the layout that matches.
- **Re-execution runs end to end** on real Strata: the verifier's plugin
  loads an audit stage of layers 0-23, the node reads its rows, and the
  TOPLOC check runs; repeated re-executions of one receipt agree with each
  other (deterministic). A sample taken while the deployment still held the
  GPU was void (out of memory), as designed.
- **Not yet confirmed:** an honest Strata sample passing as a whole. The
  last fix (prompt windows proved at once, decode windows at their commit)
  combines the two halves that each passed in separate runs; confirming it
  needs a new cycle with both GPUs free, which another node of the cluster
  took during the test.
