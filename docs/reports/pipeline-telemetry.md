# Technical report - distributed pipeline telemetry

Date: 2026-09-10

Status: instrumentation and fix implemented and validated at unit, lab and
frontend level. The E2E capture with Ornith and two peers was run on the cluster
on 2026-09-10 after deploying `65e88d10`; the MoE executor was skipped, as the
spec says.

## Executive summary

Four related causes were proven:

1. `outside-node` was computed in the UI as `total_ms - sum(processing_ms)`.
   That mixed E2E wall-clock with a sum of spans from different peers and
   treated overlapping times as an exclusive partition.
2. The second peer did not show up in decode because `hop_timings` was produced
   by the relay, but dropped by `RemoteStreamToken`, not serialized in the
   callback and never read by `RemoteDecodeStream::next`. The loss was
   deterministic, not a late arrival or a wrong correlation.
3. Closing the `ReplyInbox` removed the ticket when the answer was delivered.
   Transport receipts that arrived later were refused with `410`, with no
   reconciliation phase.
4. There was no aggregated instrumentation of opening, lifetime, frames, bytes,
   first delivery and wait of the `remote-stream`; so it was not possible to
   separate callback cost, wait and remote processing.
5. In direct decode, `await_result_with_ack` cancelled the head's ACK when the
   tail's answer won the `select`; that also erased the first hop's evidence
   precisely in the fastest distributed answer case.

The implementation now keeps the downstream hops, separates
`response_complete` from `telemetry.status == complete`, aggregates bounded
histograms, deduplicates frames/receipts and only presents overlap or true
residual when there are intervals that measure them. No new breakdown is
obtained by subtracting from the E2E.

## Reproduction and limits of the evidence

- `pwd -P` and `git rev-parse --show-toplevel` pointed to an isolated
  worktree.
- The UI at `http://localhost:8080/admin-ui/chat` showed `The network is not
  serving any model`. So there is no honest local number for Ornith, two peers
  or a before/after throughput comparison.
- The production numbers below are the ones provided by the operator's spec.
  They guided the investigation, but are not a capture produced by this
  worktree.

## 1. Origin of `outside-node`, `unaccounted` and `node processing`

Before the change, `apps/frontend/src/lib/telemetryAnalysis.ts` did:

```ts
const processing = total(rows, (phase) => phase.processing_ms);
const outsideNode = timings == null ? 0 : Math.max(0, timings.total_ms - processing);
```

When the difference crossed the threshold, the diagnosis returned
`kind: "outside-node"`. So yes, it was E2E minus the accumulated sum of the
hops' `processing_ms`.

There is no separate Rust computation called `unaccounted`. The unnamed rest
inside `processing_ms` showed up in the UI as `unattributed`; that is a local
residual of the node's breakdown, not proof of time outside the node. The
processing aggregate lives in `PhaseTrace::record` and `PhaseTrace::merge`, in
`crates/node/src/inference/prefill/support.rs`.

The defect is conceptual: `processing_ms`, RTT, callbacks and distinct peers are
sums of evidence, while `total_ms` is wall-clock. A pipeline can have
overlapping processing and transport. The difference does not identify which
interval happened and cannot be presented as an exclusive breakdown.

## 2. Why the second peer showed `decode = 0`

The old chain worked like this:

1. `handle_step` measured the local hop and `relay_and_bubble` kept the
   downstream `hop_timings`.
2. `remote_stream_producer` received that metadata when calling the local step.
3. `RemoteStreamToken` kept only `processing_ms` and `node_timings`.
4. `remote_stream_batch` serialized only those fields.
5. `RemoteDecodeStream::next` created an entry only for hop 0 and did not walk
   `meta["hop_timings"]`.

So the second peer's spans were generated in the relay, reached the remote
producer and were dropped at the `remote-stream` boundary. They were not spans
correlated to another trace, nor spans that simply arrived late. The observable
result was `decode = 0` for the downstream peer.

Now each callback keeps the ordered downstream list, each entry carries
`span_count` to tell tokens from callbacks, and the gateway records the head
plus every hop received. The causal identity carries `trace_id`, `request_id`,
`pipeline_id`, `stream_id`, frame index and span IDs in profiling mode.

## 3. `remote-stream` versus `libp2p-tunnel`

The difference was confirmed in the code. The ring/direct path through
`libp2p-tunnel` already kept `SentFrame` and `hop_timings`, and the gateway
walked the returned list. `remote-stream` only kept the head per batch and did
not record frame, lifetime, wait, first delivery or aggregated remote cost.

The new path does not turn the callback into an inference RTT. The first
control frame records the transport that opened the stream. The callbacks
record locally lifetime, interval between deliveries, queue wait, bytes and
remote processing, without subtracting clocks of different machines.

## 4. Late receipts, correlation and loss

Before, `ReplyInbox::deliver` removed the `ReplyState` immediately and
`record_transport` only worked while the ticket existed. The old test
`a_result_does_not_wait_but_late_transport_receipts_close_the_trace` documented
that later receipts were rejected.

`reply_to` uses a UUID per frame. An unknown ticket gets `410`, and cannot fall
into the next token. Now the delivered state is retained for 30 s, out-of-order
receipts update `trace_snapshot`, duplicate receipts per hop are refused and the
answer to the client still does not wait for reconciliation. After expiry, the
discard stays explicit and does not retroactively become unknown time.

Besides, when the result wins the `select` of `await_result_with_ack`, the
head's ACK continues in a detached task and records the transport as soon as it
finishes. The test `a_result_first_keeps_the_head_ack_for_transport_reconciliation`
proves that order without making the client wait.

## 5. Regression as a function of tokens

The two points of the real case are:

| Tokens | E2E | Node processing | historical `unaccounted` |
|---:|---:|---:|---:|
| 11 | 1.232 s | 545 ms | 662 ms |
| 192 | 8.807 s | 1,124 ms | 7,653 ms |

The fit of those two points is:

```text
per_token = (7653 - 662) / (192 - 11) = 38.624 ms/token
fixed = 662 - 38.624 * 11 = 237.1 ms
R2 = 1.000
```

`R2 = 1` is degenerate: two points always define a line. The result suggests an
incremental cost per token, but does not separate callback, transport, wait,
remote compute or overlap. It was not used to change the runtime.

The analysis function `fitTokenLatencyRegression` was added, with a test using
the required sizes 1, 10, 50, 100, 200 and 500. The test uses an exact synthetic
series of `237 + 38.6 * tokens`, returns six samples and `R2 = 1`. That
validates the computation tool, it does not replace a real E2E capture.

## 6. Telemetry model before and after

| Aspect | Before | After |
|---|---|---|
| E2E gap | `total - sum(processing)` classified as `outside-node` | Not computed as a breakdown; the gap stays unclassified while there is no evidence |
| Remote hops | Callback head only | Head and every downstream `hop_timings`, with `span_count` |
| Closing | Result and trace ended together | `response_complete` separate from `collecting`, `response_complete` and `complete`; a late ACK also reconciles the head |
| Transport | Late receipt was dropped | Short retention, arbitrary order, deduplication and updated snapshot |
| Remote stream | No frame/lifetime/wait metrics | Aggregated counters and histograms: open, lifetime, frames, bytes, first delivery, latencies min/p50/p95/p99/max, compute min/p50/p95/p99/max and wait total/p50/p95 |
| TTFT | One ambiguous field | `local_engine_first_event_ms`, `pipeline_first_token_ms`, `server_ttft_ms` and `client_ttft_ms` |
| Overlap | Implicit in the E2E difference | `overlap_ms` and `true_residual_ms` null until there are enough intervals |
| Profiling | No detailed stream correlation | `POM_PROFILE_SYNC=1` enables frame spans and bounded per-token events |

`critical_path_ms` in the summary is the measured wall-clock of the request. It
is not a complete causal reconstruction while there are no synchronizable
timestamps between peers. The detailed spans use the producing peer's monotonic
domain.

## 7. Integrity and states

`TelemetrySummary` publishes `expected_peers`, `reported_peers`,
`missing_peers`, `expected_decode_spans`, `reported_decode_spans`,
`completeness_percent`, `duplicate_spans`, `late_spans`, `orphan_spans`,
`wall_clock_ms`, accumulated compute, accumulated transport and the stream
aggregates.

The computation caps completeness at 100%. A peer without decode is not
considered covered just because it did prefill: when there is decode tracking,
peer coverage uses `decode.reported_spans`. So the missing second peer case
stays explicitly pending.

## 8. Changed files

### Node

- `crates/node/src/inference/prefill/support.rs`: serializable contract,
  aggregates, histograms, deduplication, collection states and profiling.
- `crates/node/src/inference/prefill/step.rs`: stream opening, head callback,
  downstream `hop_timings` and control transport.
- `crates/node/src/inference/infer_server/step.rs`: remote preservation and
  aggregation, causal identity and frame/lifetime/wait metrics.
- `crates/node/src/inference/direct.rs`: late receipts, retention, frame
  deduplication and trace snapshot.
- `crates/node/src/backend/gateway/budget.rs`: semantic TTFT and the telemetry
  summary update before serializing `x_pom`.

### Frontend

- `apps/frontend/src/lib/chatStream.ts` and `types.ts`: telemetry types, spans,
  aggregates and client TTFT.
- `apps/frontend/src/routes/Chat.tsx`: TTFT measurement in the browser.
- `apps/frontend/src/components/ExecutionTelemetry.tsx`: coverage, state,
  accumulated compute/transport, stream metrics and distinct TTFTs.
- `apps/frontend/src/lib/telemetryAnalysis.ts`: classification without
  `total - processing` and analysis regression.
- `apps/frontend/src/i18n/messages.csv`: new states, metrics and labels.
- Matching tests in `apps/frontend/src/lib` and
  `apps/frontend/src/routes/Chat.telemetry.test.tsx`.

### Documentation

The maps of the touched folders were updated, plus the subsystems
`docs/subsystems/inference.md` and `docs/subsystems/http-api.md`, including the
semantics of `remote-stream`, ticket, coverage and quantiles.

## 9. Matrix of the ten items of section 20

| Item | Proof in the code |
|---:|---|
| 1 | `outside-node` in `apps/frontend/src/lib/telemetryAnalysis.ts`, from `total_ms - processing`, before the change. |
| 2 | There was no independent Rust computation of `unaccounted`; the concept was the UI diagnosis and the local text residual. |
| 3 | `PhaseTrace::record` sums `processing_ms`; `PhaseTrace::merge` aggregates calls, with separate RTT and network fields. |
| 4 | `summary_for` sets `trace.complete` and now calls `refresh_telemetry`; `ReplyInbox` keeps collecting after the answer through short retention. |
| 5 | `reply_to` correlates by UUID ticket; `trace_id`, `request_id`, `pipeline_id` and frame/span IDs complete the stream correlation. |
| 6 | `handle_step_stream`, `remote_stream_producer`, `remote_stream_batch` and `RemoteDecodeStream::next` are the recording and propagation points. |
| 7 | `hop_timings` was dropped between `RemoteStreamToken` and the callback, leaving downstream without decode. |
| 8 | `record_transport` now accepts receipts after `deliver`, updates the snapshot, and `await_result_with_ack` keeps the head's late ACK. |
| 9 | There was deterministic loss of `hop_timings` and late loss of receipts; today both are visible through coverage, duplicates, delays and retention. |
| 10 | There is no E2E partition by summing spans; duplicate callbacks are deduplicated and overlap/residual only show up when measured. |

## 10. Tests and validation

Passed:

- `rtk cargo fmt --all`;
- `rtk git diff --check`;
- `rtk cargo test -p node remote_stream`: 27 tests;
- targeted tests of coverage, opening ACK, the head's late ACK, profiling
  spans, late receipts, duplicate receipts and aggregation without extra
  synchronization;
- `rtk npm test -- --run`: 80 files, 541 tests;
- `rtk npm run build`: TypeScript and Vite done;
- targeted frontend tests: 22 tests, including the regression on the six
  cardinalities and the parity of the two locales.

The full run `rtk cargo test -p node` ended with 1157 tests passed, 5 ignored
and 8 failures outside telemetry:

- `admin::tests::a_secondary_admin_cannot_promote_another_node`;
- three `deploy::whole_route_tests` about catalog selection/enrichment;
- `prefix_reuse_lab::unknown_session_recovery_is_bounded`;
- `spec_lab::um_orcamento_acima_de_n_batch_nao_e_truncado`;
- `webui::dead_address_tests::a_dead_announced_address_is_forgotten_and_the_peer_is_read_at_the_new_one`;
- `webui::tests::merge_parts_joins_two_slices_into_a_whole`.

Those failures repeat in the dirty worktree with changes from other work and
none of them is a telemetry test failure. The full suite cannot be declared
fully green in that shared state.

## 11. Update on the QUIC path, the card and the visual cycle

The operator's traces on `e19640c2`, without QUIC, settled the hypothesis that
the MTP gain already happens on the distributed path: `tokens/decode = 1.99`,
generation of `26.4 tok/s` and TTFT of `840 ms`, against about `22.5 tok/s`
before. The same trace observed the A -> B hop over `http`, with samples of
`51/66/68/130 ms`. So the card that still showed `86.0 ms` of libp2p was
reading a separate probe, not the Chat's execution.

This round keeps transport selection in a single place: QUIC is tried when the
preference is on; when it is off, the persistent channel is preferred; the URL
fallback publishes `http` or `https` according to the real URL. On fallback, the
metadata is encoded again per attempt so a TCP success does not show up as
QUIC. MTP uses the same `SentFrame` and the same transport identity, so there is
no hardcoded label in the handoff.

The trace now also publishes `pipeline_cycle.decode`: each sample is attributed
to the logical leg that observed it, in the order `Gateway -> head`, advances
between peers and return. The panel reads that cycle and shows the effective
transport on the edge's badge. The final Gateway total sums each leg once plus
the hops' compute, but only closes when every leg has a measurement. The
duration of a transport operation is directional in the sense of the logical
attribution; it is not divided by two, because the code still has no
synchronized monotonic clocks between hosts to prove a physical path. The
persistent `remote-stream` callback does not wait for the ACK per token and so
does not get an RTT made up from the interval between batches.

The canvas stopped drawing return edges between peers. It only draws the
advances, puts a virtual Gateway at the far end, links the last peer to it and
shows the network + compute breakdown in the Gateway card. A leg without
evidence stays `RTT -` and the total stays `não medido` ("not measured").

The new production data that motivated this round is also recorded: the
GPU -> CPU tail residual was `1924-2376 ms` in total, about `9.5 ms/token`;
transport still accounted for about `44%` of the critical path and
`remote-stream wait` had a p50 of `75 ms/token`. The message
`Telemetria de transporte incompleta` ("Incomplete transport telemetry") was a
false positive when there were `2/2` peers and `100%` of the spans: the state of
distributed spans is now computed separately from the late reconciliation of
transport receipts.

Limitation kept on purpose: in the persistent callback and in the asynchronous
final delivery there is no round-trip duration, in the same envelope, observed
before the body is sent. The report and the UI keep that gap explicit; they do
not replace the value with the libp2p probe, with `frame latency`, with zero or
with a subtraction from the E2E.

### Scenarios A-L

- A/B/C/D: `direct_lab::the_cost_per_token_does_not_depend_on_where_the_entry_point_is`
  walks 2 to 4 hops, different entry points and the lab's artificial RTT.
- E: `spec_lab::resposta_de_um_token_mantem_as_taxas_finitas` passed.
- F: the long answer scenario exists in `spec_lab`, but
  `um_orcamento_acima_de_n_batch_nao_e_truncado` is among the pre-existing
  failures above.
- G: `remote_stream` has 27 unit tests, including callbacks, downstream hops,
  three-peer coverage and aggregated metrics. There was no real E2E without a
  served model.
- H: the direct lab covers the `relay/v2-direct` answer and the path without
  switching to `remote-stream`; the tunnel is represented in the unit transport
  evidence.
- I/J/K/L: late receipt, missing peer, duplicate and out-of-sequence order are
  covered by the `direct` and `prefill` tests.

## 11. Overhead and remaining limitations

In normal mode, the trace uses counters, incremental sums and samples limited to
1024 elements. Deduplication keeps at most 1024 recent frame IDs. There is no
per-token vector nor extra per-token synchronization in the hot path. The
callback uses a queue limited to 64 batches and backpressure is still part of
the transport.

`POM_PROFILE_SYNC=1` keeps at most 1024 frame/event spans. Per-token events
without an individual interval are zero-duration events: they represent
participation and order, not measured compute. It was not possible to measure
the overhead percentage against a real inference because the environment
served no model.

Known limitations:

- there is no two-peer Ornith E2E capture in this worktree;
- `overlap_ms` and `true_residual_ms` stay null without enough synchronizable
  intervals;
- `local_engine_first_event_ms` stays null because the backend does not publish
  that explicit local milestone yet; pipeline/server TTFT are measured;
- the late `ReplyInbox` snapshot is an internal primitive and does not cause a
  live UI update after `x_pom` was already sent;
- after expiry, orphan spans are not persisted in a global counter;
- quantiles are exact up to the bounded sample and approximate after the
  reservoir;
- the stream carries downstream aggregates per callback, not an independent
  detailed profiler on each peer.

Conclusion: the implementation fixes the proven loss of the second peer and
stops calling the E2E difference `outside-node` when the distributed execution
is not explained. The breakdown now tells apart known evidence, pending
telemetry and accumulated measurements from an exclusive partition the system
cannot prove yet.

## 12. Phase 2 - root cause of the slowness and the authorized fix

This section brings in the telemetry of the run provided by the operator and
the reading of the code path. The operator's numbers are not a new local
capture. The observed baseline was 90 tokens in 4.78 s, about 23 tok/s
distributed against about 112 tok/s on one node, and about 11 tok/s when the
edge measurement was 807 ms.

### 12.1 The edge RTT is not ICMP

`crates/node/src/network/network.rs` calls `infer_tunnel::measure_rtt`. The
measurement starts before `TunnelOutbound::send` and ends when the
`TunnelRequest::get("/health")` gets the answer through the libp2p
request-response. So the 51.1 ms of edge `5b03 -> 02eb8c40` are an application
round trip in the tunnel: swarm queue, eventual dial/establishment, send,
remote processing of `/health` and return. It is not the physical ICMP RTT and
neither opens nor measures the `https-link`.

The direct ICMP ping to the remote machine provided by the operator has a median
of about 28-33 ms, with peaks of 67 and 114 ms. The difference to 51.1 ms cannot
be attributed only to the physical network: the edge measurement includes the
protocol and the `/health` endpoint. In the direct HTTPS path, the frame clock
starts before `reqwest::send` and ends after headers, body and validation; in
the `https-link`, opening is a separate operation (`https-link-setup`) and each
frame's round trip starts after the persistent channel exists. So the setup is
not multiplied per token.

### 12.2 Where the ~44 ms/token went

The direct computation of the real case is `4,780 / 90 = 53.11 ms/token`, not
44 ms. The provided telemetry also reports 1,670 ms of accumulated compute,
2,543 ms of observed transport and 1,296 ms of `remote-stream wait`:

| Evidence | Total | Average over 90 tokens | Interpretation |
|---|---:|---:|---|
| Accumulated compute | 1,670 ms | 18.56 ms/token | Sum of processing on the peers, not exclusive wall-clock |
| Observed transport | 2,543 ms | 28.26 ms/token | Includes crossings and application cost; not pure ICMP |
| `remote-stream wait` | 1,296 ms | 14.40 ms/token | Real interval waiting for an item in the queue |
| E2E | 4,780 ms | 53.11 ms/token | Critical path measured by the gateway |

Those rows overlap and do not form a breakdown: 18.56 + 28.26 + 14.40 =
61.22 ms/token. That confirms why `E2E - sum(spans)` is not a valid
correction. The avoidable floor is not obtained by subtracting those averages.

Peer B's old return did one request/reply unit per token. `HttpsLink` already
reused TCP/TLS, but each frame still waited for the answer before the next
round advanced. The operator's telemetry records about 37 ms per frame, 88
calls, 234 KiB sent and 134 KiB received, with a p50 of 36-38 ms and a max of
61 ms. That is application cost exposed by the synchronous protocol, not a
repeated handshake. The `remote-stream wait` with p50 13 ms and p95 16 ms was a
real wait for the item to reach the queue, not an artificial timer found in the
code. The emitter waited on `recv()`; the problem was that producer and delivery
were weakly decoupled and the HTTP callback could impose the path's cadence.

The physical floor for an autoregressive chain contains at least one state
crossing per token and the compute needed on A and B. The proven avoidable cost
is the serialization of each callback and the request/reply per frame on the
return. Token N+1's compute cannot consume N's hidden state before N arrives,
so there is no arbitrary overlap between A's and B's compute without
speculation. The gain allowed here is to carry answer N in parallel with the
preparation of the next cycle that does not depend on it, using a bounded queue
and a persistent channel. MTP/speculation and the MoE executor were not used.

### 12.3 Jitter and propagation without damping

The return frame's p50 was about 42 ms, p95 43 ms and max 71 ms. Together with
the ICMP peaks of 67-114 ms, that is consistent with a chain that exposes the
network tail to the generation time: without a buffer, a peak blocks the next
callback and, through autoregressive dependency, the whole answer. The previous
telemetry did not allow saying whether each peak was ICMP, callback or compute;
the new instrumentation separates `token_delivery_interval_*`,
`remote_stream_wait_*`, frame latency and remote compute. The E2E benchmark
still has to collect per-token quantiles in the same RTT~30 ms scenario to close
the propagation proof, instead of inferring it from the ping alone.

### 12.4 What was implemented

1. The `remote-stream` producer computes a round, puts each token in a queue
   bounded to 64 entries and can start the next round without waiting for the
   callback. The emitter delivers the items in parallel. The queue absorbs
   short jitter bursts and only applies backpressure when delivery is
   persistently slower than compute.
2. The `remote-stream` return to the gateway uses the persistent channel
   `/v1/link` when the capability is present. The internal A -> B crossing now
   also admits `remote_stream=true` in the `BatchCoordinator`, so it reuses the
   persistent channel and keeps the correlation by `batch_id`. The long trace of
   `65e88d10` was captured before this change and confirmed the old behavior:
   one `libp2p-tunnel` call per activation. The answer is still awaited to
   respect the autoregressive dependency.
3. The remote head's return to the gateway also uses `/v1/link`, with a
   `REMOTE_STREAM_RESULT_PATH` frame inside a full-duplex channel. The gateway
   delivers the frame to the ticket and only returns the envelope's ACK. If
   opening or using the persistent channel fails, the sender falls back to the
   per-callback HTTPS POST and marks each callback of that stream with
   `remote_stream_transport_fallback=true` and
   `remote_stream_transport_fallback_reason`.
4. The UI always shows `HTTPS fallback: N`, including zero, and the bounded
   reasons. The Rust layer aggregates `https_fallback_calls` and
   `https_fallback_reasons`; the indicator does not depend on renaming the
   transport.
5. Opening the channel and the frame round trip have distinct spans in
   `POM_PROFILE_SYNC=1` mode. Normal mode keeps cycle/delivery counters,
   histograms and quantiles, with no per-token event vector.

`remote_stream_wait` was not falsely zeroed. It is still the `recv()` measure of
the emitter's queue. The fix reduces the wait caused by serial callbacks and
makes the residual wait observable; zeroing it for presentation would erase the
evidence. The queue does not remove the physical floor and does not turn the
autoregressive chain into speculative execution.

## 13. Before/after and gain measurement

It was not possible to run Ornith with two peers and a served model in this
worktree: the local UI says `The network is not serving any model`. So there is
no honest before/after tok/s number nor a published gain promise. The baseline
the operator must repeat is:

| Scenario | Reported baseline |
|---|---:|
| One node | ~112 tok/s |
| Distributed, edge RTT ~51.1 ms | ~23 tok/s |
| Distributed, edge measurement ~807 ms | ~11 tok/s |
| Direct ICMP to the remote machine | median ~28-33 ms, peaks 67/114 ms |

For the comparison, use the same prompt, model, number of tokens and route, with
`POM_PROFILE_SYNC=1`, and compute `output_tokens / (total_ms / 1000)`. Record
before and after: E2E total, TTFT, token cycle p50/p95/p99/max, delivery
interval p50/p95/p99/max, `remote_stream_wait` total/p50/p95, frame latency
p50/p95/max, remote compute, transport, HTTPS fallback and bytes. The tok/s gain
can only be declared after that run. The implementation's success criterion is
that the normal path shows zero HTTPS fallback, a reused persistent channel, and
that a delivery peak does not freeze the producer until the queue is exhausted.

## 14. Phase 2 files and telemetry model

- `crates/node/src/inference/link.rs`: the persistent framed channel accepts
  callback frames without requiring a `frame/v1-batch` answer; setup, send,
  round trip and receive are separated in profiling.
- `crates/node/src/inference/direct.rs`: the callback handler inside
  `/v1/link`, `RemoteStreamDelivery` with explicit fallback and a delivery test
  to the ticket.
- `crates/node/src/backend/gateway/http.rs`: `/v1/link` on the gateway for the
  full-duplex return, with validation of the internal credential per frame.
- `crates/node/src/inference/infer_server/step.rs`: 64-item queue, concurrent
  producer/emitter, token index and delivery quantiles; the wait is still
  measured, with no fixed timer.
- `crates/node/src/inference/relay.rs`: persistent path before tunnel and
  HTTPS, with the fallback reason propagated in the receipt.
- `crates/node/src/inference/prefill/support.rs` and `prefill/step.rs`:
  fallback counters, bounded spans, callback intervals and token cycles.
- `crates/node/src/backend/gateway/budget.rs`: the summary no longer fills
  `outside_node_*` by subtracting accumulated processing from the E2E.
- `apps/frontend/src/lib/chatStream.ts` and
  `apps/frontend/src/components/ExecutionTelemetry.tsx`: contract and
  presentation of HTTPS fallback and delivery quantiles.
- `apps/frontend/src/i18n/messages.csv`: labels in both locales.
- `docs/subsystems/inference.md`, `docs/subsystems/http-api.md` and the
  `AGENTS.md` maps: operational contract of the persistent callback and
  fallback.

## 15. Phase 2 validation, overhead and limitations

Validations completed in the worktree:

- `rtk cargo check -p node`;
- `rtk cargo test -p node --no-run`;
- `rtk cargo test -p node remote_stream -- --nocapture`;
- `rtk cargo test -p node persistent_remote_stream -- --nocapture`;
- `rtk cargo test -p node remote_stream_https_fallback -- --nocapture`;
- `rtk npm test -- --run`: 80 files, 541 tests;
- `rtk npm run build`.

`rtk cargo fmt --all` and `rtk git diff --check` must be repeated after the
final rebase. Normal mode creates no per-token spans. The bounded queue and the
aggregated counters only add the state needed for backpressure and histograms;
the detailed profiling cost stays behind `POM_PROFILE_SYNC=1`, with a sample
limit.

The main limitation is the lack of a synchronized clock between peers: the
monotonic timestamps of A and B are not subtracted from each other. Telemetry
measures causal relations by `trace_id`, `stream_id`, frame and `token_index`,
and shows absence when the boundary was not measured. Another limitation is
that the physical RTT and baseline numbers came from the operator; the final
gain depends on repeating the E2E on the cluster.

## 16. Recommendation for the operator's benchmark

Run the same scenario with the published code, first with the normal path and
then forcing a controlled drop of `/v1/link` only to validate that the indicator
shows the counter and the reason. Do not use the controlled drop to measure
normal throughput. Compare the JSON reports of `x_pom.trace` and the
throughput, and attach the values at the end of this report. If the normal
channel keeps a high `remote_stream_wait` even though the queue stays below 64,
the next diagnosis must be the gateway's delivery point or the peer's compute,
not a new `outside-node` subtraction.

## 17. Phase 3: proof of the 5 s, session identity and dial addresses

### 17.1 What the trace proved

The trace `trace-2sessoes-2026-09-09.md` is a two-leg run
`0bda9121 -> 02eb8c40`, still with the bad result: 13.5 tok/s in generation,
TTFT of 5.85 s, E2E of 20.3 s and 9.6 tok/s E2E. The decisive evidence is the
second peer's prefill, with `queue 5002.31 ms`, while the first peer has a queue
close to zero. That is not an estimate: `PrefillJob.queued_at` starts before the
submit and `NodeTimings.queue_us` includes the wait until the scheduler picks
it.

The code confirms the origin. `run_scheduler` in
`crates/node/src/inference/infer_server/core.rs` computes `new_session_slots`
only with free sequences, does not count `retained` sessions as capacity and,
for a new prefill, only calls `evict_retained()` after
`RETAINED_RECLAIM_GRACE = Duration::from_secs(5)`. The same loop uses
`tokio::time::sleep(RETAINED_RECLAIM_GRACE)` to re-evaluate the queue. So the
5002 ms observed are exactly the reclaim grace branch, not model compute, an
ICMP handshake or a clock difference.

There is an important nuance so the diagnosis does not become a false fix: a
logical session can have the same `session_id` on A and B, but the first arrival
on B does not have a local `Session` entry yet. So `is_existing(sid) == false`
on B's first cold prefill is expected by the current local model. The applied
fix marks internal frames with `pipeline_session=true` and does an immediate
reclaim only on that path; the five-second grace stays reserved for a
standalone request from another session. That keeps the protection against
evicting hot KV for a short call, without claiming that a nonexistent local
entry already exists.

### 17.2 `session_id` trail and removal of synthetic identities

The single origin is still in `backend/gateway/generation.rs`, where
`PrefixCache::acquire` gets a new UUID only on a miss. The same value is passed
to `prefill::run_with_options`, `prefill::step_stream` and the continuations.
The metadata builders in `inference/prefill/{pipeline,step}.rs` and the forwards
in `infer_server/{prefill,step}.rs` now carry both `session_id` and
`pipeline_session=true`. `/v1/session/rewind`, `/v1/session/rollback`,
`/v1/session/release` and `/v1/session/slot` keep receiving the same ID in the
JSON and in the `x-pom-inference-session` header.

A loss point on the wire was found and fixed: when decoding a one-item batch,
old metadata could remain and hide the envelope's identity.
`llama-engine/src/wire.rs` now explicitly overwrites `meta.session_id` with
`BatchItem.session_id`; the test
`decoded_session_identity_comes_from_the_batch_envelope` reproduces the
divergent case and passes. `handle_prefill`, `handle_step`, the batch
coordinator and the persistent return no longer create `__session__`: a frame
without an ID is rejected and shows up as a protocol error.

The result is a statically proven propagation of one identity per frame, not
proof that B already has a local session before the first prefill. The
scheduler test `prefill_de_pipeline_nao_entra_na_carencia_de_pedido_avulso`
confirms the new immediate admission behavior for the distributed case. It
still has to be confirmed on the cluster that the next trace does not show the
5 s queue and that the `session_id` seen on A, B and on the return is the same.

### 17.3 What was proven about `outside-node` and `unaccounted`

Today those fields are not `E2E - sum(spans)`. The path in
`inference/prefill/support.rs::refresh_outside_node_accounting` computes
`outside_node_accounted_ms` as `critical_path_ms(&spans, "gateway")` and
`outside_node_unaccounted_ms` as
`outside_node_total_ms.saturating_sub(accounted)`. However
`outside_node_total_ms` is only fed when there is an explicitly measured
residual; `backend/gateway/budget.rs` does not make it up from the E2E and the
accumulated compute. So, in the current trace, "total outside the node: not
measured" is an honest absence, while `accumulated_compute_ms` and
`accumulated_transport_ms` are diagnostic sums that can overlap and do not form
an exclusive breakdown of the wall-clock.

### 17.4 New cut of the `remote-stream`

The previous trace showed a total `remote_stream_wait` of 14311 ms, p50 65 ms,
p95 69 ms, frame p95 70 ms and max 1400 ms, with remote compute of only 784 ms.
The old wait mixed the time in which the producer had not generated the token
yet with the time of a ready token sitting in the channel. The new
instrumentation carries an internal, non-serialized monotonic timestamp in each
`RemoteStreamToken` and publishes, per batch, these counters and quantiles:

- `remote_stream_producer_wait_*`: from the start of the cycle until the
  producer builds the token;
- `remote_stream_queue_wait_*`: from the ready token until the emitter receives
  it, including any backpressure of the 64-item bounded channel;
- `remote_stream_wait_*`: the total interval between deliveries, kept to
  preserve comparability and reveal overlap.

The test `emitter_separa_tempo_do_produtor_e_fila_de_entrega` forces an already
ready token and shows it lands in the second share. Aggregation was also
covered by
`remote_stream_aggregation_is_order_independent_and_deduplicates_frames`. That
lets the next trace tell a slow producer from a blocked delivery without
artificially zeroing the `remote-stream wait`.

### 17.5 Unannounced addresses and libp2p churn

It was confirmed in the code that `advertise_addr` did not replace a list of
the `Swarm`'s external addresses: there is no call to
`Swarm::add_external_address` in the repository, and Identify keeps reporting
the local listen addresses by libp2p's contract. Before the change,
`announce_addresses` put the pin first and appended the local addresses; on
receiving Identify, `info.listen_addrs` was copied to Kademlia; mDNS and the
tunnel fallback could also feed the address book. That explains the exposure of
LAN addresses, `172.17.0.1` and the old Tailscale address in the "expired peer
on address" logs.

Now, with a pin such as `100.x.y.z:42777`:

- `Announce.addr_candidates` contains only the pin, and the node's own answers
  to the gateway also return only it;
- the received Announce does not append the UDP origin when there is already an
  explicit list;
- the pipeline, the network chat and the `ensure_tunnel_peer_address` fallback
  use a single canonical endpoint, and the gossipsub warm-up only records the
  matching multiaddr;
- listen addresses received through Identify do not go into Kademlia; old
  entries from the same event are removed. The next boot also does not build
  the mDNS behavior when there is a pin, because mDNS itself emitted the
  `discovered/expired peer on address` logs before the handler could ignore the
  event. Without a pin, LAN discovery stays available.

The policy keeps multiple candidates only for the UDP control path when there is
no pin and for compatibility with old announcements. There is no more libp2p
fan-out from local interfaces once the endpoint is pinned. The tests
`a_pinned_address_replaces_local_announce_candidates`,
`libp2p_dial_uses_addr_canonico_e_ignora_candidatos_legados` and
`tunnel_addresses_preserve_only_the_explicit_advertised_set` pass.

The first redeploy of this round revealed an additional operational cause
before any throughput measurement: the operator's deploy script (outside the
repository) passes the full override `100.x.y.z:42777` to
`scripts/gen-node-configs.py`. The old generator copied that endpoint literally
to the four processes, even though the configs kept `p2p_port` 42777, 42778,
42779 and 42780. So all of them announced the same derived UDP/TCP socket,
collapsing the peers and preventing a valid `0..41` coverage; the E2E call
reached the gateway and returned 503 with `Nenhum pipeline válido` ("No valid
pipeline"), while `/peers` still showed only the remote range 16-40. The
evidence was taken from the four configs and the HTTP answer, not inferred from
the timeout.

The generator now reads `HOST:PORT` as a common host and only rewrites the port
to each process's own `p2p_port`. The four tests in
`scripts/test_gen_node_configs.py` pass. The mDNS pin was also validated on the
next boot: no new internal `discovered/expired peer on address` logs showed up;
the old records only remain in the historical file. On the remote machine, the
ports per service were already distinct, but the configured bootstrap still
pointed to an old Tailscale address, an obsolete separate target that still had
to be removed or replaced in operations. The redeploy and the E2E measurement
after the port fix were still pending.

### 17.6 State of the round and next numbers

Local validations of this round: `cargo test -p node --no-run`, the Node suite
with 1270 tests and 5 ignored in serial mode, tests of the remote aggregation,
the producer/queue split, the identity wire, the scheduler admission and the
canonical addresses. The frontend suite passed with 541 tests and the
TypeScript/Vite build passed. The parallel run of the Node suite had one
isolated flaky failure in the prefill lab, which passed alone and in the serial
run. There is no new post-fix E2E number in this worktree: the last value
measured by the operator is still 13.5 tok/s of generation, 9.6 tok/s E2E and
TTFT 5.85 s.

### 17.7 E2E after the `session_id` fix

On 2026-09-10, after the operator put `e0c747e` on both machines, a single cold
call was run through the gateway machine, with `POM_PROFILE_SYNC=1`, a 29-token
prompt, `max_tokens=16` and a non-streaming answer. The result was HTTP 200 and
the full trace was:

- `trace_id=dea4ae159bbf45ef887fb79683caadd4`;
- `request_state=cold`, `telemetry.status=complete`, `response_complete=true`;
- 2/2 peers, 2/2 decode spans, 100% completeness, zero duplicates, zero late
  spans and zero orphans;
- 1,520 ms E2E, TTFT of 1,314 ms, 2 tokens and `tokens_per_sec=1.3158` on the
  wall-clock. The isolated generation stretch was 206 ms, or `9.7087 tok/s`;
- prefill of 1,013 ms and `prefill_tokens_per_sec=28.6278`;
- `https_fallback_calls=0`.

The second peer's artificial queue disappeared. On hop `02eb8c40` the trace
recorded `node_queue_ms=0.043`, `node_scheduler_wait_ms=0.019` and
`node_decode_ms=602.768`; there is no `queue` close to 5,002 ms. The first hop's
wait also stayed at `node_queue_ms=0.052`. That proves the propagation's primary
goal: the path did not enter the fixed reclaim grace.

The trace still shows the bottleneck left for the next iteration: the remote
return aggregated one frame with a lifetime of 204 ms, `remote_stream_wait` of
203 ms and `remote_stream_producer_wait` of 203 ms, while
`remote_stream_queue_wait` was 0 ms. So in this run the residual wait is
production/internal round trip, not the callback's local queue. Hop B had a
decode of 99 ms and a round trip of 182 ms in decode; A's telemetry recorded the
stream opening through `libp2p-tunnel` and a later switch to `remote-stream`,
with no HTTPS fallback. The trace also contains the detailed spans of both
peers, correlated by the same `trace_id`, `request_id` and `pipeline_id`.

This result is a valid baseline, but it is not a 16-token throughput
comparison: the model ended the answer at 2 tokens. The next benchmark must
force a long output and measure TTFT, generation, p50/p95/p99 of the per-token
cycle and the full-duplex return separately. Commit `65e88d10` implements that
last cut: persistent callbacks are queued without waiting for the individual
ACK, keeping backpressure in the bounded queue and asynchronous failure
monitoring.

The next round had to be deployed on both machines, with the remote machine on
the same version and the gateway machine announcing its host on its own ports
42777-42780. The objective criterion is: the second peer's prefill queue without
the 5 s, no dial to an old LAN/bridge/IPv6 address, a normal return through the
persistent channel, and a before/after comparison of TTFT, E2E tok/s,
p50/p95/p99 per token, max frame, producer/queue split and HTTPS fallback count.
Until that run, any throughput gain would be speculation.

### 17.8 Consolidating the profiling control and the asynchronous return

The last remnant of the old control was removed from the prefill backend. The
function is now called `profile_sync_enabled` and only reads
`POM_PROFILE_SYNC`; a search of the code, documentation, configuration and the
frontend contract does not find the legacy variable nor the old helper.
`POM_PROFILE=off|basic|trace` is still the trace level, while
`POM_PROFILE_SYNC=1` enables the detailed collection of synchronized spans and
token events. They are distinct policies and there are not two variables for the
same control.

The per-callback ACK barrier was also removed from the normal path.
`RemoteStreamDelivery` now frames and queues each return on the full-duplex
persistent channel; the ACK is still recorded to detect an error and discard the
channel asynchronously. The 64-frame bounded queue keeps backpressure without
reintroducing a round trip per token. The test
`callbacks_persistentes_nao_esperam_ack_de_cada_frame` proved eight frames with
an artificial 200 ms ACK without waiting for the ACKs individually. The HTTPS
fallback is still used only when persistent delivery fails, with an explicit
reason in the telemetry.

In the full validation before that tweak, the Node bench ended with 1,263 tests
passed, 1 failure and 5 ignored: the fixture
`spec_lab::quem_rascunha_no_anel_gera_o_texto_inteiro` completed the fake
prefill in under 1 ms, correctly making the public rate that uses whole
milliseconds null, but the test required it to exist. The isolated reproduction
passed; the test now checks `prefill_tokens > 0`, without making up a rate from
a zero duration. Both bench scenarios passed again after the tweak.

The next full run, serialized after the fix, ended with 1,272 tests passed and
5 ignored.

### 17.9 E2E after removing the per-callback ACK

On 2026-09-10, with `65e88d10` on both machines, `POM_PROFILE=trace` and
`POM_PROFILE_SYNC=1`, the same cold request was repeated through the gateway
machine: the Ornith model, prompt `Responda apenas OK.` ("Answer only OK."), 29
input tokens, `max_tokens=16` and a non-streaming answer. The full trace
returned by the gateway was `27485236485841f99d02ff8e4dd1d08d`, with pipeline
`0bda9121 -> 02eb8c40`, and recorded both peers and the detailed spans of both
nodes.

| Metric | `e0c747e` / `dea4ae15` | `65e88d10` | Observed difference |
|---|---:|---:|---:|
| TTFT | 1,314 ms | 1,316 ms | +2 ms |
| E2E wall | 1,520 ms | 1,515 ms | -5 ms |
| Generation | 206 ms | 199 ms | -7 ms |
| Generation | 9.7087 tok/s | 10.0503 tok/s | +3.5% |
| Remote-stream lifetime | 204 ms | 197 ms | -7 ms |
| Remote-stream wait | 203 ms | 197 ms | -6 ms |
| Remote-stream queue wait | 0 ms | 0 ms | stable |
| Decode B | 99 ms, RTT 182 ms | 99 ms, RTT 174 ms | stable compute, RTT -8 ms |
| HTTPS fallback | 0 | 0 | stable |

The second peer stayed without an artificial queue: `node_queue_ms=0.043`,
`node_scheduler_wait_ms=0.021` and `node_decode_ms=600.699` in prefill, against
no value close to 5,002 ms. The session identity fix stays confirmed.

Removing the ACK did not produce a material gain in this short run. Both tokens
were delivered in a single frame (`frames_sent=1`), so there was no long
sequence of ACKs to multiply the removed cost. The frame had a 197 ms lifetime,
a 197 ms production wait and a 0 ms queue wait; that shows the residual wait is
in the remote cycle/compute, not in the ACK or the local queue. The small 3.5%
improvement in rate and 7 ms in the frame is consistent with the observed RTT
variation, since B's RTT also dropped 8 ms and TTFT rose 2 ms.

There is an important semantic change visible in the trace:
`trace.complete=true`, but `transport_trace_complete=false` and
`telemetry.status=response_complete`. That is expected on the new path: the
answer to the client does not wait for the callback's ACK; the ACK is observed
in the background. The delivery was accepted by the persistent channel and
there was no fallback, but transport reconciliation stays independent of the
answer's closing.

Conclusion of this measurement: removing the ACK removed the per-frame barrier
without regressing the path and kept the normal return on `/v1/link`, but the
dominant bottleneck is still the synchronous request/reply of each activation
between A and B. The trace shows `libp2p-tunnel`, one decode call on B with an
RTT of 174 ms, while the `remote-stream` return is already a persistent frame. A
long output is needed to quantify the ACK gain; the next tweak must make the
A -> B crossing persistent too, keeping the autoregressive dependency.

### 17.10 Long E2E to measure generation

Still on 2026-09-10, a long request was run on the same gateway machine with
`max_tokens=200`, using the prompt `Escreva hello world 30 vezes. Não escreva
explicações.` ("Write hello world 30 times. Do not write explanations."). The
model ended naturally at 90 tokens. The full trace was
`3d5f2efe26264d4581680e1169ffe91f`, with 2/2 peers, 178/178 decode spans,
`complete=true` and `response_complete=true`.

| Metric | Result `65e88d10` |
|---|---:|
| TTFT | 636 ms |
| Generation | 6,280 ms |
| Generation tok/s | 14.33 |
| Wall tok/s | 13.01 |
| Remote-stream frames | 89 |
| Frame/delivery p50 | 70 ms |
| Frame/delivery p95 | 74 ms |
| Frame/delivery max | 96 ms |
| Producer wait total / p50 / p95 | 6,209 ms / 69 ms / 74 ms |
| Queue wait total | 0 ms |
| Remote compute total / p50 | 357 ms / 4 ms |

The result is about 6.2% above the 13.5 tok/s generation reference, but about
23.8% below the 18.8 tok/s reference. The 6.2% cannot be attributed to removing
the ACK: the previous two-token sample varied B's RTT by 8 ms, and there is no
long before/after on the same commit. The strongest causal evidence is another
one: 89 frames had `queue_wait=0`, while almost all of `remote_stream_wait` was
`producer_wait`; so removing the ACK eliminated the return's queue, but not the
producer's wait.

The producer waited a p50 of 69 ms per token. The attempt with 100 repetitions
produced 99 tokens before being stopped by the repetition detector, so it is not
used as the final throughput, although it confirmed a `producer_wait` p50 of
70 ms, p95 of 74 ms, total queue of 0 ms and 98 `libp2p-tunnel` calls on peer B,
with an RTT p50 of 65 ms, p95 of 69 ms and max of 96 ms. So the critical path
still exposes the tail of each internal request/reply, without damping by a
persistent A -> B channel.

Operational conclusion: the persistent return is working and removing the ACK
was not the remaining bottleneck. The local change now does the same on the
internal hop: `send_frame_or_tunnel` admits `remote_stream=true` on the
correlated persistent path, keeping the causal wait of one token before the
next. The long E2E had to be repeated after deploying that change, with a
prompt that avoids the repetition detector and with TTFT, generation, wall,
p50/p95/p99 and HTTPS fallback recorded separately.

### 17.11 E2E after the persistent A -> B channel

After deploying `15caf764` on both machines, with `POM_PROFILE=trace` and
`POM_PROFILE_SYNC=1`, the long `max_tokens=200` request was repeated through the
gateway machine. The model ended at 90 tokens. The full trace was
`chatcmpl-f553641ffcc849fba1fb666f1582d4c7`, with 2/2 peers, 178/178 spans,
`trace.complete=true`, `telemetry.status=complete` and zero HTTPS fallback.

| Metric | `65e88d10` | `15caf764` | Change |
|---|---:|---:|---:|
| TTFT | 636 ms | 1,367 ms | +731 ms |
| Generation | 6,280 ms | 4,146 ms | -2,134 ms |
| Generation tok/s | 14.33 | 21.71 | +51.5% |
| Wall tok/s | 13.01 | 16.33 | +25.5% |
| Remote-stream wait p50 | 70 ms | 44 ms | -26 ms |
| Remote-stream wait p95 | 74 ms | 48 ms | -26 ms |
| Producer wait p50 | 69 ms | 44 ms | -25 ms |
| Queue wait total | 0 ms | 0 ms | stable |
| Frames | 89 | 89 | stable |

The generation gain beats both the earlier 13.5 tok/s reference (+60.8%) and
the 18.8 tok/s reference (+15.5%). The sample, however, must not be used to
claim a TTFT improvement: TTFT rose 731 ms in this isolated run, even though the
prefill took 1,065 ms and there was no reclaim queue; a controlled cold/warm
series is needed to separate startup and jitter from the hop's effect. The
generation target was reached: decode time dropped 34.0% and total wall-clock
dropped 20.3%.

A second long read, done only to extract the per-hop fields, had 96 tokens and
ended with `telemetry.status=collecting`; that is why it does not enter the
final rate. Even so, it closed the transport proof: on peer B, decode had 95
`https-link` calls, RTT min 35 ms, median 38 ms, p95 43 ms and max 43 ms, with
zero fallback. The 95 per-token `libp2p-tunnel` calls seen in the previous
diagnosis did not happen. The 95 return items show up on A as `remote-stream`;
the only remaining `libp2p-tunnel` call is the stream's control/opening frame.
The internal frame now reuses a single persistent connection, exactly as the
bench test published in `link_lab` requires.

The new trace also confirms that the local queue did not become the bottleneck
again: `remote_stream_queue_wait_total_ms=0`,
`remote_stream_producer_wait_p50_ms=44` and
`remote_stream_producer_wait_p95_ms=47`. Remote compute added up to 373 ms, with
a p50 of 4 ms. The drop from about 70 to 44 ms per round is therefore
consistent with the proven switch from libp2p request/reply per frame to the
`/v1/link` channel; the autoregressive dependency stays serial, but the
transport and framing overhead per activation is no longer paid as a new call.

The earlier long trace of 65e88d10 remains the generation comparison, and the
later diagnostic trace only serves to validate the transport. The next
benchmark must repeat at least five runs with the same prompt, separate the
first run from the warm channel and publish the median/p95 of TTFT, generation,
wall and A -> B RTT.

### 17.12 Closing the per-token critical path

The diagnostic trace `chatcmpl-d5d8e71f686d4962abb4775e1865e6b5`, although it
ended with `telemetry.status=collecting` because of the repetition detector, has
95 complete cycles and lets us close the critical path arithmetic:

| Component | Measurement | Correct reading |
|---|---:|---|
| A decode | 381 ms / 95 = 4.0 ms per token | Head compute before sending the next state |
| B decode | 1,046 ms / 95 = 11.0 ms per token | It is inside the RTT of the A -> B call, must not be added again |
| A -> B `https-link` | 95 calls, p50 38 ms, p95 43 ms, max 43 ms | One frame per token on the persistent channel, still with a required answer |
| Producer cycle | p50 43 ms, p95 48 ms, max 50 ms | A compute + A -> B RTT + residual framing/scheduling |
| Callback queue | p50/p95 0 ms, total 0 ms | No emitter/gateway blocking adding wait |

So the 43-44 ms called `remote_stream_wait` are the causal wait for the next
token, not a timer, ACK or hidden idle queue. The reported physical RTT of
28-33 ms is the network floor; the application RTT of 38-43 ms includes B's
work and the protocol. The difference between the two measurements cannot be
attributed entirely to the network, but it also does not show up as a separate
wait in the trace.

There is no additional arbitrary overlap for a single sequence: B cannot compute
N+1 before receiving from A the state derived from token N, and A cannot
produce that state before receiving/sampling B's answer. The trace also rules
out the hypothesis of unnecessary serialization on the return: `queue_wait` is
zero, the callback does not wait for the ACK to free the producer and the
persistent channel was reused without reopening per token.

The `wall tok/s` field does not reveal stream overhead in this sample. It is
lower than `generation tok/s` by exactly the TTFT: 5,513 ms total - 4,146 ms of
generation = 1,367 ms of TTFT. The rest of the gain has to come from reducing
the number of causal cycles, not from subtracting overlapping spans again.

Implementation conclusion: the `15caf764` change removed the avoidable cost of
opening/requesting the internal transport per activation and raised generation
to 21.71 tok/s. To move toward single-machine throughput, the next technically
valid mechanism is a multi-token round (MTP or speculation) when the runtime
announces the capability and the model is ready. Forcing multiple tokens on a
model without a draft would be wrong; that change was not made and the vendored
llama.cpp fork was not touched.

### 17.13 Orphan stream fix

Besides the normal cycle, a resource serialization case was found that could
contaminate later measurements: `handle_step_stream` returned the accept ACK
and then awaited `producer` and `emitter` with `tokio::join!`. If the gateway
closed the ticket, the emitter ended on seeing `410 Gone`, but the producer kept
running the autoregressive loop, holding the KV session, the load permit and new
scheduler entries until the next callback failure or timeout.

`run_remote_stream_tasks` now runs both sides as sibling tasks. If the emitter
ends first, the producer is aborted and its `JoinHandle` is awaited; if the
producer ends normally, the emitter still drains the bounded queue to deliver
the last token. The test
`emissor_para_quando_a_entrega_termina_antes_do_produtor` reproduces a hung
producer and confirms it is dropped when delivery ends. The change was published
in `82c17b11`. It fixes leaked work and keeps abandoned requests from skewing
later throughput, but it is not claimed as a tok/s gain until it is redeployed
and measured on the cluster.

### 17.14 Series of five E2E runs after `82c17b11`

With `POM_PROFILE=trace` and both peers serving, five sequential calls were run
through the gateway machine. The request was the same as in the previous series
(`Escreva hello world 30 vezes. Não escreva explicações.`, `max_tokens=200`,
non-streaming answer). All returned HTTP 200, with `trace_complete=true`,
`response_complete` and zero HTTPS fallback.

| run | tokens | TTFT (ms) | generation (ms) | generation tok/s | wall tok/s |
|---:|---:|---:|---:|---:|---:|
| 1 | 94 | 1,361 | 4,276 | 21.98 | 16.68 |
| 2 | 90 | 591 | 3,969 | 22.68 | 19.74 |
| 3 | 90 | 310 | 3,996 | 22.52 | 20.90 |
| 4 | 60 | 602 | 2,661 | 22.55 | 18.39 |
| 5 | 90 | 1,414 | 3,932 | 22.89 | 16.84 |

The set produced 424 tokens. The arithmetic mean was generation **22.52
tok/s**, wall **18.51 tok/s** and TTFT **856 ms**; the medians were 22.55 tok/s,
18.39 tok/s and 602 ms. Against the `15caf764` reference (21.71 tok/s of
generation and 16.33 tok/s wall), that is +3.7% and +13.3%, respectively. The
comparison has different natural lengths and does not fix a seed, so the mean
does not causally isolate the stream cancellation.

A controlled interruption of a stream after about 1 s was also done. Before the
abandonment, both infer servers had `queue_depth=0` and 429 batches/items.
After the abandonment, the queue kept reporting zero, but the counters advanced
to 441/442 immediately, 468/469 after 1 s, 517/519 after 2 s, 635/636 after 5 s
and 1,185/1,186 after 15 s. After 35 s, the gateway machine's health expired,
while the remote machine still showed 1,344 batches/items. The conclusion is
that `queue_depth` measures admission wait, not active work: the callback
abandoned callback kept consuming the session and the scheduler.

### 17.15 Root cause of the persistent abandonment and the prepared fix

There are two complementary failures:

1. The pump between `generate_stream_inner` and the HTTP adapter only waited
   for an event on `inner_rx`. Since the generator keeps another clone of the
   sender to close the session, the `Sender::closed()` observed inside the
   generation did not promptly detect the client that abandoned the stream,
   especially when no event was available yet. So the generation kept using KV
   and remote resources.
2. On the persistent `/v1/link` path, sending the callback returns before the
   ACK. When the gateway answers `410 Gone`, the ACK task only marked the
   channel as dead and removed the record. `RemoteStreamDelivery` kept seeing
   `Delivery::Accepted`, so `run_remote_stream_tasks` did not get
   `GatewayGone` and the producer stayed active.

The local fix makes the pump sensitive to `client_tx.closed()` in parallel with
receiving events and cancels the generation immediately. The asynchronous ACK
now publishes an explicit state: `410 Gone` becomes `GatewayGone`, while a
transport error still triggers the HTTPS fallback with a visible reason. The
persistent delivery propagates that state to the producer, which ends the
stream without falling back to HTTPS when the client only abandoned the ticket.

The regression tests passed in isolation: the pump cancels without an event,
`callback_assincrono_sinaliza_gateway_desaparecido`,
`ack_410_do_callback_para_o_stream_sem_fallback` and the `link` (13) and
`direct` (19) suites. The binary's full suite passed with **1,268 tests passed
and 5 ignored** in a serial run. The fix was published in `146c2deb` and
validated on both machines in section 17.16. There was no change to llama.cpp
and no start of MTP/speculation.

### 17.16 Cluster validation after `146c2deb`

Commit `146c2deb` was deployed on both machines with `POM_PROFILE=trace`. The
gateway machine had 4/4 healthy nodes, including node-3 with Ornith and slice
0-15; the remote machine had 3/3 healthy containers and the model loaded. The
gateway used was `<gateway-host>:8080`, with the same prompt as the previous
series and `max_tokens=200`. Run 1 was done before runs 2-5 and the five traces
returned a complete answer, 90 tokens, no error and `trace.complete=true`:

The `transport_trace_complete` field came back `false` in run 1's answer, as
already observed on the asynchronous ACK path; that did not prevent the main
trace from closing (`trace.complete=true`) nor the answer from being delivered.
The HTTPS fallback field was zero in all five answers.

| run | trace_id | tokens | TTFT (ms) | generation (ms) | generation tok/s | wall tok/s |
|---:|---|---:|---:|---:|---:|---:|
| 1 | `chatcmpl-08b9bc6b6b8647b2bacce77adca73e52` | 90 | 1,344 | 4,181 | 21.53 | 16.29 |
| 2 | `chatcmpl-b4d9a9ed902b42578e37511cce42f73c` | 90 | 599 | 3,937 | 22.86 | 19.84 |
| 3 | `chatcmpl-f86db09b7c33444eae0d07eead2a72a1` | 90 | 317 | 3,971 | 22.66 | 20.99 |
| 4 | `chatcmpl-aed8d19eca9d47809d7d746826742158` | 90 | 1,877 | 3,941 | 22.84 | 15.47 |
| 5 | `chatcmpl-8b799f5fa3444f2792f61ca5ec0de2c7` | 90 | 582 | 3,919 | 22.97 | 20.00 |

The mean was **22.57 tok/s of generation**, **18.52 tok/s wall** and **944 ms
of TTFT**. The medians were 22.84 tok/s, 19.84 tok/s and 599 ms. Against the
`15caf764` generation reference (21.71 tok/s), the mean gain is **+4.0%**;
against that reference's 16.33 tok/s wall, it is **+13.4%**. Generation stayed
practically at the level of the `82c17b11` mean (22.52 tok/s), which does not
attribute a measurable throughput gain to the cancellation alone. TTFT ranged
from 317 to 1,877 ms, so it still depends on jitter and hot/cold state.

The abandonment test opened the same request in streaming and closed the client
with `--max-time 1`; curl ended with code 28 after 1,007 ms and 2,308 bytes
received. Before the abandonment, the infer servers were at 455 batches/items
each. On the first read after closing, the counters were at 469/469 on the
gateway machine and 468/468 on the remote machine, with `queue_depth=0`; that
small advance is work already in flight. In every following read the counters
stayed unchanged:

| moment | gateway machine batches/items | remote machine batches/items | queue_depth | `/health` |
|---|---:|---:|---:|---|
| immediate | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +1 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +2 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +5 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +15 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |
| +35 s | 469/469 | 468/468 | 0/0 | `ok`/`ok` |

On node-3, the log file had 124,092 bytes before the round, 124,524 bytes after
the five generations and 124,821 bytes at the abandonment; it stayed stable at
124,821 bytes from immediately until +35 s. The exact count of
`Too many open files` was zero in every read of the abandonment. The limit
stayed at `ulimit -n=1024`; the processes used at most 128 FDs (and dropped
gradually to 121-29 per process in the last read), with no sign of a pileup. So
`146c2deb` passed this scenario without requiring a higher ulimit, although the
low limit remains a separate operational risk.

This round confirms the commit's specific goal: closing the client did not
leave batches growing after the first observation interval, both health
endpoints stayed responsive for 35 s and the FD spam did not come back. It does
not solve the architectural limit still open: decode stays serial due to the
autoregressive dependency and TTFT/wall still carry the latency between hops.
MTP/speculation was not engaged yet in this measurement; the next section
records the cause found and the fix authorized by the operator.

### 17.17 Root cause of MTP not engaging in the pipeline

Before the fix, an E2E run on the cluster with `146c2deb` produced the trace
`chatcmpl-4a70496f44c24917aa68e55fddaa495f`. The request finished complete, but
had 89 decode calls and 89 frames, with `mtp_draft_tokens_proposed=0`,
`mtp_draft_tokens_accepted=0` and `mtp_draft_tokens_rejected=0`. That confirms
there was no round with an MTP batch, not just a low acceptance rate.

The pipeline negotiation was not the cause. On the same cluster, the plan met
the three conditions of `PipelinePlan::supports_mtp`: the head announced
`speculate/v1-mtp-sliced` and `speculate/v1-lookup`, and the tail announced
those two capabilities plus `speculate/v1-mtp`. The head slice's persisted
configuration also had `mtp_enabled=true` and `mtp_draft_tokens=3`. The health
of the remote peer `02eb8c40` confirmed `mtp_ready=true`,
`mtp_allocation="vram"` and `kv_can_shift=false`; the health of the head
`0bda91213fe5` had `mtp_ready=false`, as expected for the non-final slice,
which does not carry the `nextn` block.

The block was in `infer_server::step_blocking`, on both tail answer paths.
`mtp_tail_speculation_reason` received `s.engine.kv_can_shift()` and, even with
MTP ready, turned `kv_can_shift=false` into `hybrid_tail_no_rollback`. So
`next_draft` was omitted before calling `mtp_draft`, and the gateway stayed on
the one-token cycle. The `kv_can_shift` field is a call to
`llama_memory_can_shift`, related to `--ctx-shift`'s K-shift; it does not prove
whether the engine can undo a speculative batch. The pipeline's effective
rollback uses `LlmEngine::kv_remove`, which calls `llama_memory_seq_rm`.

The fix removed the `kv_can_shift` gate from the decision to produce
`next_draft`, keeping the refusals for `mtp_ready=false` and an empty draft.
Both paths still call `kv_remove` after the round and fail closed if the
effective removal is refused. So a ready recurrent model can try MTP, but never
moves ahead with speculative tokens the engine cannot revert. There was no
change to the wire nor to the vendored llama.cpp fork.

The regression test was first put in RED with the ready case and false K-shift,
reproducing the old reason, and then passed with the new signature. There is no
post-fix tok/s or MTP acceptance number in this section yet: it depends on
redeploying both peers. The next measurement must record `proposed`,
`accepted`, `rejected`, batch width, TTFT, generation tok/s and wall tok/s,
compared with the base of 22.57 tok/s of generation, 18.52 tok/s wall and TTFT
of 944 ms from section 17.16.

### 17.18 The Chat's real transport and the Dashboard's RTT

The operator's observation was reproduced by inspecting the path the product
calls. The Chat sends `POST /v1/chat/completions` with `stream: true`; the
handler `openai::stream_response` calls
`gateway::spawn_logged_generation_with_stops`, which is the same generation used
by the harness. There is no alternative inference route for the Chat. Inside
the generation, `inference::relay::send_batch_body` tries `CAP_LINK_STREAM` and
`link::send_batch_with_session_profile` before the libp2p tunnel or the HTTPS
fallback. So the persistent channel of `15caf764` was not exclusive to the
harness. The divergence was in the source of the telemetry shown to the user.

The endpoint `POST /api/ui/chat/telemetry` read the local `RttTable` and fanned
out to `GET /api/ui/network/rtt` on the peers. That table is filled by the
network's periodic monitor with `libp2p` probes; it is right for route
selection, but does not identify the transport of an inference frame. That is
why the Chat could show `libp2p` even when the execution's trace had
`https-link`, and the Dashboard card repeated the same value.

The fix makes the route receive `execution_id`, which is the same `chatcmpl-*`
id used as `trace_id` and audit key. The gateway reads the trace persisted in
SQLite, waits briefly for the write to finish when the query happens at the
same instant as the last SSE, and derives each edge from the downstream hop's
`transport_summary`. For `head -> tail`, hop zero's synthetic probe is not used:
the measurement comes from `hops[1].decode`, which records the effective
crossing between the peers. The Dashboard, which has no request id, reads the
last completed execution and filters the requested edges. Without a matching
trace or edge, the answer is empty and the UI shows unknown; there is no
fallback that renames `RttTable` as execution.

The network-free regression test seeds at the same time a 900 ms probe with
`libp2p` transport and an execution trace with 6 ms `https-link`. Both the query
with `execution_id` and the query without an id return only:

```json
{
  "from_node_id": "head",
  "to_node_id": "tail",
  "rtt_ms": 6,
  "transport": "https-link"
}
```

So the root cause shared with MTP was confirmed at the gateway integration
level: the real execution path already had the capabilities and the persistent
transport, but the decisions and the screen read the gateway's local state,
which does not exist on a pure gateway. The MTP gate now uses
`plan.supports_mtp()` and treats the absence of a local `NodeConfig` as unknown
configuration, without ignoring an explicit `Some(false)`. The logs also record
the MTP decision, `plan_supports_mtp`, `remote_stream_capable` and `sliced_mtp`
for the next field capture.

Files added in this round:

- `crates/node/src/backend/gateway/budget.rs` and `generation.rs`: plan-based
  MTP gate and decision logs.
- `crates/node/src/backend/reqlog.rs`: asynchronous read of the exact trace and
  of the last completed execution.
- `crates/node/src/backend/webui/catalog.rs`: per-edge RTT/transport derivation
  and removal of probe usage from the Chat endpoint.
- `apps/frontend/src/lib/queries.ts`: sending `execution_id` and
  execution-based card semantics.
- `crates/node/src/backend/webui/tests/core_b.rs` and
  `crates/node/src/backend/gateway/tests.rs`: regressions for endpoint, hop,
  libp2p probe and gateway without a local runtime.

Validation of this round:

- `cargo test -p node --bin pom gateway_sem_runtime_local_usa_capacidade_mtp_remota`: passed.
- `cargo test -p node --bin pom execution_telemetry_uses_downstream_edge_not_gateway_probe`: passed.
- `cargo test -p node --bin pom creates_then_completes_the_same_audit_record`: passed.
- `cargo test -p node --bin pom chat_telemetry_ignores_libp2p_probe_when_execution_trace_exists`: passed.
- `npm run build`: TypeScript and Vite done after the documentation update.
- `cargo test -p node --bin pom`: 1271 tests passed, 5 ignored.

Overhead: the MTP decision only does reads already available from the plan and
the local configuration, with no synchronization or I/O in the hot path. The
endpoint's trace read runs in `spawn_blocking` and only when the UI asks for the
snapshot, after the generation; the Dashboard query reads at most 32 aggregated
traces to find a matching topology. The normal inference path gained no
per-token spans nor extra network calls.

Limitation: a post-deploy E2E capture on both machines is still needed to
confirm that `mtp_draft_tokens_proposed` is no longer zero and to measure the
new generation. The telemetry fix makes that capture reliable, but does not
replace the production measurement. The `RttTable` stays deliberately libp2p
for the planner; it must not be used as execution RTT.

### 17.19 Post-deploy E2E 0c671617: MTP still detoured to libp2p

The Chat was exercised against the pipeline deployed with the `0c671617`
binary, `POM_PROFILE=trace` and a long 199-token answer. The persisted capture
was execution `chatcmpl-e4fc6ee32b2847f8b0d2d7287bc7b480`.

| metric | 0c671617 | recorded base |
| --- | ---: | ---: |
| generation | 21.77 tok/s | 22.57 tok/s |
| wall throughput | 20.43 tok/s | 18.52 tok/s |
| server TTFT | 601 ms | 944 ms |
| MTP proposed / accepted / rejected | 279 / 104 / 175 | 0 / 0 / 0 |
| acceptance | 37.28% | - |

The total time of this capture was 9.74 s for the 199 generated tokens.

The trace proved that MTP is already engaged on the real gateway, but edge
`0bda9121 -> 02eb8c40` recorded 94 downstream calls as `libp2p-tunnel`, with a
median RTT of 88 ms (min 74 ms, p95 91 ms). The same authenticated query to the
endpoint used by the Dashboard returned `transport=libp2p-tunnel` and
`rtt_ms=88`; so the 86 ms value observed before was not just a visual cache or a
renamed probe. The Dashboard was correctly reflecting that execution's
effective transport, which was still wrong.

The root cause shared with MTP was located in `relay::send_frame_or_tunnel`.
`send_batch_body` already prioritized `CAP_LINK_STREAM`, but the
`BatchCoordinator` selection refused any meta with `draft` or `mtp_k`. Each MTP
handoff to the next peer then fell straight into the libp2p tunnel. The initial
control of `/v1/step-stream` cannot be changed that way, since the receiver
expects its control wire and not `frame/v1-batch`.

The fix removes that exclusion only from batch eligibility. The MTP `/v1/step`
handoff is now wrapped as a one-item batch and goes through the existing order:
persistent channel (`https-link`), libp2p tunnel and direct HTTPS as fallback.
`reply_to` and `speculate` still keep the control batch out. The coordinator key
already separated speculative contracts, so MTP and dense frames do not mix.

The new test was first seen in RED with the old exclusion and passed after the
fix: `mtp_handoff_can_use_persistent_link_batch_transport`. The regression
`remote_stream_control_stays_out_of_persistent_link_batch_transport` keeps the
initial control's boundary. The redeploy of this fix is still needed to confirm
`https-link` in the Chat, an execution RTT close to the ping's ~30 ms and the
generation improvement. The numbers above are the measurement before the fix
and must not be presented as the final result.

### 17.20 Stream clocks, residual capture and the state of the blocks (2026-09-10)

#### Stream clocks

The raw file of trace `5b035803` is not present in this copy; the analysis below
uses the numbers provided by the operator. The code lets us locate the origin
of the three values:

| field | measured boundary | interpretation |
| --- | --- | --- |
| `stream_duration_ms` | first SSE data frame until `[DONE]`, in the browser | excludes TTFT and includes closing the body and any terminal tail |
| `time_last_token_to_response_end_ms` | last frame that carried a token until `[DONE]` | isolates the tail after the last token |
| `timings.total_ms` | request entry until `summary_for` on the gateway | server summary, created before the final rewind/release |
| `node_backend_completion_ms` | `llama_synchronize` on the peer | real barrier that includes queued CUDA work and pending asynchronous copies, not just kernels |

So `6.24 s`, `2.00 s` and `34 ms` cannot be three readings of the same boundary.
If `34 ms` is the tail of the same request, there are no 4.2 s hidden in that
stream's teardown. Generating 197 tokens at 100 tok/s explains about `1.97 s`
proportionally, but alone does not explain a `6.24 s` stream. That points to
fields from different requests/boundaries or an ambiguous UI reading, not a
proven fixed sleep.

The search of the decode path found no 2 s sleep. There are windows that can
show up in failures or specific paths: `ADMISSION_PROBE_TIMEOUT` of 300 ms,
`LINK_OPEN_TIMEOUT` of 2 s only when opening a link that fails,
`REWIND_TIMEOUT` of 3 s, a 10 s release and the 5 s grace to reclaim a retained
slot. The old 2.005 s admission probe was cut to 300 ms. None of them is a
normal fixed delay of single-machine decode.

The 907 ms of accumulated completion for about 197 calls give 4.60 ms per call.
The measurement is real as a barrier interval: the runtime calls
`llama_synchronize`, and the backend scheduler synchronizes the queued work and
the output transfers. It is proportional to the number of calls, but is not
pure kernel throughput; separating that wait requires internal backend
measurement, which the current contract does not publish.

#### Bucket A: residual capture

The previous field measured the whole edge around the callback and called all
of it D2H. The implementation now publishes `residual_capture_ready_wait_ms`,
`residual_capture_copy_ms`, `residual_capture_postprocess_ms`, bytes and the
number of copies, keeping `residual_capture_d2h_sync_ms` as a compatible
aggregate. The callback still reads the buffer while the scheduler guarantees
readiness, because llama.cpp can reuse the buffers between micro-batches;
deferring the read until `llama_decode` returns would lose earlier rows. The
read is one bulk f32 allocation and one `ggml_backend_tensor_get` call per
capture, instead of element-by-element conversion.

The evidence in the vendored code is `ggml_backend_sched_compute_splits`, which
calls `ggml_backend_synchronize` before the tensor-ready callback. So, if the
next traces show ~1468 ms in `ready_wait_ms` and ~1 ms in `copy_ms`, the
bottleneck is backend wait/synchronization, not the bandwidth of a 1 MB D2H. If
`copy_ms` stays high, the new count narrows the investigation to the transfer.
The optimization is instrumented, but is not yet claimed as a production gain
without a redeploy and a comparable capture.

#### Bucket B: prefill handoff

`relay::send_frame_or_tunnel` now wraps the direct `/v1/prefill` in a one-item
batch on the persistent channel when the hop announces `transport/link-v1`. The
QUIC preference still wins when enabled; without it the effective link is
`https-link` or `http-link` according to the URL used, with an explicit fallback
and reason for libp2p/direct HTTP. The test covers the prefill handoff without
`BatchCoordinator`, which was the gap that kept prefill in the tunnel even after
decode changed transport. Confirming the time and transport in the Chat depends
on the next deploy.

#### Bucket C: sliced MTP

The gate keeps MTP for the tail of a sliced pipeline when the plan publishes
`CAP_SPECULATE_MTP_SLICED` on every hop and `CAP_SPECULATE_MTP` on the tail.
`tokens`, `mtp_k` and `next_draft` now cross each relay; the tail separates
`mtp_input_missing`, `mtp_runtime_disabled`, `tail_without_nextn` and an empty
draft, instead of calling every unavailability `model_without_mtp`.
`model_without_mtp` is now reserved for the absence of reason evidence on the
tail. The unit tests confirm selection and propagation, but the number of
proposals/acceptances in the deploy still has to be measured by the operator.

#### Delivery in blocks, validation and limits

The code blocks were published separately: capture A in `b3547968`, handoff B in
`450695b0`, and gate and propagation C in `5d4e9da2`. The formatting commit that
made the checkout clean is `de7936e4`; that is also the published tip after the
rebase.

Focused validations: `remote_stream` tests (32), MTP tests (22),
`persistent_link`, direct prefill handoff, separation of the capture metrics,
sub-millisecond timing serialization, `step_forward_meta`, `cargo check` of
`llama-engine` and `node`, plus `git diff --check`. The new normal overhead is
only clock reads and counters in the capture callback and metadata fields on
the MTP path; the detailed spans stay conditioned on profiling mode. The
post-deploy E2E measurements are still needed to quantify the real gain of A/B
and confirm C's behavior on every peer.
