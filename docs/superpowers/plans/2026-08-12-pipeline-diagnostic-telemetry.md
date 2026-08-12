# Pipeline Diagnostic Telemetry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show a phase-separated, diagnostically honest timing table for each pipeline hop without changing llama.cpp.

**Architecture:** The Rust inference engine exposes a diagnostic-only synchronization after `llama_decode`, using the already-bound public `llama_synchronize` API. This measures queued backend completion plus any already-queued output transfer, not an invented CUDA event. The final-stage sampler returns a CPU-only breakdown. The gateway carries separate prefill and decode aggregates to the chat table.

**Tech Stack:** Rust, llama-cpp-sys-2 public FFI, Axum binary frames, React, TypeScript.

## Global Constraints

- Do not modify llama.cpp or its Cargo registry source.
- Synchronization is diagnostic-only and disabled unless explicitly requested.
- Label measurements by their real boundary; do not call HTTP residual packet latency or backend completion GPU-only execution.
- Keep prefill and decode aggregates separate.

---

### Task 1: Diagnostic engine and sampler boundaries

**Files:**
- Modify: `crates/node/src/inference/llm.rs`
- Test: `crates/node/src/inference/llm.rs`

**Interfaces:**
- Produces `DecodeTimings` and `SamplingTimings` for the inference HTTP server.

- [x] Add a failing unit test asserting zero-value diagnostic structures serialize predictable boundaries.
- [x] Run the focused test and confirm it fails because the structures are absent.
- [x] Add a diagnostic mode enabled by `RDMA_P2P_PROFILE_SYNC=1`; measure decode enqueue and, only in that mode, `llama_synchronize` completion.
- [x] Return sampler timings for logits clone, repetition penalty, temperature/softmax, top-k, top-p, RNG/selection and token formatting.
- [x] Run focused Rust tests.

### Task 2: Per-phase wire telemetry

**Files:**
- Modify: `crates/node/src/inference/infer_server.rs`
- Modify: `crates/node/src/inference/prefill.rs`
- Test: `crates/node/src/inference/prefill.rs`

**Interfaces:**
- Consumes engine and sampler timing structures.
- Produces `HopTrace` fields split into `prefill` and `decode` phase totals.

- [x] Add a failing merge test that asserts prefill and decode phase totals remain independent.
- [x] Run it and confirm the field is absent.
- [x] Attach diagnostic node timing data to each reply and aggregate it by phase.
- [x] Run focused Rust tests.

### Task 3: Chat timing table

**Files:**
- Modify: `apps/frontend/src/lib/chatStream.ts`
- Modify: `apps/frontend/src/routes/Chat.tsx`

**Interfaces:**
- Consumes the phase-separated `RequestTrace` SSE payload.
- Produces a table with one prefill row and one decode row per hop.

- [x] Update TypeScript trace types with the new fields.
- [x] Render phase rows with gateway, backend, residual, sampling, transfer and bytes columns.
- [x] Build the frontend and inspect the table against a representative trace.

### Task 4: Verification and delivery

**Files:**
- Modify: the files above only.

- [x] Run focused Rust tests, frontend production build and `git diff --check`.
- [ ] Commit and push the verified change to `main`.
