# FreeToken MoE remaining dtypes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expand the Rust MoE expert executor from the delivered NVFP4, K-quant and IQ-quant formats to every active GGUF expert dtype, with explicit rejection for unsupported or invalid payloads.

**Architecture:** Keep GGUF table parsing, tensor-name pairing, geometry validation and the global expert cache at the source boundary. Add one decoder module per representation family and route every active dtype through an explicit `ExpertFormat` dispatch; a dtype is never decoded by a neighboring format. The existing dense path remains in `llama.cpp` and the wire carries no weights.

**Tech Stack:** Rust, GGUF v2, `memmap2`, `llama-cpp-sys-2`, `cargo test -p node`, local GGUF fixtures, and real GGUF inspection when a model is available in the worktree.

**Spec:** `docs/subsystems/freetoken-moe.md`

## Global Constraints

- Validate the GGUF header, tensor rank, dimensions, dtype, offset and payload length before decoding.
- Unsupported formats must refuse with `dtype=<id> (<name>)`; unknown IDs must include the raw ID.
- Dense models continue through `llama.cpp`; the MoE runtime is selected only from MoE metadata.
- No model weight crosses the network because of slicing.
- A local unit fixture is not a real-model proof; record each format as `real-GGUF proven` or `implemented, real GGUF not available`.
- Keep the untracked `llama.cpp` symlink out of commits.

### Task 1: Plain scalar experts F32/F16/BF16

**Files:**
- Create: `crates/node/src/inference/moe/format_plain.rs`
- Modify: `crates/node/src/inference/moe/mod.rs`, `runtime.rs`, `block.rs`
- Test: `format_plain.rs` and `runtime.rs` fixtures with all three dtypes, fused and split gate/up, malformed geometry, and unsupported dtype errors
- Modify: `crates/node/src/inference/AGENTS.md`, `docs/subsystems/freetoken-moe.md`

**Interfaces:**
- `PlainFormat::{F32,F16,Bf16}` maps IDs 0, 1 and 30 to scalar byte widths.
- `PlainSource::open_infer(path, experts)` validates expert tensors and exposes `router_logits`, `load_expert` and model geometry.
- `PlainExecutor` decodes a cached payload into gate/up, SiLU-gated FFN and down projection.
- `PlainMoeBlock` applies routing and the shared cache exactly like existing format blocks.

- [x] Write a failing runtime test that opens one synthetic GGUF per dtype and runs a two-token forward.
- [x] Run the focused test and confirm failure because the plain dispatcher does not exist.
- [x] Implement scalar conversion and payload geometry validation without accepting quantized IDs.
- [x] Run the focused tests and confirm all three dtypes and rejection cases pass.
- [x] Update docs with real-GGUF availability; commit and publish the block after the full validation gate.

### Task 2: Classic block quantization Q4_1/Q5_0/Q5_1/Q8_0

**Files:**
- Create: `crates/node/src/inference/moe/format_legacy_quants.rs`
- Modify: `crates/node/src/inference/moe/mod.rs`, `runtime.rs`, `block.rs`
- Test: per-format block decode, split/fused names, geometry rejection, and dispatcher rejection

**Interfaces:**
- `LegacyQuantFormat` maps IDs 3, 6, 7 and 8 and owns exact block sizes and dequantization.
- `LegacyQuantSource` owns GGUF tensor slicing; `LegacyQuantExecutor` owns mat-vec.

- [x] Add failing format tests for one block of every dtype.
- [x] Implement block layouts from the ggml type definitions and validate row divisibility.
- [x] Run focused tests and compare decoded values against an independent scalar reference.
- [x] Inspect available local GGUFs; none contain these MoE dtypes, so record explicit lack of proof and publish.

### Task 3: MXFP4, TQ1_0, TQ2_0, Q1_0 and Q2_0

**Files:**
- Create or modify dedicated format modules under `crates/node/src/inference/moe/`
- Modify: `runtime.rs`, `block.rs`, docs and inference AGENTS guidance
- Test: format-specific reference vectors, payload truncation, and dispatch names

- [ ] Add one failing reference-vector test per format.
- [ ] Implement exact block unpacking and dequantization with checked arithmetic.
- [ ] Validate focused tests, then inspect real GGUF if available.
- [ ] Publish only the tested subset and record unproven formats separately.

### Task 4: Remaining active K/IQ/integer/F64 formats

**Files:**
- Modify the format modules and explicit dispatch registry.
- Test: every active ID not already covered, including I8/I16/I32/I64/F64, Q2_K/Q3_K/Q5_K/Q8_K, IQ1_M/IQ1_S/IQ2_XS/IQ2_XXS/IQ3_XXS/IQ4_NL.

- [ ] Add a conformance table test asserting every active ID has a named decision.
- [ ] Add exact payload/reference tests family by family.
- [ ] Reject removed IDs 4, 5, 31-33 and 36-38 with their names.
- [ ] Validate all available real GGUFs, run the dense regression suite, and publish each coherent block.

### Task 5: Final conformance and measurement

**Files:**
- Modify: `docs/subsystems/freetoken-moe.md`, `crates/node/src/inference/AGENTS.md`
- Test: full `cargo test -p node`, `cargo build -p node`, `cargo fmt --check`, `cargo clippy -p node`

- [ ] Verify every active enum ID has a decoder or an explicit, named blocker.
- [ ] Run real-model requests for every available dtype and retain telemetry.
- [ ] Confirm the untracked `llama.cpp` symlink is absent from the index.
- [ ] Record tested versus unproven formats and publish the final incremental branch state.
