# Request Telemetry Design

## Goal

Make every distributed generation explainable in the Chat interface and make
the current process expose a compact, in-memory operational picture of the
cluster.

## Scope

- Keep telemetry only for the lifetime of the gateway process.
- Show a final execution summary under every completed Chat reply.
- Preserve the existing per-hop trace and extend it with failed attempts.
- Expose bounded aggregates for the current process in the request-log API.
- Show the aggregate summary on the Dashboard and the detailed history in
  Logs.

No database, external metrics backend, OpenTelemetry exporter, alerts, or
cross-restart retention is part of this work.

## Data Model

`RequestTrace` remains the per-generation transport object. Each hop records
the layer range, successful prefill/decode compute time, residual transport
time, bytes sent and received, and successful call count.

Each transport attempt is additionally represented by a bounded `TraceIssue`:

- `hop`: selected pipeline hop index, when one was selected.
- `node_id`: selected peer identity.
- `phase`: `prefill` or `decode`.
- `kind`: `timeout`, `transport`, `http`, or `protocol`.
- `attempt`: one-based retry number.
- `message`: a short, operator-safe explanation.

At most 32 issues are retained per trace. Further issues increment a dropped
counter. A trace always reports `complete`: false when a request failed before
the normal completion summary.

The existing `network_ms` remains explicitly defined as end-to-end HTTP time
minus peer-reported processing time. It is an attribution bucket for transport
and overhead, not a claim of packet-level network latency.

## Gateway and Log Flow

The gateway creates a trace when it constructs a pipeline plan. Prefill and
decode mutate that same trace, including failed and retried attempts. Every
terminal `GenEvent` carries the trace: success, partial success, and failure.

The in-memory request log stores the terminal trace with each entry. Its
bounded size remains 200 requests. The request-log endpoint returns an
`telemetry` aggregate calculated from the retained entries:

- request, success, partial, error, and abandoned counts;
- total bytes sent and received;
- mean completed duration, per-hop processing time, residual transport time,
  calls, bytes, and issue count;
- no percentile or rate claim, because the bounded rolling window is not a
  time-series database.

The log never stores prompt text or hidden-state contents.

## Chat UX

On receiving the terminal SSE event, the Chat saves the trace and terminal
status with the assistant message. Beneath every completed assistant message,
it renders an always-visible `Execution telemetry` card:

- status, pipeline source, duration, output rate, input/output tokens, and
  total traffic;
- one compact line for each hop, showing layers, compute, residual transport,
  calls, and bytes;
- a collapsed `Issues` detail only when attempts failed;
- a clearly worded caveat that residual transport includes HTTP overhead.

The existing Debug button/modal is removed from Chat because the summary is no
longer hidden. The card does not appear while tokens are still streaming; it
appears when a terminal event arrives, including a failed empty response.

## Dashboard and Logs UX

Dashboard receives a small operational strip from `/api/ui/requests`: retained
request count, errors, aggregate bytes, and per-node requests/errors. It is an
observer only and does not alter routing.

Logs continues to list individual requests, now with terminal state and a
click-to-expand per-request telemetry view. The retained trace explains the
same result as Chat without requiring access to the original session.

## Error Handling

An HTTP non-success, request timeout, transport error, or invalid response
frame produces a `TraceIssue` before the operation returns its existing error.
Retries record each failed attempt. A failed prefill returns its trace to the
client and is persisted in the log. If a client disconnects, the gateway marks
the log entry abandoned and retains observations gathered so far.

## Verification

- Rust unit tests prove successful-hop accumulation, bounded issue retention,
  failed prefill trace propagation, and aggregate calculations.
- Frontend tests prove terminal SSE metrics produce a visible telemetry state
  and issue rendering helpers classify terminal states correctly.
- `cargo test`, frontend tests, frontend build, and release build pass.
- A local `/chat` request is inspected visually after deployment on the
  MacBook. If no model is loaded, the UI must still render the terminal error
  card with its telemetry state where available.
