# Request Telemetry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show an automatically visible, per-request distributed telemetry result in Chat and retain bounded operational aggregates for the current node process.

**Architecture:** `prefill` owns a serializable trace and attempt issues. `gateway` carries the trace through every terminal outcome and hands it to `reqlog`, which retains at most 200 entries and derives aggregates on read. The frontend shares typed telemetry data between Chat, Logs, and Dashboard, with Chat rendering the completed trace inline.

**Tech Stack:** Rust, Axum, Serde, Tokio, React, TypeScript, TanStack Query, Vitest.

## Global Constraints

- Telemetry exists only in process memory and is cleared on restart.
- Request-log retention is exactly 200 entries.
- No prompt text, API credential, hidden state, or raw tensor enters telemetry.
- `network_ms` means HTTP round trip minus peer-reported processing and must be labelled as transport plus overhead.
- Preserve all existing local changes and do not commit from this dirty worktree.

---

### Task 1: Trace issues and terminal trace propagation

**Files:**
- Modify: `crates/node/src/inference/prefill.rs`
- Modify: `crates/node/src/backend/gateway.rs`
- Modify: `crates/node/src/backend/openai.rs`

**Interfaces:**
- Produces: `TraceIssue`, `RequestTrace::record_issue`, and terminal `GenEvent` variants carrying `Option<RequestTrace>`.
- Consumes: the existing `RequestTrace` and transport loops in `prefill::run` and `prefill::step`.

- [ ] **Step 1: Write failing Rust tests for bounded issues and terminal failures**

```rust
#[test]
fn a_trace_keeps_the_first_32_issues_and_counts_the_rest() {
    let mut trace = RequestTrace::new("m", &[("n", 0, 1)]);
    for attempt in 1..=33 {
        trace.record_issue(TraceIssue::transport(0, "n", Phase::Prefill, attempt, "down"));
    }
    assert_eq!(trace.issues.len(), 32);
    assert_eq!(trace.dropped_issues, 1);
}

#[tokio::test]
async fn failed_prefill_event_keeps_its_partial_trace() { /* use a refused local listener */ }
```

- [ ] **Step 2: Run the new tests and confirm they fail for missing trace fields/events**

Run: `cargo test --bin rdma-p2p prefill::tests`

- [ ] **Step 3: Add the smallest serializable trace issue model**

```rust
pub struct TraceIssue { pub hop: Option<usize>, pub node_id: String, pub phase: Phase, pub kind: TraceIssueKind, pub attempt: u32, pub message: String }
pub fn record_issue(&mut self, issue: TraceIssue) { /* retain <= 32, increment dropped */ }
```

Record HTTP, timeout/transport, and protocol-decoding failures before returning from `send_chunk` and `step`.

- [ ] **Step 4: Carry the accumulated trace through `GenEvent::Failed` and request logging**

Replace `Failed(String)` with a variant containing the error and optional trace. Emit `complete: false` for prefill failures. Preserve success and partial traces.

- [ ] **Step 5: Run the focused tests and the OpenAI stream tests**

Run: `cargo test --bin rdma-p2p prefill::tests openai::tests`

Expected: all selected tests pass.

### Task 2: Bounded request-log telemetry and aggregates

**Files:**
- Modify: `crates/node/src/backend/reqlog.rs`
- Modify: `crates/node/src/backend/gateway.rs`

**Interfaces:**
- Consumes: `Option<RequestTrace>` from terminal generation events.
- Produces: `Entry.trace: Option<RequestTrace>` and a `telemetry` object on `GET /api/ui/requests`.

- [ ] **Step 1: Write failing aggregate tests**

```rust
#[tokio::test]
async fn aggregates_completed_traces_by_node_and_status() {
    // record one success and one error with trace data
    // assert request counts, byte totals, issue count and node totals
}
```

- [ ] **Step 2: Run the test and confirm the response lacks `telemetry`**

Run: `cargo test --bin rdma-p2p reqlog::tests::aggregates_completed_traces_by_node_and_status`

- [ ] **Step 3: Store trace with each log entry and derive aggregates at read time**

Return counts for `ok`, `partial`, `error`, and `abandoned`, totals for bytes and issues, and a per-node map with request count, errors, compute milliseconds, transport milliseconds, calls, and bytes.

- [ ] **Step 4: Run request-log tests**

Run: `cargo test --bin rdma-p2p reqlog::tests`

Expected: all request-log tests pass.

### Task 3: Shared frontend telemetry contract and inline Chat card

**Files:**
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/lib/chatStore.ts`
- Modify: `apps/frontend/src/lib/chatStream.ts`
- Modify: `apps/frontend/src/lib/chatStream.test.ts`
- Modify: `apps/frontend/src/routes/Chat.tsx`

**Interfaces:**
- Consumes: terminal `x_rdma_p2p.trace`, `partial`, and error payloads from SSE.
- Produces: `RequestTrace`, `TraceIssue`, and terminal message status in the saved chat message.

- [ ] **Step 1: Write failing frontend tests for terminal state retention**

```ts
it("retains a failed trace and terminal status", () => {
  const result = finishChatStream(emptyChatStream(), { trace, status: "error" });
  expect(result.trace?.complete).toBe(false);
  expect(result.status).toBe("error");
});
```

- [ ] **Step 2: Run the test and confirm the terminal status is unavailable**

Run: `npm --prefix apps/frontend run test -- src/lib/chatStream.test.ts`

- [ ] **Step 3: Add exact TypeScript shapes and save terminal status into messages**

Use the Rust field names without loose `any` or string escape hatches. Preserve local chat-session serialization compatibility with optional fields.

- [ ] **Step 4: Replace the Debug modal with an inline `ExecutionTelemetryCard`**

Render only after a terminal event. Show source, terminal state, duration, rate, token counts, traffic, compact per-hop rows, issue disclosure, and the transport-overhead caveat. Render an error card even when a request produces no assistant text.

- [ ] **Step 5: Run frontend unit tests and build**

Run: `npm --prefix apps/frontend run test && npm --prefix apps/frontend run build`

Expected: tests and TypeScript build pass.

### Task 4: Operational summaries in Logs and Dashboard

**Files:**
- Modify: `apps/frontend/src/lib/types.ts`
- Modify: `apps/frontend/src/lib/queries.ts`
- Modify: `apps/frontend/src/routes/LogsLive.tsx`
- Modify: `apps/frontend/src/routes/Dashboard.tsx`

**Interfaces:**
- Consumes: `RequestLogResponse.telemetry` returned by `/api/ui/requests`.
- Produces: a compact Dashboard observer strip and expandable trace details in Logs.

- [ ] **Step 1: Write failing pure-UI helper tests for telemetry status tone and aggregate formatting**

```ts
expect(telemetryTone({ errors: 1, requests: 2 })).toBe("amber");
expect(telemetryTone({ errors: 0, requests: 2 })).toBe("aqua");
```

- [ ] **Step 2: Run the test and confirm the helpers do not exist**

Run: `npm --prefix apps/frontend run test -- src/lib/telemetryStatus.test.ts`

- [ ] **Step 3: Add the Dashboard telemetry strip and Logs trace disclosure**

Keep these views observational. Do not add routing controls, polling more frequently than the existing request-log query, or a browser-side accumulation buffer.

- [ ] **Step 4: Run the focused frontend test and full frontend verification**

Run: `npm --prefix apps/frontend run test && npm --prefix apps/frontend run build`

Expected: tests and build pass.

### Task 5: End-to-end verification and MacBook deployment

**Files:**
- Modify: `docs/superpowers/plans/2026-08-12-request-telemetry.md`

- [ ] **Step 1: Format and test the full workspace**

Run: `cargo fmt --check && cargo test && npm --prefix apps/frontend run test && npm --prefix apps/frontend run build && cargo build --release --bin rdma-p2p`

- [ ] **Step 2: Launch or restart the MacBook node using its existing run procedure**

Inspect the process and configured `run-p1.sh` before restarting anything. Keep the existing model and network configuration unchanged.

- [ ] **Step 3: Send a Chat request and inspect `/chat` visually**

Use the browser to submit a request, wait for the terminal event, and verify the inline card shows terminal status and any available trace. Capture a screenshot.

- [ ] **Step 4: Update this plan with evidence**

Record exact test counts, build result, deployed command/process, request result, and visual evidence. Do not mark visual telemetry successful if the cluster has no runnable model.
