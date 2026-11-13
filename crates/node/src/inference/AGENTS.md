# inference/ - the inference server, the wire format and the supervisor

> **Crate move:** `llm.rs`, `mtp.rs` (+ `llama_mtp_shim.cpp`),
> `collective.rs`, `device_placement.rs` and `wire.rs` left for
> `crates/llama-engine` (see its `AGENTS.md`). `node` re-exports them in
> `main.rs`, so `crate::llm::…`, `crate::wire::…` and the rest keep working.
> `tp.rs` **stayed** here (the collective talks to the primary over HTTP/tunnel);
> `llm` reaches its state through `llama_engine::tp_probe`, installed at startup
> by `tp::install_probe`. The descriptions below of the moved modules stay valid
> as a reference for what each one does.

The inference port (8181) and everything that runs on it: the axum server
(`infer_server/`), the llama.cpp FFI wrapper (`llm.rs`), the prefill/step
execution in the modes - hub, bubbled ring (`relay/v1-chain`) and direct reply
(`relay/v2-direct`, `direct.rs`) - plus the opportunistic batched remote
transport (`/v1/step-stream`) - the "law" that measures the direct mode over
network legs (`direct_lab.rs`), the speculation bench (`spec_lab.rs`), the
multi-token prediction through the model's `nextn` block (`mtp.rs` + the C++
bridge `llama_mtp_shim.cpp`), the binary format of the tensor on the network
(`wire.rs`) and the supervision of the embedded Rust server (`runtime.rs`),
which also keeps the annotation of the slice in service (state
Pending/Serving/Idle in `models/lastslice.rs`). When the per-deployment policy
asks for idle unloading, `RequestActivity` is evaluated by the existing monitor
off the inference path; the restore receives `reload_model` (`NodeRuntime::wake_from_idle`
starts the load and returns; `reload_from_idle` also waits, for the local
node), keeps `idle` announced while the model loads, waits for the runtime's
`/health` and announces `serving` again. The supervisor only starts the next
generation after the previous supervisor finishes the shutdown, including the
native release of the model, and that release runs in `spawn_blocking` so it
does not stall the HTTP executor. The gateway fires that event in parallel to
the planned idle peers and caps the barrier at 15 minutes.
The runtime queries the policy of `utils/thermal.rs` every 5 seconds, pauses the
model above the limit with state `thermal`, keeps the configuration for the
resumption and only redeploys after every known GPU stays below the reconnect
limit during the cooldown. `stop` clears that recovery to respect a manual
undeployment.
The inference routes between peers use the shared `connection_key` with the
marker `x-pom-internal-node: 1` on private networks and also on the public HTTP
fallback; HTTPS and QUIC may validate through the secure transport and the
marker, without depending on the secret. Client API keys never cross the hops.
Every frame rechecks the secret locally in constant time, with no extra call to
the network. The public key and the Ed25519 signature belong to the handshake
and to the peer's identity; they do not replace the symmetric credential of the
private channel.
The gateway validates API keys on the client surface, against the keys that
node minted; a key never leaves it and no other node is asked about one. On a
public network the inference port admits a TLS caller only when it is a
member (`auth::caller_is_member`). The result callbacks on
the gateway are another boundary: the per-frame `Ticket` is their credential.
The inference HTTP listener is dual-stack, or falls back to IPv4 when the
machine does not offer IPv6, regardless of how the administrative interface is
exposed: it is the reserve when the p2p tunnel fails.
The HTTP/3 listener uses a dedicated UDP port, announced in
`inference_quic_port`, derived from **this node's** `p2p_port`
(`quic::inference_quic_port` = `p2p_port + 1000`); never from
`inference_port`/`gateway_port`, which are consecutive between co-located nodes
and would make one node's QUIC port land on the neighbour's gateway port. The
client uses the announcement's public `host:port` addresses and falls back to
HTTP/HTTPS if the endpoint does not answer.
On the handoff between peers, the QUIC preference picks the HTTP/3 endpoint;
without it, the persistent link publishes `https-link` or `http-link` according
to the address actually used. Prefill and decode use this same ladder, and a
fall to the p2p tunnel or to direct HTTP stays recorded with the fallback reason.
The model is loaded **in-process** by `llm.rs`; there is no Python process. The
normal paths receive a ready `next_hops` from `routing/pipeline.rs`; TP is the
controlled exception, since it coordinates its ranks and its transport tunnel in
this directory.
llama.cpp runs every model this server loads (`executor::LLAMA_CPP_PATH`,
`dense/llama.cpp` in `/health`); there is no engine choice. A plugin's
inference service runs a whole model instead through `plugin_engine.rs`.

## Files

- `collective.rs` (new) - the core of the tensor parallelism collectives (path
  C): `reduce_sum` (the mathematical sum) and `ReduceBarrier` (a barrier that
  accumulates each rank's partials and returns the sum to everyone via `oneshot`,
  modelled on `ReplyInbox` in `direct.rs`). The HTTP transport
  (`POST /v1/tp/reduce`) is a thin layer, wired together with the llama.cpp
  fork.
- `device_placement.rs` - orders device handles from the slowest to the fastest
  only when the synthetic measurements have enough confidence and every handle
  belongs to the same backend; without that, the fit keeps llama.cpp's default
  order.
- `direct.rs` (474) - the direct reply: the tail hands the result to the gateway
  through `/v1/relay-result` instead of bubbling it up the nested stack; it also
  keeps the bounded inbox of the batched remote transport. The normal return of
  `remote-stream` uses a full-duplex `/v1/link`; a failure to open or to use it
  falls to `/v1/remote-stream-result` and carries an explicit HTTPS fallback
  reason. The direct result may be released before late transport receipts; the
  `ReplyInbox` keeps a bounded snapshot, deduplicates by ticket and hop index and
  reconciles it until it expires.
  The `reply_to` URLs come from the gateway's local interfaces, so behind
  NAT/CGNAT none of them is valid at the other end: `deliver_via_tunnel` is the
  last route, through the p2p tunnel to the
  `endpoint_id`/`endpoint_addrs` that `reply_to` now carries. Without it the
  frame died silently at the frame's deadline. The transport annotated in the
  metadata is `p2p-tunnel`, so the gateway's trace does not publish as HTTP a
  path that was not. A gateway on an older binary does not send the fields and
  delivery proceeds without the last route.
- `direct_lab.rs` (580, `#![cfg(test)]`) - the law: fake hops speaking the real
  protocol, a network emulated per site, demanding the theoretical minimum of
  legs per token for N=2,3,4 with the gateway at the head/middle/tail. The
  fixture announces only the capabilities the fake endpoint implements.
- `link.rs` (860) - the persistent link between floors (`transport/link-v1`,
  `POST /v1/link`): one streaming body going and one streaming body coming back,
  carrying successive `frame/v1-batch` frames, instead of one request per
  activation. Opening gives up to 400 ms per address and at most 800 ms to test
  two routes; timeout and refusal enter the failure memory, and the 5 min
  quarantine follows the routes' memory and avoids reopening the same destination
  after a failure. The frames travel in a `[size][status][batch_id][body]`
  envelope; the `batch_id` - the same one as `BatchCoordinator`'s - correlates
  going and coming back and is what allows several sessions on the same channel.
  One channel per edge (`node_id` + URL), opened under a per-edge guard so two
  traversals of the same edge share the opening. The opening happens outside the
  map's global lock. A transport failure degrades to the usual ladder; the
  destination that did not open stays 5 min in quarantine. An HTTP refusal from
  the peer is final and is not retried through another route.
  The receiver rechecks the `connection_key` and the internal marker **on every
  frame**, because a long connection authenticated only at opening would keep a
  secret valid for hours after another process had replaced it.
  `send_body_without_response_with_session` is the production path of the
  `remote-stream`: each frame is queued on the full-duplex channel with no ACK
  barrier per token; the bounded queue keeps backpressure and a reader task
  watches late ACKs and channel failures. `send_body_with_session` stays only in
  the tests that validate the HTTP ACK on the same persistent channel.
  The initial control of `/v1/step-stream` stays outside the batch, because the
  receiver expects that route's control wire. The normal `/v1/step` handoff with
  MTP, which carries `draft` and `mtp_k`, does enter as a one-item batch: that
  way it uses the same persistent channel and shows up as `https-link`. The
  `BatchCoordinator` key marks those batches as speculative so they are not
  mixed with dense frames. See `docs/subsystems/inference.md` §2.1.
- `link_lab.rs` (520, `#![cfg(test)]`) - the bench for the network cost per
  token on the internal hop: fake hops served over a counted `TcpStream`, so the
  number comes out of the bytes that passed through the socket and not out of an
  estimate. It compares the same chain with and without `transport/link-v1` and
  keeps the comparison (not the absolute time, which depends on the machine).
- `infer_server/` - the axum module split by base/scheduler,
  prefix/sessions, HTTP server, prefill, step/stream, direct forwarding and
  blocking step: `/v1/prefill`, `/v1/step`,
  `/v1/step-stream`, `/v1/link`,
  `/v1/session/release`, `/v1/session/rewind`, `/v1/session/rollback`,
  `/v1/session/slot` (save/restore/erase of a KV snapshot on disk, like the
  llama-server's `--slot-save-path` - see `inference.md` §3.1.1),
  `/health`; `perf.rs` measures the slice's speed (`run_bench`, a local test at
  model load, and `observe_prefill`/`observe_decode`, a moving average of the
  real traffic, called at the end of the decode in `prefill.rs` and
  `step_blocking.rs`); `/health` publishes `perf`, `active_generations` (sessions
  not retained), the RTT and the transport of this node's pinned routes
  (`route_rtt_ms`, `gateway_rtt_ms`, `route_kinds`, `gateway_route_kinds`,
  and `relayed_peers`, the peers it only reaches through a relay, inserted outside
  the `json!`, which is at the recursion limit) for the gateway's planner; the
  speed test sends the hidden state in the shape the capture produces
  (`perf::hidden_input_layout`: `[tokens, n_embd, streams]` on a model with
  several residual streams) and `POM_SLICE_BENCH=0` turns it off; session state,
  and the relay branch.
  The handlers publish `selected_path`, `executed_engine`, `executed_engines`
  and the `engine_mismatch`/`mixed_execution` flags in `node_timings`; `/health`
  publishes the selected path, since it does not represent a request.
  The first prefill's metadata also carries `thinking_enabled`, the Chat's
  per-inference choice. The head combines that value with `no_thinking` before the
  template, and the hub, ring and direct paths preserve it across the relays.
  State is accessed through `lock_inference_state`: a poisoned mutex becomes an
  HTTP error or a batch request failure, never a new panic in `unwrap`. EOS only
  closes the response accumulator; the session stays available for rewind until an
  explicit release or the idle collection.
  The prefill, step and step-stream handlers hold a `LoadGuard` on the tracker
  shared with the gateway while there is computation in flight; because of that
  the machine's own `/api/ui/node` can publish the real load, even when the
  request came straight to the inference port.
  Os handlers decodificam o frame e entregam
  commands to a bounded local queue; the worker selects one entry per session and
  may call the multi-sequence decode. Each `Session` keeps its own
  `Utf8Assembler`: `InferState::visible_piece` keeps the raw bytes (control
  tokens hidden, except the thinking and tool call markers in
  `preserved_specials`, which Flint's vocabulary marks as control and which
  otherwise reached the client as nothing), only the accepted
  positions feed the accumulator, and the `flush` happens on EOS or on the cleanup
  of an error/cancellation. `/v1/step-stream` starts a producer on the remote peer
  and a separate emitter: each one uses the same existing decoding, but the tokens
  return to the gateway in bounded opportunistic batches, with a 500 ms delivery
  timeout and backpressure.
  `node_timings` publishes the capture at explicit boundaries: scheduler
  ask-to-ready, the wall of the `ggml_backend_tensor_get` call, post-processing and
  the encode of the payload and the residual frame. Ask-to-ready includes the
  scheduler's compute and sync; `tensor_get` on CUDA may include D2H and the
  stream's sync, without exposing those internal costs separately. The wire
  measures the f32/f16 activation conversions on the peer and on the client, plus
  the f8 loop and the encode of the header/frame; the conversions are subsets of
  the general encode/decode times.
- `llm.rs` (1500) - the llama-cpp-sys-2 wrapper: it initialises the backend so
  sysmon can query device descriptions, load, decode, hidden state with residual
  capture, sampling, stopping by EOS or by the GGUF template's end of turn, the
  Gemma4 renderer derived from `tokenizer.chat_template`, the typed KV cache
  (`type_k`/`type_v`) and the ownership of the draft head (`Option<MtpDrafter>`).
  `token_to_piece_bytes` returns the piece without a lossy conversion for the
  session's accumulator.
  When `fit_memory` is active and there is more than one eligible device, it
  measures a generic GGML graph once per handle and passes the fit an explicit
  list from the slowest to the fastest only if every difference is trustworthy and
  the handles belong to the same backend. A heterogeneous combination or a missing
  backend name keeps llama.cpp's default order, since the explicit order can change
  the graph's copy and split topology across backends. The list stays in the
  `FitPlan` until the real load, making the last device receive the output head
  and the final range. A missing, equivalent or invalid result keeps llama.cpp's
  default selection, including running no microtest for a single device.
  `fit_plan` records the memory snapshot returned by the backend, the effective
  margin, the budget per device and the VRAM use of the PID itself when
  `nvidia-smi` provides it.
  `fit_plan` belongs only to the real loading path: it calls the FFI to adjust
  layers, context and the split between devices, and keeps the pointers alive until
  `LoadedModel::load`. The deploy's capacity preview no longer calls that path and
  has no `FitPreview`: it uses the analytic accounting of GGUF layout, weight, KV
  and free VRAM in `webui::capacity_preview_endpoint`.
  `runtime::engine_config_from_node` stays the `NodeConfig -> EngineConfig`
  conversion used by the real load and by the assembly of the estimate's
  parameters; both pass `cfg.needs_residual_export()` as `need_embeddings`. The MTP reservation does not open a model nor build a speculative
  graph: for known architectures, `fit_plan` only computes the deterministic KV of
  the `nextn` blocks from the GGUF metadata, and places it on the last device of
  the load order (from the fastest to the slowest), where the last layers sit. The
  compute cost is left to the real context creation, which can fall back to RAM
  without aborting the process.
  The capture identifies the token axis by the known number of tokens of the
  physical microbatch and by the tensor's shape. `n_embd_inp` validates a 2D shape
  or one with singleton extra axes; when there is a non-singleton extra axis,
  `n_embd_out` validates the expanded physical width reported by llama.cpp. Those
  widths do not take part in identifying the axis: every axis before the token axis
  is a feature and is preserved in physical order, publishing the canonical shape
  `[tokens, features...]`. Later axes greater than 1 are refused and later
  singletons are ignored. If more than one axis is still a candidate, the smallest
  axis that closes the contiguous strides is chosen deterministically. The
  accumulation keeps appending microbatches in the order produced with
  `kv_unified=true`. A shape or stride that does not close that geometry is refused
  with an explicit diagnostic. `decode_hidden` and `decode_hidden_batch` use the
  validated width to size the Rust batch. `decode_tokens_batch` and
  `decode_hidden_batch` build a `llama_batch` with independent sequences and return
  the layout to map logits and residuals per item.
- `scheduler.rs` - admission limits, the microbatch window and the FIFO round
  robin selection. The window defaults to 0 ms so it does not delay an isolated
  call, but it stays configurable for workloads that prioritise coalescing
  arrivals. Each process creates its own queue; replicas of the same range do not
  share a scheduler, KV or queue capacity.
- `mtp.rs` (445) - multi-token prediction (MTP): a draft context with
  `ctx_type = LLAMA_CONTEXT_TYPE_MTP` over the target's weights in VRAM; a CPU-only
  sibling model in the fallback to RAM; and the `observe`/`accept`/`draft`
  algorithm ported from llama.cpp's `common_speculative_impl_draft_mtp`. The `h`
  buffer follows `n_embd_out`, because the MTP context exposes the physical output
  width when the architecture has stream axes. `observe` passes positions, tokens
  and explicit hidden states in a batch that may be split internally into
  microbatches without losing the pairing; a failure to append or observe keeps the
  reason and shows up in `mtp_ready`/`mtp_unavailable_reason` on `/health`.
  When ready, `mtp_allocation` says whether the context stayed in VRAM or in RAM.
  The `fit_plan` fits the main model and its context first; it only accepts the
  reservation of the second context if the joint result preserves every main layer
  on the GPU. When it does not fit, the target stays without `nextn` weights in
  VRAM and, if allowed, loads the CPU-only MTP model with `offload_kqv=false` and
  `op_offload=false`. An override only on the `nextn` weights is not enough because
  the scheduler may also choose the device of the layer and of the output head.
  Before the MTP fit, `llm.rs` reads `general.architecture` and applies a list
  generated by the fork itself for the architectures whose
  `LLM_GRAPH_TYPE_DECODER_MTP` graph exists. `build/mtp_arch.rs` finds the effective
  verification in the model files, links the verification to the scope of
  `build_arch_graph`, links the class to the dispatch in `llama-model.cpp` and
  resolves the GGUF name in `llama-arch.cpp`. Comments that only mention the symbol
  do not count. llama.cpp's public API does not expose a support query; an unknown
  architecture stays unavailable, with a reason in `mtp_unavailable_reason`, before
  any MTP context. A missing fork, an empty extraction, fewer than thirteen entries
  or an inconsistent one makes the build fail loudly. A duplicated dispatch also
  interrupts the build. The real MTP context explicitly turns `embeddings` off,
  because its output is logits and `h_nextn`, not generic embeddings. MTP turned
  off does not read that metadata, does not estimate a budget and does not try to
  attach a context.
- `llama_mtp_shim.cpp` (48) - the `extern "C"` bridge for
  `llama_{set,get}_embeddings_nextn` and `llama_set_nextn_layer_offset`, which live
  in `llama.cpp/src/llama-ext.h` and therefore are **not** in the `llama-cpp-sys-2`
  bindings (its `wrapper.h` only includes `llama.h`). Compiled by
  `crates/node/build.rs`.
- `prefill/` - support and telemetry, prefill pipelines, steps/speculation and
  tests: `run_with_options` (hub) / `run_ring` (ring) / `run_direct` (direct),
  `step_speculative`, the two answer shapes of the speculation, and a per-request
  phase trace. `Cancellation` cancels the gateway's operation when the consumer
  disappears, before the next chunk or hop and also during the HTTP wait; a frame
  already accepted by the peer does not receive a new wire header.
  The continuation prefill carries `reuse_prefix` on the first chunk and absolute
  `start_pos`/`next_pos` on the following chunks, so the peers that receive hidden
  state keep the head's KV position. Chunk 0 may omit `start_pos` because the
  session provides `n_past`; any later chunk without a position is an error, never
  a position invented from the chunk index. The `reuse_prefix` the following hops
  receive is what the **head executed**, read from its answer
  (`cached_tokens_of`, `head_reused_prefix`), not what the gateway asked for.
  Intermediaries in an N-hop chain echo that decision instead of recomputing it.
  The head's partial answer, on both the individual and the batched path, also
  carries `n_prompt_tokens`, `start_pos` and absolute `next_pos`, including on the
  first chunk of a whole model. `PrefillOutcome::cached_tokens` returns to the
  gateway how many prompt tokens were not reprocessed; `retained_tokens` returns
  the stable boundary that stayed in the `prompt_seq` and that the gateway confirms
  in the rewind. Hub and direct reply run the prefill as a pipeline in order: a
  pump at the head dispatches the pieces in series and a downstream pump
  (`FuturesOrdered`) carries them through the remaining hops, up to `W` in flight.
  `PrefillOverlapWindow` limits
  by count (`prefill_max_inflight_chunks`, effective 1..8, default 8) and by
  hidden-state bytes in the gateway's RAM (`INFLIGHT_BYTE_BUDGET`, 512 MiB);
  each limit's permit travels with the piece. In the ring (`run_ring`), a window
  greater than 1 becomes `ring_overlap`: the gateway sends one piece at a time and each
  hop that forwards answers the intermediary after its own compute, forwarding in
  the background in order per session (`ring_relay_slot` in
  `infer_server/prefill.rs`); the last piece is synchronous. The window cancels the
  futures in flight on error or on cancellation and preserves the order of the
  result. A piece `k>0` that fails after the chain accepted the earlier ones attaches
  a `PrefillResumable` to the error for diagnostics, but the gateway does not repeat
  that point after a transport failure: the lost answer does not prove whether the
  peer already wrote the chunk, and re-entering could ask for a partial KV removal.
  The safe recovery recreates the whole session cold, once; if it fails, the cleanup
  releases the hops reached. Even without a resume point, the error carries the
  partial `RequestTrace` observed up to the failure for the audit; that changes
  neither the retry nor fills missing times. The ring mode stays serialized by the
  nested contract and ignores the window. The telemetry also records
  `prefix_match_tokens`, `prefix_reuse_status`,
  `prompt_tokens_transmitted`, `prefill_pipeline_depth_max` and the transport per
  traversal. `RequestTrace` adds the `trace_id`, the gateway's local spans,
  explicit `outside_node` fields only when there is a measured residual,
  cold/warm state, new/reused connections, first-token marks and decode counters.
  `RequestTrace::telemetry` separates `response_complete` from `trace complete`: it
  publishes the expected/observed peers and spans, duplicates, delays and the state
  `collecting`/`response_complete`/`complete`/`incomplete`. The sum of compute and
  transport is accumulated, never subtracted from the wall-clock to fabricate
  `outside-node`; overlap and a true residual only appear when there are explicit
  intervals. The `remote-stream` carries the `hop_timings` of the head and of the
  downstream peers, besides bounded counters and histograms of frames, waiting and
  remote processing. The producer uses a bounded queue of 64 items and the emitter
  splits `remote_stream_wait` into production, channel reservation, queue and
  previous callback, with no artificial timer. `remote_stream_downstream_wait_ms`
  identifies the nested relay inside the production; `node_timings.mtp_draft_ms`
  identifies the synchronous cost of the nextn and `node_timings.mtp_observe_ms` the
  one of `observe`. `RemoteStreamTelemetry::merge` also joins the distributions of
  `downstream_wait` and `delivery`; without that the p50/p95 of those two fields came
  out zeroed in the answer although the totals were right. On a sliced remote plan
  the MTP is asked of the remote head, as on the legacy path. `handle_step_stream`
  copies the control frame's `session_id` into the `reply_to`, and each returning
  batch puts it back in the envelope before reaching the ticket. The normal return
  uses the persistent channel and only the fallback uses HTTPS by callback, with a
  visible count and reason. The normal mode does not serialize spans per token;
  `POM_PROFILE_SYNC=1` adds frame spans and token events with trace, request,
  pipeline and peer IDs. Token events without an individual interval stay with zero
  duration, without artificially repeating the batch's cost. The
  `off|basic|trace` profile only controls the new detail and does not replace
  `POM_PROFILE_SYNC`. The `pipeline_cycle` aggregate keeps the traversals in their
  logical direction (`gateway -> head`, advances and return), per edge and phase,
  without dividing the RTT by two or adding a return trip that does not exist. On
  the `remote-stream`, the advances are rebuilt from the `hop_timings` and the
  callback records the emitting peer and the effective transport; since the
  persistent callback does not wait for an ACK, its RTT stays absent until a matching
  observation exists.
  In direct mode, `expected_decode_spans=0` means the expected total was not
  measured; `decode_span_expectation_available=false` keeps the UI from presenting an
  `N/0` fraction as full coverage.
- `prefix_cache.rs` - the global KV session cache per model and route, with busy
  leases, the largest textual prefix and LRU only among idle entries. It holds no
  weight, tensor or file transport. The textual key is the canonical rendered prompt
  (`openai::conversation_cache_key`), in the same order the head tokenizes.
  `PrefixLease` identifies a miss as an empty cache, a matching session busy, a
  divergent prefix or a changed route. On the Anthropic path,
  `conversation_cache_key_with_budget` also includes the `task_budget` identity to
  keep the KV of another marker from being reused. `PrefixCache::adopt` records a
  session restored from disk as a busy hit. `PrefixDiskIndex` (Phase 3) is the
  optional disk layer: an index `(route, prompt, tokens, bytes, updated_at)` per
  `key_hash`, persisted in `prefix-auto-index.json` in the gateway's
  `slot_save_path`. `record` supersedes the strict-prefix ancestors of the same route
  (one lineage, one `.bin`), applies a configurable TTL
  (`slot_auto_ttl_secs` / `POM_SLOT_AUTO_TTL_SECS`, 7 days by default) and an LRU
  budget (`slot_auto_max_bytes`, 2 GiB by default), returning the `key_hash` to
  delete. The gateway still debounces by `SLOT_AUTO_MIN_GROWTH_TOKENS` before
  rewriting. A `pinned` entry (a shared system part, `gateway::SharedPrefix`) is
  never superseded. Wired by `slot_persist_prefix_cache`; see `inference.md` §3.1.1.
- `prefix_cache_lab.rs` - the differential self-test of the cache policy, that is
  of the **choice**. The reference follows lookup, insert, busy, release and
  eviction; the proxy deliberately breaks `cached_tokens` and the battery has to
  record the divergence. No execution defect is reachable through it.
- `prefix_reuse_lab.rs` - the bench of the **execution**: a four-turn conversation
  through the `/v1/chat/completions` path, on the three transports (hub, ring,
  direct). It asserts that from turn 2 onwards `cached_tokens > 0`, that the work per
  turn stops growing as the conversation grows, it records the time to the first word
  and checks the drop through the `prefill_ms` declared by the hop, not through the
  wall clock subject to the process's scheduling. It also checks that no hop received
  a request for partial KV removal. The position contract repeats ring and direct
  reply with N=3 and N=5 hops; the `openai.rs` SSE test reproduces the session at 42
  without `next_pos` and blocks the sending of chunk 1 at 512. The fake hops imitate
  the most restrictive engine the product serves - the recurrent one, which refuses
  partial removal - and keep the prompt's stable boundary separate from the generation
  sequence. The bench uses the production functions at the decision points
  (`plan_prefix_reuse`, `reuse_prefix_for_forward`,
  `overlay_head_authoritative_fields`, `prompt_progress_for_forward`). In direct mode,
  each fake hop also uses `DirectForwardOrder` to serialize the local ingestion and
  the forwarding of the chunks like the real server does; without that, the bench
  could deliver the next chunk before its predecessor under test load. It also
  simulates a hop that loses its session in the middle of the prefill: the gateway
  repeats the full prefill cold a single time, to rebuild the KV from the prompt, and
  the battery checks that a second loss ends with a bounded and useful error, counting
  at most two cold attempts on chunk 0, without using the total of frames as a proxy
  for the cleanup during the overlap window.
  The scenario of a cut in the middle of a prefill also confirms that no hop receives
  a partial removal and that the remaining sessions are released.
- `executor.rs` - `ExecutionEngine`, the single source of the engine label that
  goes out in the telemetry (`llama.cpp`, `plugin`; missing on an old peer it
  becomes unknown in the interface), and `LLAMA_CPP_PATH`.
- `plugin_engine.rs` - whole-model inference by a plugin's inference service
  (`RuntimeParams::inference_service`, for example `"strata"`). On the target
  node, `run` replaces `infer_server::run` inside the runtime supervisor: it
  binds the inference port, asks the plugin to load the model
  (`engine.load`, with the NVIDIA GPU indices `sysmon` reports and every GPU's
  vendor and per-vendor ordinal in `gpu_devices`, `gpus_of`), answers
  `/health` from `engine.status` with the same fields the llama.cpp server
  publishes (`model_loaded` only when the plugin is `ready`) and asks
  `engine.unload` when the generation ends; an `error` state, or `idle` after
  having served, ends the generation so the supervisor restarts it within its
  budget. While it runs, `Network::set_external_engine` makes the announce
  carry only `engine/plugin-v1:<code>` (plus the tunnel capability), never the
  hidden-state capabilities. On the gateway, `plugin_of_plan` recognises a
  plan whose steps are all whole slices served that way, and `generate` posts
  the rendered prompt to its last step's generation route (locally through `engine.upstream`, remotely
  through `POST /api/internal/plugins/:code/engine/generate`, which
  `engine_generate_route` serves) and turns its NDJSON stream into `GenEvent`s.
  `plugin_session.rs` keeps named conversations in step across the whole-model
  machines: the turn's saved delta (or the whole session) goes to each peer from
  a background task, after the turn's `done` reports the save (or its deferral:
  the task polls the bridge's `/pom/session/pending` until the save is written),
  and the session routes apply it.
  Prefill/decode split: a whole-model machine deployed with
  `RuntimeParams::inference_role` = `prefill` announces
  `engine/plugin-role-v1:prefill` (`Network::set_external_engine_role`,
  `role_capability`). Every machine of the split is a head too; `prefill_peer`
  picks, per turn, an idle `prefill` machine for a free head that is not one
  (a busy head reads its own prompt), and `prefill_on_peer` sends it the named
  turn (`prefill_only`, `max_tokens` 1, `decode_holds` from the head's session
  offer, `pieces`). `plugin_session::note_holder` and `note_prompt_start` record,
  when a turn ends, the head that holds a conversation and the start of the
  prompt it read; `generation.rs::auto_head` sends a conversation, or a new one
  that begins with most of that prompt, back there. Only a `shared` head (the role the deploy gives the
  machines left at `both` when the whole model is on several) gets a prefill machine. When it saved the session, the head's turn
  carries `session_source` and `plugin_session::pull_from` fetches that session
  (waiting for its save) before the turn reads its prompt. `engine.load` gets
  `role` for a non-default role.
  `deploy_block` is the deployment check: a whole model on one machine (on
  several when the service declares `inference_role`), or
  slices on distinct machines for a service that publishes `stages`, each
  running the plugin's `inference` service. A slice's `engine.load` also gets
  the `link` from `plugin_stage_link::prepare`. A stage whose file is the whole
  model (`file_is_whole_model`) also gets `range: {start_layer, end_layer}`;
  `runs_ranges` says whether a service's `stage_ranges` covers an
  architecture, and `deploy_block` takes `ranges` for a whole model split in
  ranges. See
  `docs/subsystems/inference.md`, "Plugin inference services".
- `engine_telemetry.rs` - what the POM measures about any plugin engine. A
  plugin reports only its basic data in the `done` event (`EngineReport::from_done`:
  token counts, `prefill_ms`/`decode_ms`, the drafts, the finish; the llama.cpp
  `timings` object is read in place of a field the plugin does not report).
  Everything else is the POM's: `build_trace` (the hops of the plan, the phases,
  the rounds and drafts), `apply_stage_links` (each hop's time on the frames, the
  wire, the bytes and one transport-summary entry per channel), `merge_reported_links`
  (the links of the machines below the head, from its `done`), and `record_perf`/`perf_for`
  (the speed the `/health` publishes as `perf`). No engine-specific code belongs here.
- `plugin_stage_link.rs` - the stage link of a plugin engine split across
  machines: `prepare` (a later stage finds the earlier stage's machine, takes
  its link secret and opens a loopback relay that posts each length-prefixed
  frame of the engine to it, up to `prefill_max_inflight_chunks` at once,
  numbered, answers written back in order), `link_secret_route` and
  `link_frame_seq_route` (the earlier machine's side, which keeps one loopback
  connection per relay connection to the port of `engine.link`, writes the
  frames to it in number order without waiting for the answers, and a reader
  task per connection hands each answer to its frame). A tensor frame (kind `2`) crosses between the
  machines in the deployment's `hidden_state_dtype` through `wire` and reaches
  the engine as `f32` (`to_network`, `from_network`). Both routes take only the
  signed live peer that serves the next slice of the same deployment.
  Because the link encodes with the node's own `wire`, a plugin engine's
  machine announces the lossy formats (`plugin_engine::STAGE_LINK_DTYPES`, in
  `/health` and in `Network::plugin_engine_capabilities`), so a sliced plugin
  deployment can choose f8 or int4.
  `watch_secret` checks the earlier stage's secret every 10 s. If the secret
  changed (the earlier engine restarted), `Relay::stale` makes
  `plugin_engine::run` end the generation, so the restart links with the new
  secret.
  A frame the earlier stage answers with 410 (it reset the link: it restarted,
  or took a newer connection) also restarts this stage when the engine has not
  opened a newer connection within `RETRY` (`StaleSignal`, `ConnSlot`):
  before, the engine lost the link for good and answered "the earlier stage is
  not connected" to every request while its `/health` stayed ready.
- `relay.rs` - `BatchCoordinator` schedules the flush immediately and still groups
  concurrent calls to the same next hop in the same executor cycle; it sends a single
  frame and distributes the answers by `item_id`. QUIC also publishes, in microseconds,
  the time waited in `send_data` and `finish`; in a batch, the shared time is aggregated
  once. There is no longer a fixed timer in the relay; `bubble_hop_timings` keeps the
  per-hop telemetry.
- `runtime.rs` - `NodeConfig`/`NodeRuntime`: applies the slice, injects the global
  cache types from `pom.json` and keeps the operational `deployment_name` separate from
  `model_label`, brings up and supervises the embedded Rust server
  (`infer_server::run`), keeps a weight cache between compatible generations and keeps the
  annotation of the slice in service (Pending -> Serving after a stable `/health`
  confirmation). `stop` clears the cache and the journal; a change of slice or of loading
  parameters also invalidates it.
  The runtime keeps the `pom.db` for the gateway's decisions; the inference server
  authenticates internal hops through the shared `connection_key` on private networks and
  through TLS on public networks. It does not consult client API keys.
  The telemetry stays the source of truth about the engine that actually ran, and it
  leaves a divergence visible. After two
  consecutive `/health` answers with
  `model_loaded`,
  `spawn_health_watch` also publishes on the `Network` the effective `RuntimeCapacity`
  (`n_ctx`, `max_new_tokens`); apply, error, stop and thermal unload remove that value
  until the next runtime is ready.
  The `/health` endpoint also returns `model_label`, `deploy_id`,
  `start_layer`, `end_layer` and `total_layers`. The gateway uses those fields as the most
  recent executable photograph during the admission, correcting a delayed P2P coverage
  before calling the planner.
  With `NodeConfig.inference_service` set, the supervisor runs
  `plugin_engine::run` instead of `infer_server::run`; the health watcher, the
  journal and the restart budget are the same.
  The tests of `runtime.rs` use `Network::testkit`, with the socket and the UDP
  destination pinned to loopback, so readiness announcements never reach the LAN.
- `spec_lab.rs` (614, `#![cfg(test)]`) - the speculation bench: a real
  `generate_stream_with_stops` against fake hops that decide with the production
  `should_draft`/`find_draft`/`longest_match`. The fixture does not announce batching,
  because the fake endpoint serves unitary frames. The scenarios also check that the
  gateway's real answer carries aggregated `GenerationTimings`, including at the limit of
  one token; the policy of the effective `kv_remove` and the state of the remote stream are
  covered in the tests of `infer_server`/`prefill`. The budget fixture turns the cycle
  detector off so its periodic sequence does not end the generation before the ceiling and
  the comparison keeps isolating `n_batch` from the total budget.
- `tp.rs` (new) - registration of the tensor parallelism callback in llama.cpp
  (path C): `register_tp_reduce`/`unregister_tp_reduce` and the `extern "C"` that reads the
  partial (f32), contributes locally when it is the primary or POSTs to the primary's
  `/v1/tp/reduce` when it is a worker. The endpoint sums in the `infer_server`'s
  `ReduceBarrier` and returns the sum in the buffer. The callback runs on a llama.cpp
  `spawn_blocking` thread and uses the Tokio `Handle` captured at the group's registration,
  never `Handle::current()` on the native thread. The client is **pinned to the primary's
  `node_id`** (`TpIdentity.rank0_node_id`): a default `reqwest::Client` refuses the
  cluster's self-signed certificate and every POST died in 13 ms. The fork inserts the
  reduce op (`GGML_OP_CUSTOM`) after wo/ffn_down and calls that callback during the decode
  - confirmed in the field:
  `graph splits = 66` (2 per layer) and the log of the first reduce.

  A **replica has no opinion** (`tp::is_replica`): it contributes its half in each reduce
  and nothing else. Whoever samples the token and decides the end of the generation is rank
  0. EOS only closes the `Utf8Assembler` for the answer; the physical session stays alive
  until the gateway decides between `/v1/session/rewind` and `/v1/session/release`. That
  keeps a replica from seeing a session removed by rank 0 before the rewind or a step
  already accepted.

  `register_tp_reduce`/`unregister_tp_reduce` **turn the node on and off** in the group,
  and turning it off cleans everything: state, open decode, the replica mark and the pending
  failure. `TP_STATE` was an `OnceLock`, which cannot be cleared, so a process that had once
  served TP declared itself TP forever - deploying a whole model on top of an inherited group
  took over the state, warned "reduced 1 times, expected 64", and the check of zero reduces
  even brought the request down (18/08). Swapping the group also stopped requiring a restart.

  The partial travels as **raw f32 in little-endian**, with `barrier_id` and `rank` in the
  header (`x-pom-tp-barrier`, `x-pom-tp-rank`), and the route has the same body limit as the
  frame routes. It used to be JSON: about 12 bytes per 4-byte float, which gave about 5 MB
  per reduce on a 100-token prefill - 64 times per decode - and blew past axum's default
  2 MiB limit with `413 Payload Too Large` on the second request (18/08). Raw is 4x smaller
  and goes through no parse.

  `relay::send_bytes_or_tunnel` tries the pinned direct HTTPS candidates first. HTTP/3 comes
  before HTTP/HTTPS when QUIC is on; the tunnel only enters after every direct address fails
  on transport. With QUIC off, the member still has to announce `endpoint_id` and
  `transport/iroh-tunnel-v1` to enable the last reserve. That order matters on the TP
  reduce: the call sits inside the barrier and an unavailable tunnel must not delay a
  reachable HTTPS address. `broadcast_to` keeps the base URL and the route separate, because
  the tunnel forwards the route to the worker's loopback. The plan also carries the peer's
  endpoint addresses to the `TunnelOutbound` as dial hints, so the endpoint can dial even when
  the announcement is not in the local table. The `TunnelRequest` carries the barrier, rank
  and authorization headers. The internal authorization uses the shared `connection_key` and
  the internal node marker. On public networks, HTTPS and QUIC may validate through the
  secure transport with the marker, but the HTTP fallback also carries the shared bearer;
  without the matching proof the call is refused instead of falling to a client API key. The
  whole direct HTTP attempt, including authorization, headers and body, also respects the
  frame's single deadline (`relay::frame_time_left`).

  The `TpDecodeTelemetry` snapshot, attached to `node_timings.tp`, separates the expected
  and observed reduces, the total wait, p50/p95, the request/response bytes, local calls,
  broadcasts and direct calls, through the tunnel or by fallback.
  It measures the wait of the synchronous barrier, not a pure packet latency. On a 32-layer
  model, the current architecture has two collective points per layer, that is 64 synchronous
  waits per `llama_decode` of one token; that is TP and must not be confused with the
  pipeline's hidden-state frames.

  The replica's session lifecycle is commanded by rank 0:
  `/v1/session/release` is forwarded to the members (`replicate_session_call`),
  because the gateway only talks to rank 0 and a replica does not end a session on its own.
  Without the forwarding the worker's session held the only slot and the **second** request
  died with "seq pool exhausted" (18/08).

  `/v1/session/rewind` follows the same replica rule, but it preserves the session: it
  removes only the generated suffix and updates `n_past` on every rank. The gateway only
  publishes the prefix in the cache after confirming each hop's answer. The normal end of the
  generation does not call `InferState::finish`; that removal is reserved for an explicit
  release and for the collection of idle sessions.

  The `handle_prefill` and `handle_step` handlers also hold `tp_batch_gate` on the path of a
  frame with a single item. The gateway sends that case through the `BatchCoordinator`, so
  the receiver has to keep `item_id`/`batch_id` in the answer's meta and keep another request
  from changing the global decode while the collective still waits for all ranks. The internal
  children of a batch skip the lock through `BATCH_CHILD_HEADER`.

  `/v1/session/rollback` is still not forwarded to TP replicas (an open item: with
  TP speculation on, the ranks' KV caches would drift apart). It is only used by speculative decoding, today off in the TP
  configurations.

  A collective failure **cannot be silent**: it does not degrade the answer, it makes it
  false. `note_failure`/`take_failure` carry the reason down to the decode, which brings the
  request down; and `decode_tokens` still compares `reduce_count()` before and
  after, because "the callback was never called" had no symptom at all beyond the
  meaningless text (18/08).
- `wire.rs` - the only place that knows the dtype/encoding of the tensor in transit and
  the initial `frame/v1-batch` contract with validated offsets.
  `WireDtype` keeps f16 as the default, offers lossless f32 as a deployment option and
  implements signed f8 FP8 E4M3FN and i4g64/i4g128 with an f16 scale per group; decode
  always returns f32. The profiled variants measure the f32/f16 conversion separately from
  the frame's total. The per-phase telemetry also separates the conversion observed on the
  peer and on the client; f32 uses four bytes per value and asks for no capability.
  `prefill/` and `relay.rs` propagate the chosen dtype and record bytes sent, received and
  the effective format per hop.

## Key functions

`infer_server/`:
- `InferenceServer::new` + rotas - monta o router axum sobre o `LlmEngine` e o
  estado de sessões (sessions, seq_pool, max_sessions, sampling).
- `InferenceServer::new` + routes - assembles the axum router over the `LlmEngine` and
  the sessions state (sessions, seq_pool, max_sessions, sampling).
- `Session` - **two KV sequences per session**: `prompt_seq`, where the prefill writes up
  to the measured stable boundary, and `gen_seq`, opened by `open_generation` before the
  first chunk starting from that boundary, as a copy of the first one. The prompt's volatile
  tail and the generation live in the copy.
  `prompt_cells` counts only cells written in `prompt_seq`; chunks on the copy do not change
  that counter. `seq_id()` returns the sequence of the current step. That is what allows
  retaining the prompt across turns without ever removing positions from the middle of the
  KV; see `llm::kv_seq_budget` for the pair of sequences and what it costs.
- `retain_prompt` - end of turn: it erases the whole generation sequence (a total removal,
  which every engine accepts) and returns the session to `prompt_len`. EOS calls
  this, **not** `finish`; destroying the session at EOS was what made the gateway's rewind
  arrive at an "unknown session". `finish` stays the definitive release (explicit release,
  cancellation, error).
  `retain_prompt` erases the generation sequence and retains `prompt_len` on the prompt
  sequence. If there are extra cells really in `prompt_seq`, it also tries a removal of the
  **suffix** (not of the middle). `prompt_cells` does not include the prefill sent to
  `gen_seq`: adding it as if it had been written in `prompt_seq` left the logical position
  incompatible with the physical cells at retention and blocked the next reuse.
  `Session::apply_retention` only adjusts
  `prompt_cells`/`prompt_tokens` together with `n_past` if the prompt was already at the
  stable boundary or if the pruning was accepted; a recurrent/hybrid engine that refuses a
  needed pruning keeps the session retained with the whole prompt and loses that turn's
  reuse, without leaving incoherent counts. The
  `prefix peer eligibility`, `prefix peer comparison` and `prefix peer retention` logs
  record the reason of the decision without including prompt text.
- `plan_prefix_reuse` / `expected_prefill_pos` - the reuse rule and the position of the
  piece, pure and tested in `prefix_rule_tests`. The rule is append-only:
  outside it the session starts over, which costs the turn's reuse and never the request.
- `evict_retained` - dumps the least used retained session. **It is no longer called by
  `alloc_seq`**: swapping the hot KV of a live conversation for the prefill that just
  arrived was the production defect of 2026-09-08, in which a 35-character request dumped
  62 thousand tokens and the conversation's first word went from 2.7 s to 41.1 s. Today only
  the scheduler calls it, and only after `RETAINED_RECLAIM_GRACE` with a new session's
  prefill waiting - the hot conversation gets its chance to come back, and no accepted
  request is left hanging until the `reap_idle`.
- `new_session_slots(free_slots, retained)` (`infer_server/core.rs`) - the admission of a
  new session counts **only free sequences**. A retained session is hot cache, not capacity:
  counting it as a slot admitted a prefill that, on the worker, could only enter by
  destroying another conversation's prefix.
  `SchedulerQueue::oldest_new_session_wait` is the clock of that shortage.
- `check_prompt_fits_session`/`check_decode_fits_session`
  (`infer_server/server.rs`) - one session's window
  (`context_per_session = n_ctx_total / max_sessions`) is enforced on the peer:
  the tokenized prompt must leave room for one token, and decode refuses with
  the session at the last position. With `kv_unified` llama.cpp would let it
  go past it using another session's cells. The gateway stops earlier, with
  `length`.
- `handle_health` (`infer_server/server.rs`) - never waits for the engine: it
  tries the `InferState` lock without blocking; when busy, it returns the last
  snapshot (`HealthSnapshot`, `busy: true`, `snapshot_age_ms`) with queue and
  routes recomputed; when free, it builds the body (`health_body`) and stores
  the snapshot again. With no snapshot yet, it waits for the lock in
  `spawn_blocking`. The gateway's readiness probe and the route plan's probe
  depend on this on a peer with a long prefill.
- `handle_prefill`/`handle_step` - authentication, frame decoding and dispatch to the local
  scheduler. The worker uses `prefill_batch_blocking` or
  `step_batch_blocking` when the entries are compatible and keeps the individual path for
  speculation and compatibility.
- `BatchRoute::group` - groups descriptors only when model, deployment, phase, configuration
  and next hop coincide; `RouteAffinity` in
  `pipeline.rs` keeps the node IDs of the replicas chosen during the session.
- `spawn_scheduler`/`run_scheduler` - a single worker per llama.cpp context,
  with backpressure before allocating a session and a `429 Retry-After` answer when the
  queue reached its limit. With `batch_window_ms=0`, the first item goes to the worker with no
  timed wait; a positive value keeps the collection window. The `/health` exposes the limits
  and the last batch counters.
- direct reply block (in both handlers, when `direct_reply_to(&meta)`
  exists) - accumulates `hop_timings` on the way (`direct::accumulate` +
  `direct::hop_entry`); tail (`next_hops` empty): spawns `direct::deliver_result`
  to the `reply_to` and returns `direct_ack()`; the head includes the prefill progression in
  `prompt_progress` in the ACK, so the gateway dispatches the next chunk with an absolute
  `next_pos` during the overlap; middle: spawns
  `forward_direct` for the next hop; middle without local residual:
  `deliver_failure` with "intermediate hop produced no hidden state".
- `forward_direct` - asynchronous forwarding in its own task (whoever called was already
  accepted and waits for nothing more); forwards `reply_to` intact; rebuilds `scale=1.0`,
  `is_last` and the remaining `next_hops`. If the forwarding failed, it delivers
  `direct::deliver_failure` naming the node that actually died.
- `direct_ack`/`direct_ack_with_progress` - ACK wire-encoded with
  `{"accepted": true}`; on the prefill, the head attaches `prompt_progress` with the
  authoritative `more_chunks`/`start_pos`/`next_pos`. The gateway reads that nested field
  before launching the next chunk; it does not derive a position from the arrival order.
- `direct_reply_to` - the meta's `reply_to` field; absent in the hub and the ring, it is the
  only thing that turns on the new path.
- `prompt_progress_for_forward`/`apply_prompt_progress` - the direct counterpart of
  `overlay_head_authoritative_fields` (now `pub(crate)`): with no return trip, the head sends
  `more_chunks`/`n_prompt_tokens` **ahead** and whoever delivers copies it into the result -
  otherwise every prompt of more than one piece truncates at the first.
- `direct_lab::direct_prefill_preserves_the_absolute_position_with_and_without_reuse` -
  an end-to-end scenario with two chunks, a nested progress ACK and a remote head; the fake
  hops refuse a continuation without a position and check the absolute values both on a cold
  session and with a reused prefix.
- relay branch (`should_relay`) - in the ring, when `next_hops` is not empty and there is
  local hidden state, it forwards to the tail instead of returning to the caller; it patches
  its own `processing_ms`/`node_timings` before encoding the return.
- `owns_tokens`/`is_final_chunk_for_forward` - only the head has authoritative
  `more_chunks`/`n_prompt_tokens`; the ring's bubbled answer always brings false/absent,
  otherwise a multi-chunk prompt truncates in silence.
- `handle_rollback` - `RollbackRequest{accept_count, batch_width}`: discards the unconfirmed
  KV of the speculation; an unknown session is a no-op (not an error); the gateway fans out to
  every hop.
- `handle_slot` + `InferState::{save_slot,restore_slot,check_slot_compatible}` /
  `erase_slot`/`count_slot_entries` - a KV snapshot on disk
  (`slot_save_path`). The health counts sidecars from the directory's structure,
  outside the engine's lock, and only opens the JSON when it needs to save or restore.
  `save_slot` writes **`prompt_seq` truncated at `prompt_len`**, never `gen_seq`,
  plus the `<name>.json` sidecar (the identity of the model and of the slice).
  `restore_slot` checks the sidecar before touching the engine, `alloc_seq` a new session
  `retained` at the saved boundary. `slot_dir` comes from
  `NodeConfig.slot_save_path` (default `~/.pom/kv-slots`, from
  `Config::slot_save_path_effective`); with it empty the route answers 400. `replicate_slot_call`
  forwards the action to the TP workers. Capability `inference/slot-persist-v1`.
  See `inference.md` §3.1.1.
- `infer_server/ram_cache.rs` - the host-memory KV cache (`--cache-ram`):
  `KvRamCache` parks the serialized `prompt_seq` of a retained session that
  loses its sequence and `InferState::park_session`/`unpark_session`
  (`server.rs`) move it out of and back into the context. `evict_retained` and
  `reap_idle` park before closing; `finish` and a reset forget the entry; the
  prefill unparks on chunk 0 when `reuse_prefix` is asked. Half of the host RAM
  always stays available to the user and a host under 12 GiB parks nothing
  (`user_ram_reserve`, `HostMemory`). Off for tensor-parallel ranks.
  `POST /v1/session/cache/clear` empties it. See `inference.md` §3.1.2.
- `reap_idle`/`SESSION_IDLE_TIMEOUT_SECS=900` - an idle session is released;
  a process restart also removes the in-memory state. If one of those events happens in
  the middle of a prefill, the peer returns an unknown session and the gateway starts the
  full prefill over once, never only the chunk that lost the KV;
  `INFERENCE_FRAME_BODY_LIMIT=64 MiB` cuts an impossible frame at the body.
- `tcp_nodelay(true)` on `axum::serve` - see the twin note in
  `gateway::start_gateway_with_ready`: without it, the direct reply ack (a small write)
  stays stuck in Nagle.
- `/health` announces `relay/v2-direct` and, when `slot_dir` is configured,
  `inference/slot-persist-v1` (keep it in sync with `network.rs`
  `self_announce` and the gateway's local entry); it exposes `kv_can_shift`,
  `profile_sync_enabled` (the effective value of `POM_PROFILE_SYNC`), the
  `selected_path`, `slot_persist_enabled`/`slot_count` and also the scheduler's snapshot
  (`queue_depth`, the limits, the size and tokens of the last batch). The observed engine
  comes out through `node_timings.executed_engine` and `executed_engines` on every hop of
  the prefill/decode and is aggregated in the `RequestTrace` with no inference from the
  screen.
- `NodeTimings` also publishes, per `step` call, the MTP counters
  `mtp_draft_tokens_proposed`, `mtp_draft_tokens_accepted` and
  `mtp_draft_tokens_rejected`. `proposed` counts the draft submitted to the target's
  verification, not a `next_draft` that went unused at the budget cut.
  `RequestTrace` sums those samples per request and serializes
  `mtp_acceptance_rate` as accepted/proposed, with `null` when there was no proposal. The
  increments stay outside the per-token loop and do not change the speculation policy.
- `should_draft` (and `find_draft`/`longest_match`) - the head decides whether it drafts
  based on the invitation and on `mtp_ready` or on a pre-built draft.
  Refusing is a legitimate answer, not an error: the head omits `draft` in the frame and the
  tail answers in the old one-token shape (see
  `step_blocking::incoming_draft` and the section of two answer shapes in
  `prefill/`). The tail demands `mtp_ready` to produce the next draft;
  `kv_can_shift` is only a diagnostic of the `--ctx-shift` K-shift. The real rollback stays
  checked by `kv_remove` and fails closed if the engine refuses the removal.

`llm.rs`:
- `LlmEngine::load_with_cache` - loading with `EngineConfig` (total n_ctx, n_batch,
  n_gpu_layers, offload_kqv, fit_memory, its configurable margin per GPU,
  and max_sessions). The runtime turns the per-session context into
  `n_ctx * max_sessions` before creating the engine. With
  `fit_memory=true` and the default of GPU layers (`99`), it calls llama.cpp's
  `llama_rs_fit_params` bridge before loading the weights. Before that, it queries through
  the bridge the free and the total of the backend's devices; the free already excludes
  existing processes and the default margin of 256 MiB covers only late allocations and an
  occupation that appears later. The plan keeps `tensor_split` and `tensor_buft_overrides`
  when there is more than one local GPU. If creating the context fails with the KV on the
  GPU, it tries again with `offload_kqv=false`, keeping the same weight layers on the GPU and
  leaving the KV cache in RAM. `/health` shows whether the fit was applied, the effective
  layers and `n_ubatch_effective` - and, in `placement` and
  `context_memory`, where the layers and the KV actually landed. The public `n_batch`
  stays the limit of the prompt piece, but `fit_memory=true` caps the internal microbatch at
  512 tokens to reduce the CUDA temporaries that are only reserved in the first real graph;
  without the fit, the microbatch follows `n_batch`.
  This effective microbatch governs the compute buffer of both the main context and the MTP
  context, so 512 costs considerably more VRAM than 128.
  `llm::apply_context_batch_params` is the single source of that policy and is also used by
  the MTP context, which keeps the public `n_batch` but does not inherit a 4096 microbatch by
  mistake.
  `LlmEngine::pool` (N contexts over a single model, `pool_tests` covers how the total is
  split between single-session contexts) still **has no caller**: the server loads one context
  with `load`, and the `#[allow(dead_code)]` on the `pool` is the mark of that. `LlmEngine::load_with_cache` uses the runtime's `ModelCache` to create only a new
  context when the identity of the slice and of the weight loading did not change; KV,
  context, session, batch and sampling parameters do not force a re-read of the GGUF.
- `LlmEngine::placement` / `LlmEngine::context_memory` - where the layers and the
  KV cache **actually** landed, from llama.cpp, through
  `llama_rs_model_placement_json` and `llama_rs_context_memory_json`
  (`vendor/llama-cpp-sys-2/wrapper_common.cpp`). They return an `Option`: a bridge failure is
  `None`, and the `/health` publishes `null` instead of a map derived from
  `n_gpu_layers` - which would always lie whenever a buffer type override sent part of a
  layer to the CPU. `json_from_wrapper` is who calls and who frees the string
  (`llama_rs_string_free`, never `free` nor `CString::from_raw`).
- `effective_n_threads` / `apply_thread_params` (`llama-engine`, `llm/config.rs`)
  - the CPU threads of the main and MTP contexts. Without `n_threads` configured, it uses
  every thread available to the process (`available_parallelism`, which respects the
  container's CPU affinity and quota) instead of llama.cpp's fixed and small number; it
  applies to generation and to batch.
- `LlmEngine::flash_attn` / `LlmEngine::load_params` - with which llama.cpp parameters this
  context stands, for the `/health` and the Details screen.
  Nothing here is an echo of the `EngineConfig`: the values come from the live context
  (`llama_n_ctx`, `llama_n_batch`, `llama_n_ubatch`, `llama_n_seq_max`,
  `llama_n_threads`), because the KV fallback in RAM raises a context with another
  configuration and the operator needs to see what runs. `kv_unified`, `embeddings`
  and `n_rs_seq` have no getter, so they come from a `ContextParamsSnapshot` taken on the
  line before `llama_new_context_with_model`. Flash Attention is the only `Option` field:
  llama.cpp's default is `AUTO` and the decision only exists in `get_cparams()`, read through
  `llama_rs_context_flash_attn`; a bridge failure is `None` and becomes `null` on the
  `/health`, never `false`.
- residual capture - `ggml_backend_sched_eval_callback` on `l_out-<N>` before the
  `output_norm`, with a `CaptureSlot` per context through `cb_eval_user_data`; the
  DeepSeek4 calls the same boundary from `l_last-<N>`, and the callback accepts both names. A
  head that comes afterwards may run, but it does not alter the residual already copied. When
  `llama_model_n_embd_out` diverges from `llama_model_n_embd`,
  `cparams.embeddings` stays off to keep llama.cpp's generic copy from asking for more bytes
  than the real tensor; the callback stays the output POM uses. The token axis is located by
  the known count of the current microbatch and by the physical shape, never by the product
  until
  `n_embd_inp`; that way, stream axes or other earlier features are
  preserved. After the axis, dimensions greater than 1 are refused and
  dimensions of 1 are ignored; in an ambiguous case, the smallest physically contiguous
  axis wins deterministically. `accumulate_capture` joins one firing per
  **microbatch**: llama.cpp splits the batch into pieces of `n_ubatch` and calls the
  capture once per piece, so replacing instead of accumulating kept only the last one. Every
  decode path calls `reset_capture_slot` beforehand, otherwise the leftover of one request
  would enter concatenated into the next. The order is the one of
  `split_simple`, which `kv_unified=true` keeps for the transport of the residual.
  In multimodal prefill, one callback may bring only part of the physical cells (for
  example, the text before the cells expanded by the image); the capture infers that
  callback's physical axis and accumulates up to the expected total, without using `n_past`
  as a row count.
- `profile_backend_sync_enabled` - the profile of the synchronous backend,
  `POM_PROFILE_SYNC`
  (on by default; "0"/"false" turns it off), published explicitly per peer on
  `/health`. `NodeTimings` keeps the counters in microseconds and serializes the
  diagnostic `*_ms` fields as fractional milliseconds, so a sub-millisecond measurement does
  not become zero on the wire. `backend_call_ms` measures the llama.cpp evaluation call,
  separate from the queue and the wait for the mutex;
  `backend_sync_ms` measures the explicit barrier and may include GPU work already queued.
  Neither is pure kernel time nor a source of truth about
  `POM_PROFILE_SYNC`; the Chat reads the flag published by `/health`.
- `unsafe impl Send/Sync` behind a std Mutex (llama.cpp is not thread-safe);
  `Drop` releases only the ctx - the weights belong to the `LoadedModel`.
- `SamplingConfig` - at each step, the sampler applies the repetition penalty, top-k by
  count, a softmax with positive temperature, the top-p nucleus over the normalized
  probabilities and a multinomial draw. A non-positive temperature returns argmax.
  `repeat_last_n=64` stays the default: the penalty is per token and does not detect n-gram
  cycles, so widening the window would not guarantee blocking the observed cycle and would
  change the policy.
- `BatchLayout` - turns the physical order of the `llama_batch` rows into ranges per
  `item_id`; the global position of logits is never inferred from the session index. Mixing
  token and hidden state stays in distinct lanes.

`prefill/`:
- `run_with_options`/`step` - a three-level dispatch: `run_direct`/`step_direct` when
  `plan.supports_direct_reply()`, otherwise `run_ring`/`step_ring` when
  `supports_relay_chain()`, otherwise the hub pipeline. `run_with_options` receives
  `max_inflight_chunks` and a `resume: Option<PrefillResumable>` for a resumption the caller
  has proven safe; only an ambiguous delivery or a divergent remote state demands a cold
  rebuild.
- hub and `run_direct` - a pipeline in order, not locked pairs: `head_pump`
  (a serial dispatch to the head, reading `next_pos` from the ACK) and a `drain` with
  `FuturesOrdered` that carries each hidden state through the remaining hops, up to `W` in
  flight. `run_direct` dispatches each chunk **once** to the head with
  `reply_to`; the tail delivers the result straight to the gateway
  (`await_result_with_ack` on the ticket). `is_last` is always `false` at the head (the
  direct mode demands >= 2 hops).
- `step_direct` - the decode twin: one outgoing call (to the head), one wait on the ticket.
  It is the difference between `2N` traversals (ring) and `N+1` (direct), and that is why the
  cost per token stops depending on where layer 0 lives.
- `step_stream`/`RemoteDecodeStream` - a single control frame for the remote head; the peer
  keeps decoding and delivers bounded callbacks with at most eight ready items per batch.
  Plans with more than one hop keep the other peers inside the head's relay; the gateway does
  not pay a call per token.
  When the plan announces sliced MTP, the callback carries `next_draft` together with
  `speculated`, `speculation_active` and `speculation_disabled_reason`, and the head installs
  the draft in the next local round. Each hop rolls back its own suffix before bubbling the
  answer, with no extra call.
- `direct_failure` - a result that arrives with `error` in place of the token names the hop
  that died and marks **that one** (not the head) with `routes.mark_failure`.
- `record_direct_hops` - folds into the `RequestTrace` the `hop_timings` and the per-traversal
  evidence received in direct mode; it does not invent an attribution for the first hop when
  the measurement does not exist.
- `step_speculative` - stays on the ring/hub for the old path; the new remote transport carries
  the speculation through the same `handle_step` of the peer that owns the session, preserving
  sampling, content and order.
- `speculative_step_from_meta` - interprets the tail's answer, and this is the point that was
  killing the generation in silence: the refusal to draft is a **valid** answer. A speculative
  round has two legitimate shapes -
  `{done, accepted, accept_count, batch_width}` (the head drafted) and the old one-token shape
  `{done, token_id, text}` (the head refused: `should_draft` false). Whoever reads the answer
  has to accept both; treating the old shape as a fatal error brought the generation down with
  one token and `partial: true` on every request (2026-08-15, Qwen3.8-27B). Without `draft`,
  `speculated` is
  `false` and `batch_width` is 1. Stream callbacks may declare
  `speculation_active=false` and a named reason; without the new fields, the parser keeps the
  old fallback.
- `Speculate`/`DraftSource` (`infer_server/`) - the invitation to draft that travels in the
  frame's `speculate` field: `{k, m, source, draft?}`. On the legacy path, `draft` is the
  pre-built draft the gateway injects in sliced mode; on the remote stream the head installs the
  draft received from the tail in its own `step_meta` before the next round. `source` is what
  separates the **two** mechanisms, and they are not the same thing with different numbers. A
  frame without `source` is an old gateway and `Lookup` applies.
  `Speculate::to_meta`/`from_meta` and `DraftSource::wire` are the serialization pair.
- `should_draft(speculate, mtp_ready)` - `Mtp` demands a ready local head or a pre-built
  draft; `Lookup` only demands the invitation. Asking MTP of an engine without the `nextn` head
  is a refusal, never a silent swap for lookup. The tail of a pipeline only stops returning
  `next_draft` when `mtp_ready=false` or the draft produced is empty. `kv_can_shift` is exposed
  as a diagnostic, not as a rollback requirement; the effective removal through `kv_remove`
  stays fail closed. The other refusals use
  `tail_without_nextn`, `model_without_mtp` or
  `peer_without_sliced_mtp_wire` according to the evidence available.
- `DEFAULT_CHUNK_TOKENS=512` - about 8 MiB f16 per hop per piece
  (measured: 565 KB for 69 tokens, exactly `hidden_dim 4096 × 2`).
- `RequestTrace`/`PhaseTrace`/`HopTrace` - timings per phase per hop; `round_trip_ms`
  keeps the observed round trip of each call for the RTT sum of the execution,
  while `node_lock_wait_ms` separates a "slow node" from a "busy node". The fields
  derived from `NodeTimings` keep fractions of a millisecond. This is gateway/SSE telemetry and
  it does not change the frame between peers. Each `PhaseTrace` aggregates the traversals in
  `transport_summary`, per transport, with calls, bytes, minimum/median/p95/maximum RTT,
  fallback per destination and transport swaps. The internal RTT and decode samples have a fixed
  ceiling; `transport_trace_complete` reports missing receipts. `hidden_state_dtype` is only
  observed when hidden-state bytes crossed the hop; on a whole-model plan of a single node, the
  absence of bytes makes the wire precision not applicable and the field stays absent. The remote
  transport records `remote-stream` once per batch callback, with bytes received and aggregated
  processing; the callback is not a new RTT measurement, which is why `round_trip_ms` stays zero
  on that entry.
  `RequestTrace` also carries the per-request MTP counters and the derived rate, while the
  `node_timings` fields keep the values of each hop/step for the audit.

`relay.rs`:
- `send_frame_planned` - the entry of every remote frame (`send_frame_or_tunnel`):
  with a route pinned in the plan (`route_plan`, the `Inference` plan) the step is reduced to
  it through `pin_step` and `hop_transport` releases **only** the measured transport
  (`HopTransport`: the persistent channel and TCP for HTTP, only QUIC for QUIC, only the tunnel
  for the tunnel). A failure before delivery repeats the route once, then moves to the next
  measured one on the same frame; an exhausted list asks for a reanalysis and the frame still
  walks down the whole ladder. A failure after delivery never changes route.
  Without a pinned route, `HopTransport::unplanned` is the usual ladder.
- `BatchFailure` - a batch's failure reaches each call keeping whether it happened
  before the delivery (`FrameNotDelivered`) and the HTTP refusal (`HttpRefusal`); before,
  it became text and the route swap did not recognize the failure.
- `send_frame` - HTTP to the next hop with the wire-encoded body;
  `SentFrame{round_trip_ms, bytes_sent/received, encode/headers/body/decode ms}`
  also identifies the `transport` (`local`, `p2p-tunnel`, `https`,
`http`, `https`, `quic` or `remote-stream`). The internal detail
`https-link` is normalized to `http`/`https` in the telemetry. The
`transport_fallback` field reports when the call walked down the ladder after a
transport failure. A frame that goes straight to the tunnel without trying
the direct path - route pinned to the tunnel (`tunnel_first`) or a peer marked
by `prefer_tunnel` - is **not** a fallback: only what had already failed before
the ladder counts (the persistent channel, for example). `DIRECT_SKIPPED` is the
reason that only shows up in the error of a tunnel that also failed.
- `send_batch_body` is the single point every remote batch passes through,
  including the `remote_stream` activations that cannot be grouped with a
  different session. That is where the transport selection lives: with QUIC on,
  it tries QUIC and falls to HTTP/HTTPS; with QUIC off, it tries the persistent
  channel (`link::send_batch`, when the hop announces `transport/link-v1`), then
  HTTP/HTTPS by request and only then the p2p tunnel. Each step is only used
  after the previous one failed on **transport**; an HTTP refusal from the peer is
  final, except 404/405 on QUIC, which mean a peer without the HTTP/3 route. The
  step used becomes `transport`, and having walked down the ladder becomes
  `transport_fallback`. With the direct path skipped on purpose, a tunnel that
  delivers does not mark a fallback and there is no empty-reason "did not
  receive the batch".
- The per-request transport tries QUIC before the TCP client when the
  preference is on; a QUIC failure falls to the pinned HTTP client and, only if
  every direct address fails, to the p2p tunnel (QUIC under the peer key). The QUIC destinations
  come from the `host:port` the peer announced, not from the `inference_url`, and
  use the dedicated UDP port published in the `Announce`. With QUIC off, the
  HTTP/HTTPS client is still tried before the tunnel;
  `TunnelRequest::get("/health")` is reserved for the topology's RTT measurement.
  The `SentFrame`'s `round_trip_ms` includes remote processing and does not feed
  the directional routes table.
- `bubble_hop_timings`/`as_hop_timing_entry` - inserts its own hop's entry in
  front (head->tail order) and folds the callee's `processing_ms`+`node_timings`
  with the caller's round trip.
- `extract_relay_failure` - used by `forward_direct` to name the hop that failed in
  the delivery of the direct error. `is_unknown_session_error` walks the whole
  `anyhow` chain, because `PartialPrefillTrace` wraps the refusal on the direct
  path; looking only at the outer sentence lost the marker and blocked the cold
  rebuild.
- `RelayFailure{failed_node_id, kind, message}`/`HttpRefusal`/`SendIssue` - the
  gateway answers 502 attributing the hop that failed. `CHUNK_ATTEMPTS=3` repeats
  only failures proven to be before the delivery, with an exponential backoff of
  100 ms and 200 ms. After accepted chunks, the prefill resumes at the absolute
  position when it is safe; an ambiguous delivery or a divergent session demands a
  cold rebuild with an explicit reason.

`direct.rs`:
- `deliver_pinned` - the return to the gateway uses the `Gateway` plan's pinned route
  (`route_plan`) before the `reply_to` ladder: HTTP on the measured address, QUIC on
  the measured endpoint, or straight to the tunnel. Same failure rule as `relay.rs`;
  `None` returns the delivery to the whole ladder. `RemoteStreamDelivery` only uses the
  persistent channel when the pinned route is HTTP (on that address); a pinned QUIC or
  tunnel goes through the per-request delivery without marking a fallback.
- `ReplyInbox`/`Ticket` - a ticket->oneshot channel map that lives in the `GatewayState`
  (it is the gateway that opens the request); `register` draws a UUID v4 per frame;
  `Drop` removes a ticket not yet delivered from the inbox on any exit path (error,
  deadline). When there are expected traversals, `deliver` releases the result and keeps
  only the snapshot for 30 s for late receipts; after that the unknown ticket becomes
  `410` and never falls into the next token.
- `StreamTicket`/`deliver_stream` - an mpsc channel bounded to 64 batches; a remote
  emitter waits for the receiver when the queue fills, and `Drop` removes the ticket so
  cancellation, stop, cycle or error do not leave pending deliveries. The producer/emitter
  pair is closed together: if the delivery detects that the gateway disappeared,
  `run_remote_stream_tasks` aborts the producer instead of leaving the KV session and the
  scheduler admission alive until the next timeout. On the return through `/v1/link`, each
  callback's ACK stays asynchronous, but a `410 Gone` is published to the
  `RemoteStreamDelivery`, which returns `GatewayGone` and closes the producer without
  falling to HTTPS.
- `DirectReply::new(inbox, urls)` - `None` when the node knows no address of itself; then
  the ring takes over instead of dispatching a frame that never comes back.
- `self_reply_urls` - gateway URLs in the order of preference of
  `gateway::self_dial_host` (operator > LAN > the interfaces' candidates); bind wildcards
  and loopback (v4, v6 and `localhost`) are discarded by
  `is_unroutable_host` - loopback-only returns an empty list, and then the ring takes over.
  The exception is the address the operator pinned (`advertise_addr`), which passes even
  being loopback: on a single machine that is the right configuration. What is refused is
  the loopback that appears alone, from `self_dial_host`'s fallback or from the interfaces'
  enumeration.
- `handle_relay_result` - the handler of `RELAY_RESULT_PATH` (`/v1/relay-result`);
  it does not use a client API key: the per-frame UUID v4 `Ticket`, known only to the
  chain, is the delivery's credential; a result without a live ticket is `410
  Gone`.
- `handle_remote_stream_result` - receives `/v1/remote-stream-result` and waits for the
  bounded mpsc before answering. It also uses only the `Ticket`; a late batch or a
  cancelled ticket receives `410 Gone`.
- `await_result_with_ack` - a `select!` biased among three things: the inbox's result (the
  normal case), the head's ack (it only matters when it fails, or when an old peer bubbled
  the whole answer - a present `token_id` counts as a result), and the frame's deadline the
  caller passes (`relay::frame_deadline`
  with the round's tokens and every hop of the plan, raised by the
  generation's `step_budget_ms` when the engine is slow - `relay::StepBudget`;
  a hop died after accepting). In prefill the deadline is a liveness one,
  `relay::prefill_frame_deadline` (10 min per hop): the chunk may wait in the
  peer's queue behind another session, and the work-based deadline brought both
  down. `RemoteDecodeStream::next` uses the same calculation between two
  callbacks. If the result arrives before the ack, it does not wait for late
  telemetry: the ack's future stays in a detached task to record the first hop's
  evidence.
- `deliver_result` - the tail (or the hop that failed) tries each `reply_to` URL
  in preference order, with a memory of three consecutive failures and a 5 min
  quarantine. Intermediate candidates get `DELIVERY_TIMEOUT=500 ms`; the last one
  gets 30 s to separate slowness from death. `410` stops trying, because the inbox
  no longer exists. A URL that swallows the SYN is paid once, the alternative takes
  over and the following batches do not repeat the wait.
- `deliver_failure` - delivers a `RelayFailure` in place of the result so the gateway
  names the dead node instead of waiting for the whole deadline.
- `hop_entry`/`accumulate` - each hop describes itself on the way: the list travels
  head->tail and the tail delivers it together with the result.
- `record_transport`/`deliver_transport` - the caller delivers a separate evidence per
  traversal, with transport, RTT and bytes. The inbox appends the evidences already received
  to the result and marks the trace as incomplete when some arrives late or does not arrive;
  telemetry never holds the answer back.

`direct_lab.rs`:
- The "law": **the cost per token does not depend on where the pipeline's head is.**
  Fake hops speak the real protocol (`wire`, `relay::send_frame`,
  `infer_server::forward_direct`, `direct::deliver_result`, and the fake gateway
  serves the production handler of `/v1/relay-result`), driven by the real
  `prefill::run`/`step`; the only fake part is the engine (it sleeps instead of
  decoding).
- A network emulated per site: each hop and the gateway live on a site and every
  message between different sites costs `LINK_MS=50` one-way, charged on arrival and again
  on the way back - that is what makes the bench measure traversals instead of the machine it
  runs on. The cost test pauses the Tokio clock so the scheduling of the rest of the suite is
  not counted as a network leg.
- `the_cost_per_token_does_not_depend_on_where_the_entry_point_is` - for
  N=2,3,4 with the gateway at the head, in the middle and at the tail, legs per token
  ≈ `minimum_legs` (a tolerance of 1 leg to accommodate frame overhead); the two ends tie; the
  worst entry (middle) costs at most one leg more than the head; and the bubbled ring with a
  remote head pays 4 legs (the case that hurt, the operator's drop from 25 to 14 tok/s),
  proving it is a correction and not the way it always was. It also asserts that each result
  frame is delivered direct exactly once and that the transport receipts do not enter that
  count.
  A prompt of more than one piece crosses whole (a lost `prompt_progress` truncates at the
  first), including with prefix reuse.
- It inherits from the 2026-08-14 investigation (`labsse.rs`), which proved the asymmetry and
  refuted the SSE hypothesis through a counterfactual.
- The relay's four failure modes (dead middle hop, dead tail, lost direct reply,
  timeout) still have no four-peer E2E scenario.

`runtime.rs`:
- `NodeConfig` - everything that defines the slice served: model_path/label,
  start/end/total_layers, peer_count/index, primary, inference_port, n_gpu_layers,
  n_ctx, offload_kqv, n_batch, max_sessions, batching_enabled,
  max_queue_requests, batch_window_ms, max_batch_sequences,
  max_batch_tokens, prefill_chunk_tokens, max_new_tokens,
  sampling, no_thinking, single_peer, no_embeddings, speculative_decoding_enabled,
  speculative_k/lookup_window, mtp_enabled, mtp_draft_tokens,
  mtp_min_probability, mtp_cpu_fallback_enabled.
- `NodeConfig::mtp_effective` - the MTP turns on on whoever serves the model's last layer
  (where the `nextn` block lives): the whole model (start_layer 0,
  peer_count 1) **or** the tail of a sliced pipeline (start_layer > 0,
  peer_count > 1). See the "Multi-token prediction" section below.
- `NodeRuntime::apply` - applies the slice and brings up the embedded Rust server
  (`infer_server::run`); changing the config restarts the server. It annotates the slice in
  `models/lastslice.rs` as **Pending** - but the annotation only says what the `apply` decided,
  not what the server managed to load.
- `NodeRuntime::supervisor` - keeps the `JoinHandle` of the current generation. On applying or
  reloading, the generation receives the stop signal and the runtime waits for the supervisor to
  finish `serve_with_quic`, the FFI cleanup and the release of the port before starting another
  server. That wait has a 5 s ceiling; if it overruns, it keeps the handle and refuses to start
  the new generation, so it does not overlap a `llama_free` still running.
- `infer_server::run_native_cleanup` - runs `LlmEngine::unload` in
  `spawn_blocking`; `llama_free` and the native destructors must not block the thread serving
  the gateway or the `/health` probes.
- `spawn_health_watch` - the single point that promotes the load: after two consecutive
  `/health` probes with `model_loaded`, it promotes the annotation from Pending to **Serving**. A
  first return does not clear the journal, because the process can fall exactly while building
  that answer. The watcher captures the journal's path when it comes up; another
  instance/test that changes `POM_CONFIG` afterwards cannot redirect its annotation. Only
  `Serving` authorizes resuming the slice on a restart.
  It watches every load attempt of its generation: the supervisor publishes each attempt on
  a `watch` channel, and after promoting, the watcher waits (no polling) for the next one. A
  restart inside the same generation (a Strata stage that stops because the earlier stage
  restarted) writes `Pending` again; before this the watcher had already returned and the
  node stayed "loading" with the engine ready.
- `note_unloadable_slice` - marks the slice as not resumable and puts the reason in the
  `status.detail` (what the panel shows); it is what an empty node says instead of staying
  silent - an empty, silent node is indistinguishable from a machine that never received a
  slice.
- `free_port` - frees the port before bringing the server up: it finds who holds the port
  (`lsof -ti :PORT` on Unix, `netstat -ano`+`taskkill` on Windows) and kills it (SIGKILL). It
  returns `Err(reason)` when the occupant is not identifiable or the port stays held after 5 s,
  and the supervisor puts that reason in the `status.detail` as `Failed` **from the first
  attempt** - before, the operator only saw "loading" until `MAX_RESTARTS` ran out while the
  server died in a loop.
- `log_file_path` / `append_log` - `<project_root>/infer_server_<port>.log`,
  one per port, pointed to by `RuntimeStatus.log_path` (`set_log`, called at the supervisor's
  startup). It carries the server's lifecycle (start, attempt, reason of the fall, model
  loaded); the stdout does not keep that history.
- `needs_residual_export` - the **only** condition to install the capture callback: it costs
  7.8-8.2 ms/token against 0.14-0.67 ms without it (measured
  2026-08-12); without the callback, `hidden_state` refuses with "residual capture
  did not fire".
- `MAX_RESTARTS=3` with `RESTART_INTERVAL=30s` between attempts (a newer generation cuts the
  wait short), the context ceiling is the model's own `<arch>.context_length` (`splitter::read_context_length`), with no fixed POM cap. The restart budget is persisted in the
  journal `<config>.last-slice.json` (the path `NodeRuntime` fixes when it is built, never
  read again from `POM_CONFIG`; the test benches give each runtime its own) and shared
  with the supervisor: if the slice does not fit
  in memory or falls during the confirmation, each new process increments `Pending.attempts`. On
  reaching `MAX_AUTOMATIC_ATTEMPTS`, the node comes up without a model and leaves the reason in
  `status.detail`; `pom undeploy` also records a persistent block.

`wire.rs`:
- `BatchFrame`/`BatchItem` - `frame_version=1`, several named items in the same concatenated
  payload; `encode_batch`/`decode_batch` reject an unknown dtype, a divergent shape, offsets
  outside the payload, an overlap, a duplicated item and a payload above 64 MiB.
- `encode_batch_items`/`decode_batch_items` is the distributed path; a single-session request
  uses the same protocol through
  `encode_batch_single`/`decode_batch_single`. There is no fallback to the v1/v2 frames
  from before the launch, which never shipped; released nodes speak `frame/v1-batch`, so a
  change to it must stay readable by them (root `AGENTS.md`, compatibility policy).
- **The hot path does not copy the tensor for nothing.** `encode_batch_items` writes the tensor
  once, straight into the buffer that goes to the wire (the positions come from
  `tensor_payload_len`, which computes them without allocating; `frame_prefix` already sizes the
  buffer). `split_frame` separates header and payload by **borrowing** the payload, and
  `decode_batch_items`/`decode_batch_single` come out of that. `peek_batch_header` and
  `validate_batch_bytes` read only the header, for whoever needs the route, the `batch_id`, the
  item count or a validation without copying the whole hidden state. `validate_items` expresses
  the validation over the payload's **size**, which is what makes both possible. The conversion
  goes through a stack block: growing the vector value by value costs more than the conversion
  itself. Numbers in `docs/subsystems/inference.md` §2.2.
- `FrameEncoding::F16` stays only as a marker for internal calls;
  the official wire is always `inference/frame-v1-batch`. The helper
  `encode_with_encoding` belongs only to the equivalence proof in tests;
  `WireDtype{F32, F16}` keeps `as_str`/`parse`/`bytes_per_value`.
- `MAX_FRAME_PAYLOAD_BYTES=64 MiB`; the relay separates batches by the payload budget and does
  not coalesce the head when the answer may carry hidden state without the size appearing in the
  request.

## Multi-token prediction (MTP) - `mtp.rs`

`mtp.rs` is the `nextn` drafter: `MtpDrafter::observe` / `accept` / `draft`, the draft travelling
back to the head inside the stream frame that was going out anyway (`next_draft`), and the CPU-only
companion the runtime prefers when one exists. It is a different mechanism from `find_draft`'s
prompt lookup, which is why there are two keys (`mtp_enabled`, `speculative_decoding_enabled`).
Whoever drafts is the peer serving the last layer, where the `nextn` block lives;
`PipelinePlan::supports_mtp` demands `speculate/v1-mtp-sliced` on every hop and `speculate/v1-mtp`
on the tail.

The full mechanism, the two details that are the correction and what is still unmeasured:
`docs/subsystems/inference.md`, sections 5.1 and 5.2.
## Typed decisions (`probe`)

- `prefill::DecisionProbe` (gateway side) carries the labels, the head's `retain_prefix_bytes` and
  the question metadata; `add_probe_to_head_meta` puts them in the head frame, and `trace_summary`
  builds `RequestTrace::jev`.
- `infer_server/prefill.rs`: `HeadProbe`/`parse_probe` read the frame, the head bounds the stable
  boundary with `prefix::bounded_stable_len`, and the last peer answers `probe_result` (plus
  `probe_ms`) before sampling. Probe jobs run alone in the scheduler.
- `RequestTrace::refresh_telemetry` expects no decode rounds for a trace with `probe` or `jev`:
  decisions are prefill-only.
- Mechanism: `docs/subsystems/inference.md` 3.9.
## Connections

- **Called by**: `backend/gateway/` (assembles the `DirectReply` with `ReplyInbox` +
  `self_reply_urls`, registers `/v1/relay-result` and
  `/v1/remote-stream-result` without an API key, using the per-frame `Ticket`, passes
  `direct.as_ref()` to `prefill::run`/`step`; `step_speculative` goes without a direct),
  `backend/webui/` (the rollback/release fan-out), `main.rs`
  (`NodeRuntime::apply` at startup, including with `lastslice::take`),
  `routing/pipeline.rs` (the capabilities `CAP_REMOTE_TOKEN_STREAM`/
  `CAP_RELAY_CHAIN`/`CAP_RELAY_DIRECT`/
  `CAP_SPECULATE_LOOKUP`/`CAP_SPECULATE_MTP`/`CAP_SPECULATE_MTP_SLICED` decide
  ring vs direct vs hub, the wire format per hop and which of the two speculations runs).
- **Calls**: `llm.rs` (decode/hidden_state), `wire.rs` (frame encode/decode),
  `relay.rs` (send_frame). `runtime.rs` consumes `models/model.rs`
  (`LayerAssignment`, `ModelProfile`) and `network.rs`.
- **Consumed by**: `models/lastslice.rs` serializes `runtime::NodeConfig`
  (the annotated slice reapplied on restart) and is what keeps the annotation's state: Pending
  (written by `apply`), promoted to Serving only by
  `spawn_health_watch`; `routing/AGENTS.md` lists the capabilities that
  `infer_server/`/`relay.rs` carry out.
- **External**: no external process. The model loads in-process through
  the engine's `llm` module (`llama-engine`, re-exported by `main.rs`), over
  llama-cpp-sys-2.

### Multimodal prefill

`LlmEngine` loads the `mtmd` context only when `NodeConfig.mmproj_path` is configured, and images
use neither prefix reuse nor concurrent batching. `NodeConfig::validate` refuses a projector whose
output identity or dimension contradicts the model, so an incompatible `mmproj_path` persisted
earlier never reaches the server.

The projector's placement and caps, the hyper-connection `hc` copies, the four M-RoPE positions per
row, the KV-cell versus logical-position split and the cause of a failed prefill:
`docs/subsystems/inference.md`, "Multimodal input".
## Maintaining this file

This file is a map of the folder's responsibilities. When you change an inference interface or a
session invariant, update it here and the subsystem README in the same commit. Keep
implementation details in the code or in `docs/subsystems/inference.md`, so this index stays
short.


Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
