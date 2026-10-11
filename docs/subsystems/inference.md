# Inference path

What the code does today, checked against the sources (see the version note at
the end).

A generation request (OpenAI `/v1/chat/completions`, Anthropic `/v1/messages`,
`POST /api/generate`) goes down a single path:
`gateway::generate_stream_with_stops`. It plans the peer chain with
`routing/pipeline.rs`, sends the prompt and each token through the distributed
pipeline and returns tokens to the client. This document describes the
execution modes, the hidden-state binary format, chunked prefill, the direct
reply, speculative decoding and the limits.

The `/v1/prefill`, `/v1/step` and `/v1/step-stream` handlers hold a
`LoadGuard` during computation. The tracker is shared by the gateway and the
local inference server, so `GET /api/ui/node` also measures requests that
arrive straight at the inference port; no requests reports zero, and a read
failure stays unknown in the UI.

## Multimodal input

The catalog and the deployment carry vision as explicit state: without
`NodeConfig.mmproj_path`, an image part in `/v1/chat/completions` is refused
with `este modelo não tem visão` ("this model has no vision"); text files are
still accepted. With the projector loaded, the gateway validates base64
`data:image/...` images, sends them to the first peer together with the prompt
and the prefill uses llama.cpp's `mtmd`. Qwen3.8-Flash-Next follows that
contract: the GGUF declares `general.architecture=qwen4exp` and needs
`mmproj-BF16.gguf` or `mmproj-F16.gguf` as a sibling file. The ignored test
`qwen4exp_initializes_vision_from_separate_mmproj` loads both artifacts from
the real paths when `POM_QWEN4EXP_MODEL` and `POM_QWEN4EXP_MMPROJ` are set. The
path is intentionally separate from the text prefill so prefixes are not reused
and images are not mixed into concurrent batches.

### KV cells and M-RoPE logical positions

An image writes one KV cell per embedding token, but in M-RoPE it only takes
`max(nx, ny)` **logical positions**: `mtmd_input_chunk_get_n_pos` is
deliberately smaller than `mtmd_input_chunk_get_n_tokens`. A 28x28 image
produces 784 cells and advances the position by 28. The
`find_slot: non-consecutive token position` lines llama.cpp prints on the image
batch are the normal consequence of that design, not a sign of a defect.

That is why the peer's session keeps the two quantities apart: `n_past` is the
next logical position and `prompt_cells` the count of physical cells. Whatever
decides whether the prompt has one more chunk compares position with position
(`prefill_has_more_chunks`), and a multimodal decode is always complete because
`mtmd_helper_eval_chunks` consumes every chunk in one call. Collapsing the two
into one made the head ask for a second chunk of an already ingested prompt and
decode the `vec![0; n_tokens]` filler of `prompt_tokens` as if they were real
tokens - the answer came out as a row of exclamation marks. Sessions with media
also do not serve as a reusable prefix nor as a snapshot
(`Session::positions_match_cells`): `prompt_tokens` there is filler, not the
cells' real ids.

On the hub hand-off, `mrope_positions` travels in the metadata together with the residual so
every downstream slice preserves the head's logical positions.

### The projector's placement and its caps

`LlmEngine` loads the `mtmd` context only when `NodeConfig.mmproj_path` is configured. The head's
prefill carries the images in base64 only on the first chunk; the peers decode them and the `mtmd`
does the prefill in order.

Before applying a configuration, `NodeConfig::validate` reads both GGUF headers and refuses a path
that is not a projector or whose output identity/dimension contradicts the model. That also keeps
an incompatible `mmproj_path` persisted earlier from reaching the server.

The projector is placed by `VisionContext::load` after the model and its contexts are loaded: on
the GPU with the most free memory that fits its weights plus `VISION_COMPUTE_RESERVE_BYTES`, or on
the CPU when none fits - loading it on a full GPU made llama.cpp abort in the warmup or dereference
null in `ggml_gallocr_alloc_graph` on the first image. `image_max_tokens` is capped at
`VISION_MAX_IMAGE_TOKENS`, which bounds the warmup and each encode.

### An image on a hyper-connection model enters as `hc` copies

`qwen4exp` reads any batch without token ids as its wide residual (`n_embd_out = hc * n_embd` per
row), the form one pipeline hop forwards to the next. The projector emits `n_embd_inp` per row, so
`mtmd_helper_eval_chunks` aborted the head in `ggml_reshape_3d` on the first image. When
`n_embd_out` is a whole multiple of `n_embd_inp`, `decode_multimodal` evaluates the chunks itself
(`eval_chunks_with_wide_images`): text chunks still go through the mtmd helper, and each image is
encoded by mtmd and decoded with every row repeated `n_embd_out / n_embd_inp` times, which is how
the token path starts the residual. Positions and the non-causal switch follow the helper.

Such a model runs without `cparams.embeddings`, so its last layer keeps only the rows marked as
outputs; when the hop forwards the residual, every text and image cell is marked (the mtmd helper
marks only the last one, and `take_residual` then refused the short capture). Text chunks are
decoded there too for that reason. Other models keep the stock helper.

### M-RoPE hidden-state batches carry four positions per row

With an embedding batch (`batch.token` null) llama.cpp reads `pos[j * n_tokens + i]` for every
M-RoPE section. Text-only requests used to send one scalar position per row, so every peer after
the head rotated sections 1-3 with heap garbage and a long prompt crashed in
`llama_batch_allocr::ubatch_add` once the read crossed a page. `decode_hidden` and the batched
hidden path build the four sections (`text_mrope_positions`) whenever the model's rope type is
MROPE or IMROPE. MTP batches carry tokens, so llama.cpp reads one position there.

### The cause of a failed prefill

A failed prefill reaches the client as `Prefill falhou: <cause>` ("Prefill failed: <cause>"), built
by `prefill::failure_cause`, which drops the telemetry and resume wrappers from the error chain;
the gateway also logs the full chain at `warn`.

## Inference port and administrative exposure

The inference server (`inference_port` in `pom.json`) keeps listening on IPv4
and IPv6, with a fallback to plain IPv4 when the machine offers no IPv6. That
port is not closed by the administrative UI preference: direct HTTP is the
required fallback when the libp2p tunnel fails. On the gateway, `/api/generate`
and `/v1/**` follow the same availability rule; only `/api/ui/**` and
`/admin/**` enter the localhost/token boundary described in `http-api.md`.

### UTF-8 assembly in streaming

llama.cpp returns each token's piece as raw bytes: a BYTE token can be a single
byte and a normal token can end in the middle of a codepoint.
`llm::LlmEngine::token_to_piece_bytes` keeps those bytes up to the emission
limit. Each `Session` of `infer_server/` has its own `Utf8Assembler`, which
only delivers complete characters in the delta. On the speculative path, only
the accepted positions advance the accumulator; rejected positions must not
contaminate the next round.

The accumulator is drained with `flush` when the model ends on EOS. Cleanup by
explicit release, error, cancellation or timeout also calls `flush` before
returning the KV slot, discarding the result on those paths and guaranteeing
that no leftover survives into another request. A truly invalid sequence keeps
the `from_utf8_lossy` semantics: it produces a single U+FFFD.

## 0. Tensor parallelism collective (the engine's `collective` module)

For path C (tensor parallelism without transferring weights), the exchange core
is the all-reduce collective: each rank produces a partial sum of the
row-parallel projection and the result is the sum of the partials.
The engine's `collective` module has `reduce_sum` (the math sum) and `ReduceBarrier`
(the barrier that accumulates each rank's partials and returns the sum to all
of them through a `oneshot`, modeled on `direct.rs`'s `ReplyInbox`). The HTTP
transport (`POST /v1/tp/reduce`) is a thin layer on top, wired together with the
llama.cpp fork that calls the collective in the middle of the forward pass.
Rank 0 contributes directly to the local `ReduceBarrier`; the workers send the
partial to the primary. `broadcast_to` keeps the base URL and the route separate
so the tunnel forwards the request to the right path on the worker's loopback.
The broadcast and the remote reduce follow the transport preference: with QUIC
on, they try HTTP/3 and fall to HTTP/HTTPS; with QUIC off, they try the direct
candidates. In both cases, the libp2p tunnel is only used after a direct
transport failure, when the member announced a `PeerId` and the capability.
That order is required by the collective: the call waits inside the barrier and
an unavailable tunnel must not delay a reachable HTTPS address.
`TunnelRequest` keeps the barrier, rank and authorization headers. The routes
between peers accept `Authorization: Bearer <connection_key>` together with
`x-pom-internal-node: 1` on private networks and on the public HTTP fallback;
HTTPS and QUIC can use the secure transport and the marker without depending on
the secret. A client API key is not a fallback and does not cross the pipeline.
The tunnel's loopback keeps the session release's JSON `Content-Type` instead of
applying the binary default. The gateway validates the client's API key at the
entry surface and can query the principal for an unknown key. That query takes
no part in authenticating the internal hops. When a step's announced target is
this process's own `node_id`, the relay dispatches the frame straight to the
in-process inference handler. That call opens no socket and does not go through
HTTP. The HTTP inference server stays up for health checks and for remote peers;
with QUIC on, the remote order is QUIC, direct HTTP/HTTPS and, only last, the
libp2p tunnel. With QUIC off, direct HTTP/HTTPS also comes before the tunnel.
That ladder only applies to an edge with no analysis yet. With a route pinned by
the route plan (`route_plan`, see `p2p-network.md`, "Per-edge route plan"), the
frame only uses the measured transport; a failure before delivery retries the
route once, moves to the next measured route and, with the list exhausted, asks
for a reanalysis and goes down the full ladder. The return to the gateway
(`reply_to` and remote-stream) follows the pinned route of the delivering
node's `Gateway` plan. A failure in a call first walks the available transport
ladder without waiting for the backoff: QUIC, HTTP/HTTPS and, last, the libp2p
tunnel. Only if every path confirms the body did not reach the peer can the same
chunk be repeated, with three attempts and a short backoff of 100/200 ms. A
timeout or a lost answer after the body started is ambiguous and does not
repeat the chunk.

In prefill, a failure proven to have happened before delivery resumes at the
index and absolute position already accepted, keeping the same session id, the
remote KV, the prefix cache and the accumulated trace. The whole session is
only rebuilt when the peer no longer has the session, the delivery was
ambiguous, a downstream hop may have advanced the KV or a reliable absolute
position is missing; each rebuild records the reason. A cold QUIC open has a
separate setup budget, in milliseconds: 3x the peer's measured RTT, between 100
and 300 ms, or 150 ms before the first measurement. Sending the frame and the
answer use their own service deadline, so a large body does not eat the short
handshake budget.

The route cache keeps failures per endpoint, including the port. An inference
HTTP failure does not remove the P2P address of another port on the same host;
address rediscovery and KV resume are separate mechanisms. See
`docs/superpowers/specs/2026-08-17-tensor-parallelism-design.md`.

llama.cpp's native callback runs on a `spawn_blocking` thread, outside a Tokio
task's local context. That is why registering the group captures the server's
`tokio::runtime::Handle` and the callback uses that handle to await the reduce;
calling `Handle::current()` inside the native thread panics.

The map telemetry does not invent RTT from a topology probe. The Chat endpoint
aggregates the measurements of the finished run: each forward hop is a
directional origin->target leg and the final return is a peer->gateway leg. The
value is the observed duration of the operation on the effective transport,
without dividing by two or copying another origin's RTT. If the asynchronous
operation publishes no compatible observation, the leg stays unknown. A
`/health` probe can still exist for admission and planning, but is not
presented as the run's RTT nor mixed into the cycle cost.

The visual and temporal cycle of a three-peer pipeline is
`Gateway->A->B->C->Gateway`. The network sum only uses those legs, once each,
and the total cost adds each hop's measured compute. The backend exposes it in
`RequestTrace.pipeline_cycle`; the UI shows the aggregate at the virtual
Gateway at the end of the row. A missing leg does not become zero: the total
stays explicitly unmeasured.

Each run can also carry `node_timings.tp`. That block records expected and
observed reduces, total wait, the collective's p50/p95, request/response bytes,
local calls, broadcasts and the direct or tunnel path. The wait includes the
synchronous barrier and is not pure packet latency. TP bytes and waits stay
separate from the pipeline's hidden-state frames. With 32 layers and two
row-parallel points per layer, decoding one token can do 64 collective waits;
that is inherently expensive between machines on the internet. For WAN, a
pipeline with whole layers per node exchanges one hidden-state frame between
blocks and avoids those TP collectives.

`PhaseTrace` also keeps `round_trip_ms`, the sum of the observed round trips of
that phase's calls, and `network_ms`, which uses the connection RTT measured by
QUIC when available or the residual of `round_trip_ms` minus the remote
`processing_ms` otherwise. The summary publishes the quantiles of both. The
field goes in the stream's `x_pom` for the Chat screen and enters neither the
frame between peers nor the planner. The Chat uses `/api/ui/chat/telemetry` to
fetch separately the directional RTT of the edges between the trace's peers.
`/health` publishes `profile_sync_enabled` per machine, derived directly from
`POM_PROFILE_SYNC`; a zero in `node_backend_sync_ms` is never used to deduce
that flag. `expected_decode_spans` uses the decode rounds (output tokens minus
the accepted MTP drafts) times the plan's hops when there is no explicit
expectation from the remote-stream callbacks: each hop reports one span per
round, and a speculative round produces several tokens. Counting only the
tokens expected ~3.8x the spans of a run with MTP and marked 26% coverage on a
complete trace. The reported spans are still observed counts per hop. Typed
`/v1/systemone` (`/v1/decisions`) requests are prefill-only: the final peer
scores labels from the next-token distribution and does not run a decode step.
They count one API output token, but a trace with `probe` or `jev` expects zero
decode rounds, so its coverage follows the prefill peers (section 3.9).

`hidden_state_dtype` is evidence from the wire, not the gateway's local
configuration: it only shows up when hidden-state bytes crossed the hop. That is
why it is absent, instead of claiming f8 or f16, for a single-node whole model
plan, which transmits no hidden state. If there are no output tokens and no
explicit callback expectation, coverage stays unavailable and the UI does not
show `N/0 · 100%`.

### End-to-end trace and time outside the node

`RequestTrace.trace_id` is the same identifier as the request audit and travels
in the prefill and step metadata between gateway and peers. A node does not
subtract another node's clocks: each process measures intervals with `Instant`,
and the `*_at_ms` fields use wall clock only for visual correlation. The gateway
records local spans for validation, orchestration, pipeline selection, route
resolution, session acquisition and the return of the first token. Those
intervals form the local timeline and are not added to peer spans to fabricate
an E2E partition. The `outside_node_total_ms`, `outside_node_accounted_ms` and
`outside_node_unaccounted_ms` fields are only published when an explicit
residual measurement, with compatible time boundaries, is provided; the runtime
does not derive them from E2E minus the aggregated processing. If the profile is
`off`, the detailed spans and the attribution computation are not emitted;
`POM_PROFILE_SYNC` stays a policy independent of the trace.

`decode_calls` and `tokens_per_decode_call` are **derived from the hops**
(`RequestTrace::refresh_decode_counters`), not from a loose counter: the
continuation loop merges each leg's trace into the base trace, and the merge
carried `PhaseTrace::calls` but not the summary counter - which is why the
summary said "not measured" while the per-hop table showed 111 calls in the
same run. It is the **maximum** across hops, not the sum: a decode round touches
each hop once, and summing would divide the yield per call - the speculation's
acceptance rate - by the pipeline's size.

#### Pipeline admission probe

`ready_for_pipeline` still confirms, per request, that each announced peer
answers `/health` before entering the plan - the defense against an old UDP
announcer. The answer carries the executable identity (`model_label`/
`deploy_id`) and the range (`start_layer`/`end_layer`/`total_layers`), which
replace the P2P snapshot while the next `Announce` is still in transit. So a new
deployment does not fail with a coverage gap just because gossip arrived after
the local load. What changed is the price. The deadline is
`ADMISSION_PROBE_TIMEOUT` of 100 ms, not the general client's 2 s
`CONNECT_TIMEOUT`, and whoever fails goes into quarantine for
`ADMISSION_FAILURE_COOLDOWN` (`AdmissionHealth`, in `gateway/state.rs`), staying
out of the plan without being probed again. Before that, since `join_all` only
ends with the slowest, **one** unreachable peer cost 2.005 s on every inference
of the cluster - constant across engines, prompt sizes and cold/warm. Measured
with an announcer that swallows SYN: 2005 ms before, 301 ms on the first request
and 0 ms on the following ones. Coming back is automatic: once the deadline
passes, the next request pays one probe and the peer re-enters if it answers.

The probe follows the pinned route (`bases_by_route_plan`): a tunnel route goes
straight to the tunnel, an HTTP/QUIC route puts the measured address first. It
tries every inference address in `Network::inference_bases` order (the host that
already answered first) and, last, `/health` over the libp2p tunnel
(`infer_tunnel::health_via_tunnel`, 3 s) - the only way to reach a peer behind
symmetric NAT. Session lifecycle calls use `SESSION_LIFECYCLE_TIMEOUT` (60 s)
instead, because the peer's handler needs the engine lock. Each successful body
also feeds `AdmissionHealth::note_perf` (`fastest::PeerPerf`), which the
delivery-time planner and the dashboard telemetry read. A successful `/health`
also refreshes the model label, the deployment id and the layer range from the
inference process, so a P2P announcement still in transit cannot make a newly
loaded pipeline look incomplete.

The trace also carries `server_ttft_ms`, `engine_ttft_ms`, the decode call
count, tokens per call, cold/warm state and calls with a new, reused or unknown
connection. On the peer, `NodeTimings` records the first token produced and
serialized with a monotonic duration and a local wall timestamp. The web client
adds the browser's receipt with `performance.now()` and `Date.now()` only for
correlation. `streaming` separates first chunk, first token, duration, end of
the last token, number of chunks and bytes. More precisely,
`stream_duration_ms` is `first SSE frame -> [DONE]` on the browser's monotonic
clock: it excludes TTFT and includes the terminal interval until the body
closes. `time_last_token_to_response_end_ms` is
`last frame with a token -> [DONE]`, isolating that terminal interval.
`timings.total_ms`, on the other hand, is measured on the gateway when
`summary_for` builds the summary, before the final rewind/release that can delay
`[DONE]`. The fields are not interchangeable and must not be compared as if they
were a single clock. Fields the architecture does not measure stay absent and
show up as unattributed in the UI.

The contract and the incremental plan are in
`docs/superpowers/specs/2026-09-08-inferencia-trace-outside-node-design.md` and
`docs/superpowers/plans/2026-09-08-inferencia-trace-outside-node.md`.

`RequestTrace::telemetry` separates the end of the answer from the end of the
collection. The status is `collecting`, `response_complete`, `complete` or
`incomplete`, and the object publishes expected/observed peers, expected/
observed decode spans, duplicates, late receipts, missing peers, wall clock,
critical path and compute/transport totals. Those totals are not an exclusive
breakdown of the wall clock: the UI does not compute `outside-node` as
`E2E - sum(processing)`. `overlap_ms` and `true_residual_ms` stay absent until
there are synchronizable intervals to back them. If the prefill fails after
partial progress, the error audit gets the partial trace observed by the
pipeline; unobserved fields and `timings` stay null, with no substitute
estimate.

In `remote-stream`, the head sends the downstream peers' `hop_timings` together
with the token batch. Normal mode aggregates frames, bytes, open, lifetime,
first delivery, latencies, remote processing and wait with bounded-sample
histograms. The producer uses a bounded queue of 64 items and the emitter splits
`remote_stream_wait` into production until the token is ready, reservation and
send on the channel, time of the already queued token and delivery of the
previous callback. `remote_stream_downstream_wait_ms` is nested in production
and measures the relay to the next hop, including remote compute and transport.
There is no artificial timer: a high production value is only legitimate when
the decode, sampling, `mtp-draft`, residual or downstream spans explain it. The
callback's normal return uses the full-duplex `/v1/link` on the gateway; only an
explicit channel failure falls back to `/v1/remote-stream-result`, with the
HTTPS fallback count and reason in telemetry. `POM_PROFILE_SYNC=1` adds frame
spans and per-token events, keeping zero duration for an event whose batch does
not measure an individual interval. The published clock identifies the peer's
monotonic domain and is never subtracted from another machine's clock.

### KV cache and VRAM fallback

`n_gpu_layers` still controls where the weights live. The context first tries
to honor `offload_kqv=true`, putting the KV cache on the GPU. If the KV cache's
`cudaMalloc` fails, `LlmEngine::load_with_cache` repeats the context creation
with `offload_kqv=false`: the weights stay on the same GPU layers and the KV
cache moves to RAM. The application logs the fallback and `/health` shows the
effective value. That solves a lack of VRAM for the cache; if the weights
themselves do not fit, it is still necessary to reduce `n_gpu_layers` or split
the model across machines.

#### Limitation: KV on a GPU other than the weights'

The runtime does not offer a `context_gpu` or `kv_cache_gpu` field today. The
vendored llama.cpp API has no such degree of freedom:

- `llama_split_mode` documents `layer` and `row` as splits of layers **and KV**
  across GPUs (`vendor/llama-cpp-sys-2/llama.cpp/include/llama.h:198-203`).
  `tensor_split` changes the model's split, it does not create an independent
  split for the cache.
- `llama_context_params` exposes `type_k`, `type_v` and `offload_kqv`, but no
  device or buffer type for the KV
  (`vendor/llama-cpp-sys-2/llama.cpp/include/llama.h:421-468`).
  `offload_kqv=false` is the only placement fallback and picks CPU/RAM.
- In the native implementation, each KV layer uses `model.dev_layer(il)` and
  that device's buffer type when `offload_kqv` is on
  (`vendor/llama-cpp-sys-2/llama.cpp/src/llama-kv-cache.cpp:210-223`).
  `model.dev_layer` is defined by the same layer allocation that loads the
  weights (`vendor/llama-cpp-sys-2/llama.cpp/src/llama-model.cpp:1363-1384`).
- `llama_model_tensor_buft_override` belongs to `llama_model_params` and is
  applied to tensor names while loading the model
  (`vendor/llama-cpp-sys-2/llama.cpp/include/llama.h:302-322` and
  `vendor/llama-cpp-sys-2/llama.cpp/src/llama-model-loader.cpp:1237-1254`).
  It does not reach the `cache_k_l*` and `cache_v_l*` tensors, created later by
  the context.

POM's wrapper confirms the boundary: `crates/llama-engine/src/llm/model.rs`
only passes `devices`, `tensor_split` and `tensor_buft_overrides` in
`llama_model_params:429-447`, while creating the context only fills `n_ctx`,
`n_seq_max`, `kv_unified`, `offload_kqv` and the K/V types in
`crates/llama-engine/src/llm/config.rs:1012-1022`. There is no existing API that
lets the context's cache point at one GPU while keeping the weights on another.

So `split-mode layer`, `row` or `tensor` can use both GPUs, but changes the
placement of weights and KV together. The alternative available without
changing the fork is `offload_kqv=false`, which keeps the weights on the GPU and
moves the KV to RAM, not to another GPU. The full investigation and the contract
proposed for a future native change are in
`docs/architecture/kv-cache-device-placement.md`.

The cache's numeric type is an installation setting, in `pom.json`, not a
per-machine choice inside a deployment:

```json
{
  "cache_type_k": "q8_0",
  "cache_type_v": "bf16"
}
```

The accepted values are `f32`, `f16`, `bf16`, `q8_0`, `q4_0`, `q4_1`,
`iq4_nl`, `q5_0` and `q5_1`. When the fields are missing, both use `f16`. The
runtime applies the same setting to the main context, the MTP context and
`llama_rs_fit_params`; `/health` exposes `cache_type_k` and `cache_type_v` after
loading to confirm the effective value. The deployment can still choose
`offload_kqv` per machine, because that only controls the cache's placement,
not its numeric type.

In pipeline or whole model deployments, `RuntimeParams.fit_memory=true` is on
by default. Before the real load, the application calls llama.cpp's
`common_fit_params`, which can reduce `n_gpu_layers` to fit the available
memory and keeps the total capacity matching `n_ctx * max_sessions` when it was
set explicitly. `/health` exposes `fit_memory_applied` and
`n_gpu_layers_effective`. An `n_gpu_layers` other than the default `99` is an
explicit choice and turns off the automatic layer fit. The native fit does not
support tensor parallelism; the planner and the runtime normalize that option to
off in TP mode. The KV fallback stays active after the fit.
`RuntimeParams.fit_memory_margin_mb` sets how much the fit leaves free on each
device, 256 MiB by default. Before calling `common_fit_params`, the bridge
queries the total and free memory on the backend in the same order as the GPUs
used by the fit; the free value from `cudaMemGetInfo` already excludes what
other processes take. So the margin does not subtract that usage again: it
covers the gap between the projection and late allocations, plus usage that
shows up between the query and the load. If the free value drops below the
configured margin, the reserve follows the free value; below the 4 MiB floor,
it becomes zero so every load attempt does not become an impossible target. The
value is not queried when `fit_memory=false`. `llama_max_devices()` is the
**compile-time capacity** of the vectors the C wrapper fills (16 today), never
the number of cards present. The vectors handed to C keep that capacity, but
every physical decision uses `fit_device_count`, which counts the eligible
handles of `llama_rs_get_devices` and falls back to the memory snapshot's count,
with a minimum slot for the host. This prevents the compile-time capacity from
being mistaken for installed hardware, both in the memory snapshot and in the
per-device MTP reservation. The native fit is exclusive to the real load. The
Deploy screen's capacity preview uses GGUF metadata, `plan::range_cost` and the
current free VRAM for an analytical estimate; it calls neither
`llama_backend_init`, `llama_rs_fit_params` nor `fit_plan`, so it does not
compete with a load in progress.

When `mtp_enabled=true`, the fit first fits the main model and its context
without MTP. Before considering the optional reservation, it reads
`general.architecture` and checks a list generated from the fork itself for the
architectures that implement `LLM_GRAPH_TYPE_DECODER_MTP`.
`crates/llama-engine/build/mtp_arch.rs` finds the effective check in each
`src/models/*.cpp`, crosses the class with the `llama-model.cpp` dispatch and
resolves the official name in `llama-arch.cpp`'s `LLM_ARCH_NAMES` table.
Comments that merely mention the symbol do not count, and the check must be in
the unambiguous scope of `build_arch_graph`. The build fails if the fork is
missing, if the extraction is empty, smaller than thirteen entries or
inconsistent, so an incompatible upstream change does not silently turn MTP
off. llama.cpp's public API exposes no query for graph support; unknown
metadata or an architecture outside the generated list turns MTP off before
creating any context and publishes the reason in `mtp_unavailable_reason`. The
`nextn` weight alone is not proof that the graph is valid.

For a known architecture, the combined reservation computes only the KV of
the `nextn` blocks from GGUF geometry and the configured cache types. It uses
the fitted MTP context's `n_ctx` once: `kv_unified=true` allocates one unified
buffer of that total capacity, while `kv_seq_budget` sets sequence IDs and does
not multiply the KV cell count. The reservation follows llama.cpp's actual
`tensor_split` layer mapping, so each NextN layer is charged to the device that
will hold it, even when the suffix spans devices. When llama.cpp returns an
all-zero split because its initial fit needed no changes, the native model
loader would derive the split from free VRAM. POM mirrors that default from the
fit's memory snapshot and carries the resulting explicit split into the real
load; an absent split is not itself grounds to send MTP to RAM. POM repeats the
joint fit up to four times until the reservation and resulting split agree; if
the split cannot be estimated or does not converge, MTP uses its CPU companion
instead of accepting an under-reserved VRAM fit. This calculation neither
opens a model nor builds a dry MTP context to measure `compute`: the exact
measurement would require building the graph, and an internal llama.cpp assert
at that stage would abort the node before POM could fall back. The compute cost
is therefore only validated on the real attempt to create the context, which
can fall back to RAM. The combined attempt is only accepted when it keeps the
main model's full fit; if it reduces `n_gpu_layers` or fails, the main plan
wins, the target is left without `nextn` weights in VRAM and MTP falls back to
the companion model in RAM, if allowed, or becomes unavailable. So the
conservative reservation protects the layers each token needs to run without
making the fit a source of aborts. MTP off reads no MTP metadata, estimates no
budget and does not try to attach a context. Before
creating the draft context, the runtime checks the evidence of the `nextn`
tensors in the GGUF and whether they were requested in
`llama_model_params.load_mtp`; `llama_model_n_layer_nextn` alone is not enough,
since it only exposes metadata and can stay positive on the target without
those weights. On the fallback to RAM, the context selection is made explicitly
on the CPU-only companion model. If the selected model does not carry `nextn`,
the guard refuses the context before the graph reservation, publishes the exact
reason `o modelo carregado não traz o bloco nextn` ("the loaded model does not
carry the nextn block") and keeps the main inference. If creating the MTP
context still fails in VRAM, the runtime tries the fallback when
`RuntimeParams.mtp_cpu_fallback_enabled=true`, the default. That fallback loads
a CPU-only sibling model for the `nextn` context, with `offload_kqv=false` and
`op_offload=false`; so the scheduler does not drag the graph back to the target
model's GPU. The target model stays in the placement defined by the fit. The
attempt only ends with MTP unavailable if the operator turns the fallback off or
the CPU model/context load also fails. `/health` publishes `mtp_ready`,
`mtp_allocation` (`vram` or `ram`) and `mtp_unavailable_reason`. The screen
shows the placement on every success and the operational reason when the model
has no `nextn`, when the fallback was refused or when both VRAM and RAM failed.
The `context_memory` field still separates the KV cache (`context`) from the
compute buffer (`compute`) for the live contexts. The MTP fit does not try to
project `compute` through a dry probe: `sched_reserve` and the graphs vary by
architecture, and building that graph as a measurement is not a recoverable
operation when llama.cpp hits an invalid input. The decision is deliberately
conservative: reserving the computable KV is less exact, but keeps the
possibility of falling back to RAM and cannot take the process down. The wrapper
can still report the real buffers after the MTP context was created; that is
telemetry, not part of the fit's speculative budget. `fit_plan` also logs the
PID, the process's own GPU memory when `nvidia-smi` provides it, the total, the
free memory and the margin per device. To keep the late reservation of CUDA
temporaries from taking the process down when the GPU is already almost full,
`fit_memory=true` keeps `n_batch` as the chunk limit, but uses
`n_ubatch=min(n_batch, 128)` in the context. The prompt still accepts chunks of
up to `n_batch`; the cost is more internal compute steps in exchange for a
smaller reservation. `/health` exposes that value as `n_ubatch_effective`. With
`fit_memory=false`, `n_ubatch` follows `n_batch`. That policy lives in
`llm::apply_context_batch_params` and is applied to both the main context and
the MTP context, without reducing the public `n_batch`.

#### Flash Attention and the load parameters

The project never writes `flash_attn_type`: every context comes from
`llama_context_default_params()`, whose default is
`LLAMA_FLASH_ATTN_TYPE_AUTO`. llama.cpp probes the backend when creating the
context and decides - the request is not the answer. Since nothing in the
public `llama.h` returns the decision, `llama_rs_context_flash_attn` (in
`vendor/llama-cpp-sys-2/wrapper_common.cpp`) reads `get_cparams()` and returns
the pair `(on, came from the probe)`.

`LlmEngine::load_params` joins that decision to the other context parameters
and `/health` publishes it in `load_params`. The values come from the live
context (`llama_n_ctx`, `llama_n_batch`, `llama_n_ubatch`, `llama_n_seq_max`,
`llama_n_threads`) whenever there is a getter; only `kv_unified`, `embeddings`
and `n_rs_seq` are kept in a `ContextParamsSnapshot` taken right before
`llama_new_context_with_model`, because they have no getter. **Echoing the
`EngineConfig` would be wrong**: the KV-in-RAM fallback rebuilds the context
with another configuration, and the operator needs to see what is running. The
Details screen shows all of it in "llama.cpp load parameters" (see
`web-ui.md`).

#### Safe order by speed

When the automatic fit finds more than one eligible device, POM collects the
handles in llama.cpp's same default selection and captures free and total
memory before any probe. It then measures once per handle a small F32
matrix-vector multiplication through GGML's generic graph interface. The score
stays cached for the process, so the next load of the same model does not
repeat the measurement. There is no vendor API, card table or fixed priority
per backend.

The ordering is only applied if every device reports its total memory and
every handle uses the same backend. If a backend reports no memory, or if the
backends are heterogeneous or unknown, POM does not guess: it leaves
`llama_model_params.devices` null, keeping the current order and behavior. A
speed measurement that failed, or speeds within 25% of each other, only mean
that cards of the same size keep their current order. The same applies to a single device, which does not pay for the
probe. The backend guard is needed because an explicit list can change the copy
and split topology of a mixed Vulkan/CUDA graph; the automatic fit must not turn
a reordering into a new heterogeneous path. When the order applies, the
explicit list is passed both to `common_fit_params` and to the
real load, from the largest total memory to the smallest. The fit fills the
devices in the order it is given: the first takes layers up to its memory
budget and each next device takes what is left, so the largest card is used to
its limit and the unused memory, and any layer that spills to system memory,
ends up on a smaller one. (With a slowest-first order, a 12 GB RTX 3060 was
filled to 376 MiB free with one layer overflowing while a 16 GB RTX 5060 Ti
after it kept 3 GB unused.) Cards of the same size are ordered by the measured
speed, fastest first, when the difference exceeds 25%; otherwise they keep
their order. The ranges stay contiguous and the number of graph boundaries does
not grow; the final range and the output head sit on the last device that
received layers.

An external MTP draft (`mtp_draft_path`) is a second model loaded on the device
of the last NextN layer. The joint fit reserves there, besides the NextN KV,
the weights of the file that is really loaded (the completed copy of a shared
draft, which is larger than the draft itself; an upper bound) and 512 MiB for
its compute buffers. Reserving only the KV let the fit fill that card, and the draft then
failed with `cudaMalloc failed: out of memory` on its compute buffers and MTP
was dropped. When the reserve does not fit without taking layers from the main
model, the existing policy applies: the main layers win and MTP falls back.
A deployment chooses where MTP goes with `mtp_placement` ("Where MTP runs" in
the deployment forms):

- `auto` (the default): MTP goes to VRAM only when the whole main model still
  fits there with it; otherwise it goes to host memory and the main model keeps
  every layer on the GPU.
- `vram`: MTP takes its VRAM first. The joint fit is accepted even when it
  offloads fewer main layers than the main model would get alone, and it runs
  even when the main model alone already did not fit entirely; the main model
  takes the memory that is left and gives up layers to host memory.
- `ram`: the fit is asked for host memory from the start; MTP spends no VRAM
  and runs on the CPU.

`mtp_cpu_fallback_enabled` keeps its meaning in every placement: whether a
VRAM attempt that fails may fall back to host memory instead of leaving the
deployment without MTP.

If the context of a draft whose weights are in VRAM still cannot be created,
with its KV on the card and then in host memory, the draft is loaded again in
host memory and attached there instead of MTP being given up.

The KV cache is not artificially moved to the slow device. It is accessed on
every token, grows with the context and follows the layer's device; so the fast
device also gets the KV of the layers it runs, within the memory
limit. Free memory is still the hard constraint for weights, context and
temporaries. This policy avoids trading a repeated crossing for a nominal byte
saving and keeps the existing fallback to `offload_kqv=false` when the KV does
not fit on the GPU.

This policy is POM-only. llama.cpp's `common/fit.cpp` was not modified: the
registry names the backend already exposes are enough to prevent an unsafe
explicit ordering without changing the memory distribution after the probe.

The external MoE executor is a host callback, but uses CUDA internally. Each
entry of the bridge gets the shard's GPU ordinal and pins allocations, streams
and events to that card, restoring the thread's current device on exit. Without
that guard, the callback can leave the thread on one GPU and the scheduler's
next wait on another receive a stream or event in the wrong current context.
The resident cache measures the compute buffers per `CUDA<n>`, keeps a separate
budget and LRU per card and the routes run the GPU groups in parallel before
summing the partials. Telemetry publishes the device names and
`device_cache_resident_bytes_by_device` in the same order as the shards. The 59
splits observed with the hook are expected: each externalized layer crosses the
custom host operation; reducing that number neither fixes the affinity nor
replaces correct synchronization.

Cache warm-up is opportunistic and cooperative. When the first real route
arrives, the runtime cancels it without removing entries already admitted; so
the warm-up thread cannot evict the request's working set nor turn a second call
into another sequence of misses and uploads. The detail of the confirmed cause
and the fix is in `docs/subsystems/freetoken-moe.md`, section "Phase 5.7".

The guard is also the degradation boundary for the defect observed on
2026-09-01: the Vulkan/CUDA combination keeps llama.cpp's default order and does
not get the list that made the slice change from `CUDA,Vulkan` to
`Vulkan,CUDA`. That protects the known load before the first decode. There is
still no in-process boundary able to catch any `ggml_cuda_error`: the fork calls
`ggml_abort` and ends the process after a CUDA error, and
`catch_unwind`/`llama_decode`'s error code do not intercept that abort.
Continuing in the same CUDA context after an illegal access would not be safe
either; isolation in a separate process or a change to the fork's fatal policy
would need an architectural decision of its own.

When MTP is on, loading also passes `llama_model_params.load_mtp=true`, so the
`nextn.*` weights are loaded before creating the draft context; leaving that
flag false keeps the MTP metadata, but leaves `nextn.eh_proj` null and makes
Qwen3.5 abort while building the graph.

### Reconfiguring without rereading weights

`NodeRuntime` keeps a local `ModelCache` with the `Arc<LoadedModel>` of the last
slice. On a new generation, `NodeConfig::weights_are_compatible` compares the
path, the range, the total count, the GPU load shape, the effective MTP and the
TP identity. If everything matches, `infer_server::run` calls
`LlmEngine::load_with_cache` and only creates a new `llama_context`, applying
the new context, session, KV, batch and sampling values. The previous generation
keeps owning the old context until the server stops accepting connections and
drains the in-flight requests.

A change of slice, path or load parameter clears the cache before publishing the
new generation. `NodeRuntime::stop` also clears the cache, because an explicit
unload must give the weights back. The cache is never shared between processes
or machines and changes no wire contract.

### Where the layers ended up

Nothing above says where the model **actually** ended up. `n_gpu_layers` is the
request; the `fit` reduces it on a full GPU, and a buffer type override sends
part of a layer - typically a MoE's experts - to the CPU without changing the
block's device. Deriving the placement from those numbers is wrong exactly in
the cases that matter.

That is why the inference server's `/health` carries two fields measured after
loading, both coming from llama.cpp through `wrapper_common.cpp` (see the
`AGENTS.md` of `vendor/llama-cpp-sys-2/`):

- **`placement`** - per layer, the assigned device (`dev_layer`) and how many
  weight bytes ended up in each buffer, plus the output head, the totals and
  `has_tensor_overrides`. A layer is "on the GPU" by its bytes, not by the
  block's device: with an override, the two diverge.
- **`context_memory`** - the context's memory per buffer, split into `model`,
  `context` and `compute`. `context` is the KV cache, and it is what answers
  where the KV ended up; `offload_kqv` is only what was requested, and the VRAM
  fallback above may not honor it. The size shown already reflects the
  configured type, which the same `/health` publishes in `cache_type_k` and
  `cache_type_v`.

Both are `null` when the bridge does not answer, and then the UI says it does
not know instead of inventing a map. They are shown by the Dashboard's machine
card, which fetches them through `/api/ui/peer/:node_id/runtime` only with the
card open - see `http-api.md` and `web-ui.md`.

Those snapshots run inside `GET /health`, after the model is loaded and before
the `model_loaded` answer reaches the process watcher. The same wrapper also
serves the device list used by sysmon and the catalog. In
`llama_rs_backend_devices_json`, `ggml_backend_reg_name(reg)` can be null on a
CPU-only machine; the wrapper normalizes that name, plus the descriptions and
the device/buffer names, before building strings or JSON. So incomplete
metadata does not turn `strlen(nullptr)` into a crash on Windows and the health
check stays observational.

## 1. The execution modes

Prefill happens in `inference/prefill/` (`run`); in decode, the gateway picks
the remote transport when the head is another node and announces
`inference/remote-token-stream-v1`. Otherwise, `step` uses the three
traditional modes:

1. **Batched remote transport** (`inference/remote-token-stream-v1`,
   `step_stream`/`/v1/step-stream`) - the gateway sends a control frame to the
   remote head and receives the tokens through callbacks at
   `/v1/remote-stream-result`. The head keeps the generation loop; in a
   pipeline with more hops, the other peers stay inside the head's relay. The
   producer puts each token in a queue bounded at 64 items and does not wait for
   the callback; the emitter drains up to 8 items that are already ready. If the
   queue fills up, sending the callback waits and applies backpressure up to the
   generation. Each callback URL that fails stays out for 5 min and delivery has
   a 500 ms ceiling per attempt, so a route that swallows SYN is not retried on
   every batch. EOS and error send a terminal batch; stop, limit, cycle and
   cancellation remove the ticket and make a late delivery get `410 Gone`. On
   the persistent return, the ACK stays asynchronous so it does not impose one
   RTT per token, but the `410` is propagated to the emitter as `GatewayGone`; it
   also ends the remote head's producer, releasing the KV session and the
   scheduler admission instead of leaving an orphan generation running until a
   timeout. In a sliced MTP plan of two or more hops, `remote-stream` sends
   `speculate.source="mtp"` to the remote head: the tail drafts `nextn` after
   sampling and returns `next_draft` in the same answer, and the head installs it
   in the next step. The draft lengthens the tail's round, but the round now
   yields up to `mtp_k` tokens; with one token per round, the whole head-tail RTT
   fell on every token (measured on 2026-09-28: ~12 tok/s without MTP, with 96%
   acceptance available on the same prompt through the legacy path). The
   callback also reports `speculated`, `speculation_active` and
   `speculation_disabled_reason`, with no extra crossing or call per token. Each
   batch also keeps the downstream peers' `hop_timings` and `span_count`, plus
   `stream_id`, frame index, lifetime, latency, bytes and wait; so the head is
   not the only visible hop in the trace. The control frame's `session_id` is
   copied into `reply_to` and into each batch, keeping the same session on the
   token's path up to the gateway's ticket.
2. **Direct reply** (`relay/v2-direct`, `run_direct`/`step_direct`) - when
   `PipelinePlan::supports_direct_reply()`: every step announces
   `relay/v2-direct` **and** the chain has at least 2 hops. The gateway
   dispatches each frame once to the head, carrying a `reply_to`; the tail
   delivers the result straight to the gateway at `/v1/relay-result`. The
   `reply_to` URLs are the gateway's local interfaces; when none answers - the
   normal case with the gateway behind NAT/CGNAT - delivery falls to the libp2p
   tunnel, using the `libp2p_peer_id` and `libp2p_addrs` that `reply_to` also
   carries. See `docs/subsystems/p2p-network.md`, "NAT traversal".
3. **Bubbled ring** (`relay/v1-chain`, `run_ring`/`step_ring`) - when
   `supports_relay_chain()`: the gateway makes **one** call (to the head) and
   each hop forwards to its next one through `next_hops`; the result bubbles back
   up the nested call stack.
4. **Hub** - the rest: the gateway calls each hop in sequence, gets the tensor
   back and resends it to the next.

The gateway log marks which mode was used
(`resposta=remote-stream|direta|anel|hub` - "reply=remote-stream|direct|ring|
hub"). The choice is all-or-nothing per request: a single step without the
capability drops the feature for the whole plan
(`supports_relay_chain`/`supports_direct_reply`/`supports_speculation` in
`routing/pipeline.rs`), so modes are not mixed in the middle of the chain. The
remote transport requires the head to announce the capability and the gateway
to have a callback address; without that the request keeps the previous path.
A local head never uses the remote transport.

## 2. Hidden-state protocol (the engine's `wire` module)

The tensor that crosses the network is the **only** place that knows the dtype
and the encoding in transit. Format:

```
[4 bytes big-endian: header size][JSON header][raw tensor]
```

- `dtype`/`shape` go in the JSON; the body is **binary**, in f16 by default, f32
  when the operator picks full precision, or groupwise f8/int4 in the lossy
  alternatives (never base64). Precision and encoding are a parameter of
  `encode`/`decode`, not an assumption.
- `frame_version=1` (`inference/frame-v1-batch`) is the first and only official
  contract. A JSON header carries `batch_id` and items `{item_id, session_id,
  shape, dtype, offset, len, meta}`, followed by the concatenated payload. The
  decoder validates version, dtype, the size derived from the shape, limits and
  overlap before exposing any tensor.
- A single request uses the same frame through `encode_batch_single`. The relay
  schedules the flush immediately and groups, when possible, concurrent calls to
  the same next hop, model, deployment, configuration, phase and route in the
  same executor cycle. So an isolated call does not wait for a fixed timer,
  while calls that already arrived together still share the frame. The
  receiving peer distributes the items concurrently to the local scheduler and
  returns the results by the original `item_id`.
- The product has not been released yet, so there is no fallback, adapter or
  compatibility code for earlier v1/v2 formats.
- `MAX_FRAME_PAYLOAD_BYTES = 64 MiB` caps the payload on decode; the axum
  handler applies the same ceiling to the body (`INFERENCE_FRAME_BODY_LIMIT`).
  The relay splits tensor batches by the frame's remaining budget and does not
  group a head that may return a hidden state when the answer's size does not
  show in the request. A deterministic size refusal does not repeat the same
  cold prefill.
- Decode always returns the tensor as f32: reduced precision is a property of
  the wire, never of the math. `f16` stays the default. `f32` keeps the captured
  f32 values exactly and uses four bytes per value, twice the f16 payload, with
  no precision conversion. `f8` uses FP8 E4M3FN, and `i4g64` / `i4g128` use
  signed int4 with one FP16 scale per group of 64 / 128 values. f8 and i4 are
  explicit lossy choices; i4 packs two values per byte plus the scales. The
  converter refuses non-finite values and FP16 scales out of range instead of
  silently saturating them.
- `RuntimeParams.hidden_state_dtype` is global to the deployment and follows the
  same persisted path as context, batch, prefill chunk and KV cache. The default
  stays `f16`; `f32` is a lossless option available on the deploy screen and
  needs no extra negotiation, since it is the wire's base format. f8/i4 are only
  sent when the operator picks them explicitly. Each peer announces separate
  capabilities for `inference/frame-v1-dtype-f8`,
  `inference/frame-v1-dtype-i4g64` and `inference/frame-v1-dtype-i4g128` in
  `/api/ui/node`. The planner blocks the deployment if any selected peer does
  not announce the chosen lossy format, and the server still refuses an
  incompatible frame with an explicit message. There is no silent
  interpretation.
- Each hop's trace records `hidden_state_bytes_sent`,
  `hidden_state_bytes_received` and `hidden_state_dtype`. The UI shows those
  fields per crossing and the effective format, including when a requested
  configuration cannot be used. The observed number, not a transport estimate,
  is the measure for comparing f16 and f8.

### Wire conversion telemetry

The per-phase telemetry separates `residual_wire_f16_encode_ms` and
`residual_wire_f16_decode_ms` from the peer's overall encode/decode time. It
also separates `client_f16_encode_ms` and `client_f16_decode_ms` on the side
that builds and consumes the remote frame. Those four fields only measure the
conversion of activations between f32 and f16; they are subsets of the overall
times and must not be added to them again nor to llama.cpp's compute. With f32
on the wire, the f16 conversion metrics are zero, but serialization, payload
reading and network cost remain. The Chat telemetry line shows those
conversions next to the f8 conversion and the frame serialization.

### Residual shape and cost per token

The captured residual is not assumed to be a two-dimensional matrix. The
capture finds the token axis from the known token count of the physical
microbatch and the tensor's shape. `n_embd_inp` describes the width of the input
hidden state and is not used to locate that axis: in architectures with
streams, the earlier features can make the product exceed that width. Every
axis before the token axis is kept in physical order. The wire normalizes that
to `[tokens, features...]`. Later dimensions larger than 1 are refused; later
dimensions equal to 1 are ignored. If more than one axis has the token count,
the smallest axis that also closes the contiguous strides is picked
deterministically. A geometry or stride that does not close that rule is
refused with the shape in the error. In multimodal prefill, the callback can
split the text segment from the cells expanded by the image; in that case the
capture identifies each callback's physical count and accumulates the segments
up to the expected total, without confusing that count with the logical
position `n_past`. On the hub path, the M-RoPE positions travel in the metadata
together with the residual to the following hops.

On receipt, validation uses `n_embd_inp` for the 2D shape and for extra
singleton dimensions. When there is a non-singleton extra axis, the effective
width is `n_embd_out`, which llama.cpp computes from the architecture and which
already represents the physical width produced by the callback. The metadata's
stream count describes the geometry, but is not multiplied again by the
receiver. So a `[tokens, n_embd, streams...]` shape only passes when the product
of the features matches the reported expanded width; an incompatible width is
still refused with the shape in the error. `decode_hidden` and
`decode_hidden_batch` use that same validated width to reserve the Rust batch,
avoiding accepting a shape that would later overflow the copy into the buffer.

When there is an extra stream dimension, the residual must carry each stream.
There is no generic boundary where a combined `n_embd` width is mathematically
equivalent: the pre-combination and post-combination ops keep the streams, and
the combination op only mixes sources into targets. The combination observed
after the layers is an auxiliary output, not the equivalent input for the next
peer. So for that architecture the correct path is to carry every stream; the
planner does not compress the tensor through a family-name heuristic.

The cost below is the tensor payload, measured by the same computation as
`tensor_payload_len`; it includes neither the JSON header nor the transport.
For `n_embd = 4096` and four streams, the cost per token is:

| Representation | Four streams | One combined width |
|---|---:|---:|
| f16 | 32,768 bytes | 8,192 bytes |
| f32 | 65,536 bytes | 16,384 bytes |
| f8 | 16,384 bytes | 4,096 bytes |

The one-width column is only the theoretical limit of an equivalent
compression; it is not used when the architecture provides no such boundary.
The microbatch order does not change with the extra dimension: `CaptureSlot`
accumulates each block in the order delivered by `split_simple`, enabled by
`kv_unified=true`.

Measured with `tcpdump` on peer 2 (a request of 35 prompt tokens + 34
generated): 565,336 bytes of hidden state from head to tail, 6,715 bytes of
sampled tokens on the way back. 565,336 / 69 tokens = **8,192 bytes per token
per hop**, exactly `hidden_dim 4096 x 2` (f16). When `f32` is selected, the
binary payload costs `4096 x 4 = 16,384` bytes per token per hop. The historical
base64 f32 format cost `4096 x 4 x 4/3 = 21,846` bytes per token per hop; the
measured binary f16 frame uses **62.5% fewer bytes** than that historical
format. A 512-token prefill chunk is ~8 MiB f16 per hop per chunk. With f8, the
payload of the same tensor drops to one byte per value, but the effective
reduction must be read in `hidden_state_bytes_*`, per crossing, because header,
transport and traffic pattern also enter the measurement.

### 2.1 Persistent link between stages (`transport/link-v1`)

The frame above describes **what** crosses. This section describes **how**,
inside the pipeline: between one hop and the next.

The batched remote transport (section 1, item 1) took the per-token crossing off
the gateway -> head stretch. The head's loop also uses the persistent channel on
the next hop when it announces `transport/link-v1`: `POST /v1/step` stays the
logical route, but headers, routing and framing travel inside the per-edge
stream. The `remote_stream` flag no longer excludes that path. When the
capability does not exist or the channel fails, the fallback ladder can still
make one request per activation. The TCP/TLS connection underneath already came
from the pool (that is why `tls_handshake_ms` only shows up on the hop that
opens a connection).

The initial `/v1/step-stream` control is not wrapped as a batch because that
route's receiver expects the control wire. After the head starts the loop, the
MTP handoff in `/v1/step` carries `draft` and `mtp_k` in a one-item batch; those
fields must not stop the `BatchCoordinator`, and the batch is delivered through
the persistent channel when the hop announces `transport/link-v1`. The
coordinator's key marks the contract as speculative, keeping that frame
separate from dense batches.

`transport/link-v1` keeps **one channel per edge** (`node_id` + inference URL):
a single `POST /v1/link` whose outgoing body is a stream of binary frames and
whose returning body is another stream of binary frames. Opening tries up to two
routes, with a ceiling of 400 ms per address and 800 ms in total, and keeps the
failed route in memory for 5 min. A per-edge guard only serializes concurrent
openings to the same target; the opening happens outside the channel map's
global lock. Each frame, in both directions:

```
[4B BE size of the rest][2B BE status][2B BE id size][id][frame/v1-batch]
```

- `status` is `0` on the way out and the HTTP code on the way back. A peer
  refusal arrives with the same meaning it would have on the per-request path
  and becomes `HttpRefusal` in the caller - switching transport does not change
  a considered refusal, so it is **not** retried another way.
- `id` is the frame's `batch_id`, the identity the `BatchCoordinator` already
  uses. It correlates way out and way back and is what allows **several
  sessions on the same channel**: frames do not come back in order, they come
  back identified. Each frame is dispatched in its own task on the receiver, so
  a slow session does not head-of-line block the session that arrived after it.
- The route goes in the `frame/v1-batch`'s own `kind`, so the envelope does not
  duplicate the path. The receiver dispatches through the same in-process route
  table the per-request path uses, and channel and request run exactly the same
  inference code.
- The outgoing queue has 64 frames: a slow hop holds back whoever feeds it
  instead of letting memory grow. The body limit is turned off on that route,
  because the body is the whole generation, not a frame.
- **The credential is rechecked on every frame**, not only at opening. The
  receiver checks the `connection_key` and the internal marker; a 12 h
  connection that authenticated once would keep valid for 12 h a secret replaced
  by another process. That authenticates the peer on the transport, it does not
  add an Ed25519 administrative signature to each inference frame. A refused
  frame is a refusal of **that frame** - the channel keeps serving the following
  ones.

**Degradation.** A transport failure on the channel does not kill the
generation: with QUIC off, the call goes back over per-request HTTP/HTTPS and
only then through the libp2p tunnel; with QUIC on, it falls first to HTTP/HTTPS
and uses libp2p only as the last reserve. In both cases it publishes
`transport_fallback=true`, and the dead channel leaves the pool. A target whose
opening failed stays in quarantine for 5 min, the same deadline as the route
memory, so the opening attempt is not paid per token on a cluster where the
direct address does not work (a peer behind NAT only reachable through the
tunnel). The channel has a 12 h lifetime ceiling, because the HTTP client's
timeout applies to the whole request and the streaming body counts toward it;
once the ceiling passes, the in-flight call degrades and the next one opens a
new channel.

On the `remote-stream` return, the remote head reuses that same persistent
protocol to send callback frames to the gateway, which delivers them to the
ticket and only answers the envelope's ACK. The producer's internal hop uses the
same channel for the `/v1/step` activations, correlated by `batch_id`; waiting
for the answer is still required to respect the autoregressive dependency, but
opening a new HTTP request is no longer part of each token. The emitter queues
the callbacks without waiting for the individual ACK, so the return's round trip
does not become a per-token barrier; the channel's bounded queue still applies
backpressure and a reader task watches late ACKs and failures. If opening or
using the channel fails, `direct::RemoteStreamDelivery` switches to the HTTPS
POST per callback and records `remote_stream_transport_fallback` and
`remote_stream_transport_fallback_reason`; the switch stays visible and is not
confused with the transport's aggregated name.

### 2.2 Optional HTTP/3 over QUIC

When `Config.quic_enabled` is turned on by the principal, new hidden-state hops
and direct return deliveries prefer HTTP/3 over QUIC. The endpoint uses a
dedicated UDP port, derived from `p2p_port` as `p2p_port + 1000` and announced
as `inference_quic_port` (the gateway announces `gateway_quic_port`). The client
gets the `host:port` addresses from the peer's public announcement, with IPv6
properly bracketed; `inference_url` stays only the TCP fallback. The endpoint
configuration is created separately from the TCP TLS configuration and the
HTTP/3 server is ready from startup, so flipping the preference neither
restarts the model nor interrupts a call in flight. Each request opens an
independent stream of the QUIC connection per peer and announced address, and
the session stays in the pool to allow future multiplexing.

QUIC always negotiates its own TLS 1.3 and validates the Ed25519 certificate
against the `node_id`; that is why `quic` is encrypted even with
`tls_required=false`. If the negotiation fails, the peer does not offer HTTP/3
yet or the route answers 404/405, the same frame is tried over the TCP chosen
by the TLS policy: `http://` when it is off and `https://` when it is on. The
fallback path keeps the run going and publishes `transport_fallback=true`; an
inference HTTP refusal outside those codes stays final.

In `hop_timings` and in `transport_summary`, the `transport` field represents
the real wire and is limited to `http`, `https` and `quic` for these hops. The
persistent link's internal detail does not show up as a fourth layer: it is
normalized to `http` or `https`. The `transport_rtt_us` field carries the RTT
measured by the QUIC connection, without the remote handler's processing. The
`tls` field stays true for QUIC because TLS 1.3 belongs to the protocol itself,
regardless of the TCP switch. Each complete attempt, from the handshake to the
answer body, has a single deadline, the frame's: (1 s + 50 ms per token + 2 s
per image) x remaining hops (`relay::frame_deadline`), the same on QUIC,
HTTP/HTTPS, the persistent channel and the tunnel. A peer that went down is
detected earlier by the connection's lifetime (`QUIC_MAX_IDLE`, TCP keep-alive).
A prefill chunk (`/v1/prefill`, `relay::frame_deadline_for_path`) is the
exception: its deadline is a liveness one, 10 min per hop
(`PREFILL_FRAME_DEADLINE_PER_HOP`), because the chunk may wait in the peer's
queue behind another session's chunk; the work-based deadline expired during
that wait, the delivered frame could not be repeated, and both sessions failed
in cascade ("libp2p tunnel did not answer in time"). The gateway's wait for each
chunk's direct result (`prefill/pipeline.rs`) uses the same deadline.

**Cleanup.** Every pending delivery lives in a per-channel registry and leaves
it on a guard's `Drop`, on any exit path: answer, error, deadline, caller
cancellation. When the channel dies, the reader drains the whole registry with
an error before going away - no caller stays hanging, and a late answer finds no
ticket to deliver to.

**Measurement** (`inference/link_lab.rs`, 2026-09-03). The bench serves each
fake hop on top of a `TcpStream` wrapped in a counter, so the numbers are the
bytes that really went through the socket, not an estimate. A fake engine on
purpose: a hop that decoded would measure the GPU, not the transport. A
`[1, 5120]` f16 residual (10,240 bytes), 40 tokens per configuration,
macOS/arm64, test build. The only difference between "before" and "after" is
whether the hop announces `transport/link-v1`.

| Chain | Bytes/token/crossing (1st hop) | Bytes/token in the chain | ms/token (average of 5) |
|---|---:|---:|---:|
| 1 hop, request per token | 11,465 | 11,465 | 0.935 |
| 1 hop, persistent channel | **11,302** | **11,302** | **0.815** |
| 2 hops, request per token | 11,768 | 23,233 | 2.421 |
| 2 hops, persistent channel | **11,625** | **22,927** | **1.804** |
| 3 hops, request per token | 12,043 | 35,276 | 2.928 |
| 3 hops, persistent channel | **11,920** | **34,847** | **2.816** |

The byte columns are **deterministic** - they repeat value for value across
runs, because they are bytes counted on the socket. The time column is noisy
(test build, shared machine): across five runs it varies more than the
difference it measures on some rows, so the honest reading is "the channel did
not get slower and tends to be between 4% and 25% faster", not an exact number.
Whoever wants the production time has to measure it there, where the crossing
includes RTT and GPU.

**The byte gain is small and its size must be stated: 163 bytes per token per
crossing, -1.4%.** What the channel removes is exactly the per-activation HTTP
envelope, and that is all it has to remove. The reason is in the table itself:
of the 11,302 bytes per token, **10,240 are the hidden state** - 90.6%. The
network cost per token on the internal hop is the tensor, and the tensor does
not change size because of the transport; what reduces it is the wire's `dtype`
(section 2). Of the remaining ~1,060 bytes, the largest share is the JSON
header, and within it the `next_hops` repeated on every token (about 275 bytes
per remaining hop, visible in the 1st hop column: 11,302 with one hop against
11,920 with three). That is the next lever, and it requires the peer to keep the
route per session - it is not pure transport, and was not done here.

The channel is also what guarantees that **one** connection serves many tokens
and many sessions: the bench asserts zero new connections after warm-up, and six
concurrent sessions with ten tokens each fitting in a single connection.

### 2.1.1 Advertised address regression and failure memory (2026-09-04)

The regression that dropped the remote head to **2.2 tok/s**, with a **TTFT of
6.04 s** and **31 s end to end**, was not llama.cpp's work: the node measured
**593 ms** of processing, while the run waited about 30 s outside the node. The
operator's counterfactual confirmed the cause: switching the advertised address
from IPv6 to the LAN IPv4 brought the run back to the order of **43 tok/s**.

Post-fix validation closed the counterfactual without depending on the
workaround: the operator restored the original IPv6 address and measured the
inference again, with no regression and the same fast result seen on IPv4.
Through the API, `/api/generate` with the remote head went from **2.2 tok/s**
before the fix to **67.4 tok/s** after, with a **TTFT of 58 ms** and **949 ms**
end to end. That puts the remote path in the same order as the local path,
measured at **95.4 tok/s**.

There are three different facts:

- **Trigger:** an announced address candidate, especially an IPv6 that did not
  accept real traffic, was used to open the TCP connection. The operating
  system waited for the connection timeout before the transport ladder moved
  on. In `remote-stream`, introduced by **`7ace9d5`**, the tail also tried the
  `reply_to` callback list on every batch, always starting at the same address
  and with no failure memory. The result was a repeated wait per batch, seen as
  about 553 ms per token with the engine idle.
- **Masking condition:** the local head did not need to deliver callbacks over
  the remote route, and the IPv4 chosen by the operator was reachable. Also,
  since **`1738c88`** the route task probed the gateway's `/health` and recorded
  the matching inference URL; a healthy gateway could therefore promote an
  inference port that swallowed SYN. `RouteTable::mark_failure` removed the
  choice, but did not keep the failed address: the next resolution could pick
  it again. The screen also only looked at the fields measured on the node and
  the network, so it said the backend dominated even though the end-to-end
  total was much larger.
- **Visible symptom:** 8 `remote-stream` callbacks for 56 tokens, a transport
  switch in the middle, a high TTFT and a generation rate 43 times lower, even
  though `llama_decode` and the node's processing stayed close to the local
  path.

The raw RTT showed that the network explains part of the anomaly, but not the
30 s: over the tailnet, ping between the machines averaged **3.9-4.4 ms** and
`/health` **9-22 ms**; over the direct LAN IPv4, the average was **25.0 ms**,
with a maximum of **91.7 ms**, and the reverse direction to the private IPv4
was not reachable. That jitter is high for two machines on the same LAN and
justifies checking Wi-Fi and power saving, but it cannot produce hundreds of
milliseconds per token when the callback RTT shows as zero: the delay was in the
connection attempt and was not measured by the `remote-stream` trace.

The fix is structural and applies to IPv4 and IPv6:

1. `RouteTable::measure_peer` probes `/health` on the inference port itself,
   never on a parallel gateway port.
2. Real failures are remembered for **5 min** in `RouteTable` and in
   `PeerClients`; a resolution or callback does not pick the failed address
   again while there is an alternative candidate.
3. `PeerClients` caps the TCP open at **2 s**, without reducing the processing
   timeout of a long request. Result/callback delivery has a ceiling of
   **500 ms** on intermediate candidates and **30 s** on the last one, and tries
   the next URL in the same call. Three consecutive failures move a URL to the
   end of the order for 5 min.
4. `transport/link-v1`, introduced by **`d7b4e6e`**, helps after a failed
   opening, since its 5-min quarantine avoids reopening the same URL. Before
   this fix it only covered the channel's key, not the other candidates, and
   opening could still get stuck on the connection timeout. Now the channel
   tries the announced URLs, caps the opening and shares the failure memory.
   The quarantine stops being a hidden wait per token, without being treated as
   the cause of the repeated callback.

The test `a_blackholed_reply_address_is_tried_only_once_across_stream_batches`
reproduces a target that accepts the SYN and does not answer HTTP: the first
delivery records the failure and uses the healthy alternative; the second
delivery no longer touches the dead target. The equivalent `relay` and `link`
tests check the same counterfactual on the direct frame and on the persistent
opening. With the previous implementation, the single-connection assertion
failed because `blackhole_hits` stayed at 2; with the failure memory, it stays
at 1. The ignored bench `the_tunnel_carries_a_real_frame_to_a_live_peer` also
uses `192.0.2.1` (TEST-NET) to exercise an address that swallows SYN and confirm
the opening timeout happens before the fallback. The API has no special path:
`/api/generate`, `/v1/chat/completions` and the UI call
`generate_stream_with_stops`, so a run through the API goes through the same
address selection and gets the same fix.

### 2.3 Copies on the `wire` module's hot path

Framing one crossing (encoding + decoding one `[1, 5120]` f16 item) cost
21.97 µs in release; today it costs 20.94 µs - **-4.7%**
(`framing_cost_per_traversal`, same machine, 2,000 rounds). What
changed:

- `encode_batch_items` writes the tensor **once**, straight into the buffer that
  goes on the wire. The positions come from `tensor_payload_len`, which computes
  them without allocating, and the JSON header is ready before any conversion.
  Before, the same bytes were touched four times: convert, concatenate into the
  payload, clone the `BatchFrame`, copy to the output.
- Conversion goes through a 1 KiB stack block. Growing the vector value by
  value costs more than the conversion itself - measured at almost twice the
  whole framing, and that is why the first version of this change came out
  **slower** than the code it replaced.
- Decoding **borrows** the payload from the received buffer instead of copying
  it: `split_frame` separates header and payload without `to_vec`, and
  `decode_batch_items`, `decode_batch_single` and `decode_inference_request`
  build on that. The only tensor copy left is the expansion to f32 that
  inference consumes.
- `peek_batch_header` and `validate_batch_bytes` only read the header. Whoever
  only needs the route, the `batch_id`, how many items come in the frame, or
  whether the bytes are a valid frame, no longer copies the whole hidden state
  to find out. There were four such points on the hot path: the two transport
  validations in `relay.rs` and the "is this a batch?" test at the entry of
  `handle_step` and `handle_prefill`.

The `frame/v1-batch` envelope and the header structure stay the same; f8 and
the i4 formats are optional dtypes identified by the `dtype` field and the
matching capability.

## 3. Prefill and step

Before, the prompt crossed in a single block: memory scaled with the prompt,
everything had to fit in `n_batch`, and a multi-gigabyte body died on an
internet link. Today the prompt advances in **chunks** (`prefill.rs`): each
chunk is decoded at its own position and the hidden state travels alone.

- **Retry and memory unit**: `chunk_size` (default
  `DEFAULT_CHUNK_TOKENS = 512`). In practice the chunk is
  `min(prefill_chunk_tokens, n_batch)` (`prompt_chunk_of`), because a chunk is
  decoded as one batch on the peer and anything larger than `n_batch` would
  abort llama.cpp just like the whole prompt used to.
- **Adaptive chunk**: the gateway models every hop and every link into a hop
  as a stage whose call costs `fixed + per_token * chunk`, fitted per model and
  route by decaying least squares over finished prefill traces (two points per
  hop and trace: the mean call and the median, full-chunk call; a link's fixed
  part is its network RTT). For the next request it simulates the chunked
  pipeline with the expected new tokens (prompt bytes / 4 minus the prefix it
  will reuse) and `prefill_max_inflight_chunks`, and picks the chunk size, in
  steps of 16 from 32, whose last chunk finishes first; when one chunk holding
  the whole prompt wins it sends the ceiling. Capped by `prefill_chunk_tokens`
  and `n_batch`. A route without samples uses the ceiling; the choice makes no
  probe and adds no wait on the request path. The fixed part is what the
  earlier bandwidth-delay heuristic missed: on a LAN with a ~0 ms RTT it cut a
  199-token decision into five 48-token chunks and paid the per-call overhead
  (GPU sync, frame encode, ~20-30 ms per hop) five times.
- **Transport failure in prefill**: a chunk can be tried up to three times
  (`relay::CHUNK_ATTEMPTS = 3`) with a backoff of 100 ms and 200 ms, but only
  when every path proves the body was not delivered. If earlier chunks were
  already accepted, the gateway resumes from the same index and absolute
  position, keeping the confirmed KV. A timeout or failure after sending can
  leave the peer ahead; in that case it does not blindly repeat the chunk and
  rebuilds the session cold once, recording the reason. If that rebuild fails,
  the generation ends cleanly and releases the sessions reached. A missing
  `start_pos` on later chunks is a protocol error, never
  `chunk_index * chunk_size`.
- **Step does not retry**: decode has no KV rollback protection, so `step`
  always goes with `attempts = 1`. Prefill and decode do not share the constant
  so it does not drift.
- Only the head tokenizes (it has the prompt), and it is the only authority on
  `more_chunks`/`n_prompt_tokens`. The gateway always sends
  `is_final_chunk: false` and whoever tokenized corrects it (see
  `prompt_progress` in section 4).
- When the head still needs another chunk, the partial answer also publishes
  the absolute `n_prompt_tokens`, `start_pos` and `next_pos`. That applies both
  to the individual path and to the batch, including the first chunk of a whole
  model, so the gateway and the following hops do not fabricate the position
  from the chunk index.
- Closing the stream creates a cooperative cancellation per generation. The pump
  watches the HTTP `Sender`'s close even while keeping a clone for the turn's
  bookkeeping, and `prefill::Cancellation` interrupts the in-flight HTTP wait and
  stops the next retry, hop or chunk. An already accepted callback frame does not
  wait for an ACK on the critical path; its asynchronous ACK tells the
  gateway's `410 Gone` from a transport failure, and the first ends the remote
  producer. After that the gateway releases the sessions of the peers reached.

**Prefill pipeline (H1).** On the hub and direct reply paths, when there is
more than one hop, prefill runs as an ordered pipeline, not as a locked pair.
Two concurrent routines in the same gateway task (`prefill.rs`,
`run`/`run_direct`): the **head pump** dispatches the chunks in series - each
one is a network leg + the head's decode - and the **downstream pump** carries
each hidden state through the remaining hops, keeping up to `W` chunks in flight
at the same time and collecting the results in send order (`FuturesOrdered`).
That is what keeps every link and every GPU busy in parallel, instead of a
single stage.

`PrefillOverlapWindow` enforces two limits: the **count** of chunks in flight
(the deployment's `prefill_max_inflight_chunks`, effective between 1 and 8,
default 8; 1 turns overlap off) and a **byte budget** of hidden state in RAM on
the gateway (`INFLIGHT_BYTE_BUDGET`, a fixed 512 MiB, which only bites with a
large `n_embd` or a high window). Each limit's permit travels with the chunk and
is only released when its downstream ends. A failure or the client's
disconnection cancels the in-flight futures; in direct mode, a UUID sequence and
a per-peer queue (`DirectForwardSequence`) keep the hop-by-hop forwarding in
order. The head's ACK carries the absolute `next_pos` inside `prompt_progress`,
and that is where the head pump dispatches the next chunk without waiting for
the previous one's result. The position is never inferred from completion or
arrival order.

**Resume.** An error after chunk `k > 0` carries
`PrefillResumable { from_chunk, start_pos, ... }` as a diagnosis of the point
where a resume could be safe if the remote state were known. A lost transport
answer does not give that certainty: the gateway does not re-enter at `k`,
because that could ask for a `kv_remove` in the middle of the KV. It does a
single cold rebuild from chunk 0, which resets the whole session on every hop.
A session lost on the peer follows the same rebuild; if it fails, the session
ends cleanly and the gateway releases the hops reached. Classifying an unknown
session walks the whole error chain, because partial telemetry may wrap the
direct refusal and hide its message at the outer level.

In ring mode the overlap is done by the hops, not by the gateway. With
`prefill_max_inflight_chunks > 1`, `run_ring` marks `ring_overlap` on the
request; each forwarding hop answers an **intermediate** chunk right after its
own compute and carries it to the next hop in the background
(`infer_server/prefill.rs`, one forwarding slot per session). The next chunk
only moves on after the previous forward finished, so the next hop receives the
chunks in order; this hop's compute on chunk `k+1` runs while the next one
processes `k`. The early answer carries the fields only the head knows
(`next_pos`, tokens, prefix) and the telemetry of the following hops on the
previous chunk. The last chunk stays bubbled and synchronous, and only goes out
after all the previous ones were delivered. A failure in a background forward
comes back on the next chunk as a 502, which the gateway treats as a cold
rebuild, never as a resume. The gateway still sends one chunk at a time: the
trace publishes `prefill_overlap_enabled` and depth 2 (one chunk in compute, one
being forwarded). Before, the ring ignored the window, and a route the planner
sent through the ring lost the whole overlap: measured on 2026-09-29 (Qwen3.8,
between two CUDA machines), each chunk paid `head + network + tail` in series,
~330 ms.

H1 brings prefill close to the transmission-bound regime: with `K` chunks and a
full pipeline, the time tends to `K · max(leg, compute)` plus the fill, instead
of `K · (legs + compute)`. It **does not reduce the bytes**: the 126 MB of a
30,000-token prompt still cross every boundary. The trace publishes
`prefill_pipeline_depth_max`, `prefill_overlapped_chunks` and
`prefill_max_inflight_chunks`.

### 3.1 Prefix cache between turns

The gateway keeps a global `PrefixCache` with per-session leases. The key
combines model and hop sequence, and the cache only keeps text, session identity
and token count, never weights or tensors. A hit entry stays busy during
generation; idle entries follow LRU within the configured capacity. Busy leases
are not evicted, even if the total temporarily exceeds the nominal capacity. On
a miss, an idle entry can be recycled before the new prefill, which keeps the
path compatible with `max_sessions=1` without sharing a busy entry. The trace
reports `gateway_prefix_miss_reason`: `cache-empty`, `matching-session-busy`,
`prompt-prefix-changed` or `route-changed`. That explains the gateway's local
decision; `cached_tokens` stays the count executed and confirmed by the head.

**Generation does not happen on the prompt's sequence.** Each session of a peer
has two KV sequences: `prompt_seq`, where the prefill only writes up to the
stable boundary measured for the next turn, and `gen_seq`, opened before
decoding the first chunk from that boundary as a copy of the first one
(`llama_memory_seq_cp`, metadata only - both share the same cells with
`kv_unified`). The template's tail, the tokens that complete the current prompt
and the generation live in the copy. At the end of the turn the **whole**
generation sequence goes away, which is a total removal every engine accepts,
while `prompt_seq` keeps the prefix the next prompt can reuse. That is why reuse
does not depend on the model accepting the removal of positions from the middle
of the cache - an operation `llama_memory_recurrent::seq_rm` refuses whenever
the rollback goes past the snapshot window (`n_rs_seq`), that is, on any
real-size generation in a hybrid/recurrent model such as Qwen3-Next/Ornith. The
declared cost is a second sequence per session (`llm::kv_seq_budget`), which
`fit_memory` already accounts for.

Retaining is the peer's local act: on EOS the session is **not** destroyed, it
goes back to `prompt_len`. `/v1/session/rewind` checks that state and covers the
stops that do not go through EOS (token limit, stop sequence, client that hung
up); it is idempotent and rolls nothing back. Releasing for good stays an
explicit act of the gateway, through `/v1/session/release`.

`prompt_cells` counts physical cells written in `prompt_seq`, not the total of
tokens already processed in `n_past`: after the stable boundary, the prefill
chunks are written to `gen_seq`. Adding those chunks to the retained sequence's
counter made `retain_prompt` try to prune positions that only existed in the
generation copy. In recurrent/hybrid models the pruning could be refused; the
session stayed retained, but `positions_match_cells` blocked the next reuse
even though the stable prefix was still intact. Now the counter follows each
decode's real target. `retain_prompt` erases the copy and accepts the prefix
directly when the physical count is already at the stable boundary; it only
asks for a suffix removal when there really are extra cells in `prompt_seq`. So
`prompt_cells`, `prompt_tokens` and `n_past` represent the retained prefix
before the next turn.

The logs `prefix gateway`, `prefix peer eligibility`, `prefix peer comparison`,
`prefix peer retention` and `prefix rewind` show the decision chain and the
result per session/hop without logging the prompt or tool content. The token
comparison shows up once on the first chunk; the other logs are one step per
request.

The length confirmed on rewind is the stable boundary measured during the
prefill (`retained_tokens`), not necessarily the prompt's tokenized total: the
template's tail and the input that provides the first token's logits stay in
that turn's generation sequence.

On the next turn the first frame carries `reuse_prefix`. The head's rule is
**append only** (`plan_prefix_reuse`): the prefix serves when the retained
tokens are an exact prefix of the new ones and end before them; any other case -
divergence in the middle, a prompt identical to the retained one, a session
without a prompt - restarts the session from scratch, which costs that turn's
reuse and never the request. The answer declares what was **executed**
(`reuse_prefix`, `cached_tokens`) and it is that decision, not the gateway's
request, that the following hops obey: whoever tokenizes is who knows. In a
chain of N hops, the intermediate one only echoes the head's authoritative
decision; recomputing it as `false` would make the next hop reset the session
and receive a hidden state at a positive position. `start_pos` and `next_pos`
are absolute session positions, confirmed by the peer that just executed. On
chunk 0 a position without a field is where the session is - judging it by
`chunk_index * chunk_size` = 0 erased the reused KV one line after using it. On
the following chunks, a missing field is an error and triggers the cold
fallback, instead of fabricating 512 for a session that is at 42.

A partial hit is observed, but not applied as a partial KV. If the first tokens
match and the divergence would require shrinking the retained sequence, the head
reports `prefix_match_tokens` and restarts the session cold; `cached_tokens`
stays zero and `prompt_tokens_transmitted` goes back to the whole prompt. That
avoids a wrong answer on recurrent or hybrid engines that refuse partial
removal. `prefix_reuse_status` distinguishes `full`, `partial-cold`,
`full-cold`, `miss` and `cold`; when a session loss requires the cold repeat,
the gateway publishes `session-lost-cold`.

If a peer loses the session during the prefill - through idle collection, a
process restart, explicit removal or a lost transport answer -, the gateway does
not repeat a later chunk. It acknowledges that the remote state may be at any
position and repeats the whole round from scratch once, even when the request
did not use a prefix. The cost is redoing also the chunks already accepted and
the KV on every hop. If the rebuild fails, the final error says the system tried
to rebuild the context and the request was not executed; before it,
`release_sessions` walks every URL of each hop and releases best effort. Reuse
is an optimization, never a reason for the request to fail.

The prompt the cache recognizes is the text of the canonical rendering
(`openai::conversation_cache_key`), in the same order the head tokenizes. That
matters because `system`, `developer` and tools are moved to the start; keying
by the messages' original order could consider a prefix valid when the real
prompt had changed at the start. `render_prompt` labels every turn, including
the first, so new messages at the end keep the prefix. Requests with images
neither reuse nor record an automatic prefix, because the marker's text does not
identify the image content. Raw unlabeled text stays available in
`/v1/completions` and `POST /api/generate`. On the Anthropic path, when there is
an `output_config.task_budget`, `conversation_cache_key_with_budget` also
prefixes `type`, `total` and `remaining`; changing any value invalidates the KV
because it changes the marker the model sees. Since the marker is final and
changes on every request, it does not match between turns, following the
provider's documented semantics.

### Why the session disappears

The investigation of the real path found no single event that can be attributed
retrospectively to the peer of the occurrence: the 400 error does not carry the
removal's reason. A session's state is an in-memory `HashMap` inside
`InferState`; there is no persistence or restoration after a restart. The
possible removals today are:

- a process restart, which recreates `sessions` empty;
- idle collection in `reap_idle`, after 900 seconds without use, called when
  `alloc_seq` needs a slot;
- capacity pressure: `max_sessions` is 1 by default. Until 2026-09-08
  `alloc_seq` evicted the least used retained session right away; today it
  evicts nothing, and only the scheduler reclaims a retained session, after the
  grace period (see "A one-off request does not evict a hot conversation");
- active removal through `finish`, used by `/v1/session/release`, by the
  cancellation/error cleanup and by the capacity eviction itself.

The session limit does not remove an active session: eviction only selects
retained sessions, and a lack of slots without a victim returns
`seq pool esgotado` ("seq pool exhausted"), not `sessão desconhecida` ("unknown
session"). Idle collection is not a continuous timer either. During a normal
prefill, each accepted chunk updates `last_used`, and a session in progress is
not marked as retained yet. So, for the symptom seen on a later chunk, the code's
evidence points to a process restart or an explicit/concurrent removal during
the prefill; TTL and capacity mainly explain the prefix cache pointing at a
session lost between turns. A new request working right after on the same peer
proves it is serving again, but does not tell a restart from a removal.

#### A one-off request does not evict a hot conversation

A production defect caught on a CUDA server on 2026-09-08, with the operator
using a coding agent against POM. A 35-character request, one message and zero
tools - generating a conversation title - evicted the agent's 62 thousand token
context. The first word went from 2.7 s to 41.1 s and cache reuse dropped from
99% to zero. In the same log: 29 prefix evictions and 10 rewind refusals.

Two mechanisms added up, one at each end:

- **On the gateway**, a `PrefixCache` miss recycled the identity of the least
  used idle session. The small request debuted with the agent conversation's
  `session_id`, and that conversation's follow-up came back with "rewind
  recusado: sessão desconhecida" ("rewind refused: unknown session"). Today a
  miss debuts with its own identity; idle entries stay bounded by LRU on
  `commit`, which is what always bounded the cache's size.
- **On the peer**, `alloc_seq` called `evict_retained` as soon as the
  `seq_pool` emptied, swapping the hot KV for the newly arrived prefill. Today
  admission belongs to the scheduler: `new_session_slots` only counts truly
  free sequences, and a retained session is not capacity. A new session's
  prefill without a slot waits in the FIFO queue, and only after
  `RETAINED_RECLAIM_GRACE` (5 s) does the scheduler reclaim a retained session -
  so no accepted request hangs until the 900 s `reap_idle`.

With `max_sessions = 1` there is still a single sequence: two truly
simultaneous conversations take turns, and the second waits for the grace
period. What stopped happening is the hot conversation being destroyed by a
trivial request that arrived in the middle of the turn. For real concurrency the
operator raises `max_sessions`, which multiplies the KV reservation (see
`total_context_tokens`).

Benches:
`prefix_cache::tests::a_small_concurrent_request_does_not_take_over_a_hot_sessions_identity`,
`scheduler::tests::a_new_prefill_waits_instead_of_evicting_the_retained_session`.

Conclusion: losing the session is possible and, in part, intentional in the
current lifecycle. The `prefix peer eligibility`, `prefix peer retention` and
`prefix rewind` logs show whether the session reached the peer, passed the KV
validation, was retained and whether the gateway confirmed the rewind. They do
not identify retrospectively the removal by `finish`, `reap_idle` or
`evict_retained`; for that it would still be necessary to log the reason of each
removal and correlate the process start with the `node_id`. It is also worth
aligning the gateway's `prefix_cache` capacity with the peers' `max_sessions`.
Automatic recovery is still the right answer for a legitimate loss; the new logs
make the decision observable on the next run.

EOS or end of turn only closes the answer's UTF-8 accumulator. The server keeps
the physical session until it gets `/v1/session/rewind` or
`/v1/session/release`, because the gateway may need to keep the KV for the next
turn. Removing it directly on the terminal path would make the rewind answer
`sessão desconhecida` and leave the answer code indexing a missing session.

The cache is an internal optimization. The binary `frame/v1-batch` envelope and
the public APIs did not change, but the saving is now **published**:
`usage.prompt_tokens_details.cached_tokens` (OpenAI),
`usage.cache_read_input_tokens` (Anthropic) and `x_pom.trace.cached_tokens`
report prompt tokens the pipeline did not reprocess - a measure of what ran,
not of the choice. The trace also publishes `prefix_match_tokens`,
`prefix_reuse_status` and `prompt_tokens_transmitted`, to separate a common
candidate from tokens really saved. `/health` publishes `sessions_retained`,
`prefix_tokens_retained` and `kv_seq_max`.

`prefix_cache_lab.rs` compares each in-memory cache operation with a reference
and holds a deliberately broken proxy that must trip the battery - it validates
the **choice**. `prefix_reuse_lab.rs` validates the **execution**: a four-turn
conversation through the `/v1/chat/completions` path, over the three transports
(hub, ring, direct reply), asserting that from turn 2 on `cached_tokens > 0`,
that the work per turn stops growing with the conversation, that the TTFT drops
relative to the cold turn and that no hop got a partial KV removal request. The
sync contract repeats ring and direct reply with N=3 and N=5 hops, checking on
each peer `start_pos == n_past` before the decode and `next_pos == n_past`
after. The `openai.rs` SSE test reproduces the peer that confirms the session at
42 without `next_pos` and asserts that the fallback neither sends chunk 1 at
position 512 nor delivers a `server_error`. That bench's fake hops imitate the
most restrictive engine the product must serve - the recurrent one that refuses
partial removal; the bench's deterministic delay makes the executed cost
observable, without confusing the cache's choice with the real saving.

### 3.1.1 KV cache snapshots on disk (`slot_save_path`)

The prefix cache above lives **only in memory**: a process restart, idle
collection (`reap_idle`) or capacity pressure (`evict_retained`) erase the
retained KV and the next turn pays a cold prefill (see "Why the session
disappears"). The snapshot directory turns on a mechanism to write and reread
that KV, equivalent to llama-server's `--slot-save-path`. It comes, in this
precedence, from the `POM_SLOT_SAVE_PATH` variable or the `slot_save_path` field
of `pom.json` (`Config::slot_save_path_effective`); the env var is per process
and not persisted. With neither set, it is `~/.pom/kv-slots`
(`config::default_slot_save_dir`), so every peer serves snapshots by default; an
empty value turns the feature off. The value is injected into `NodeConfig` in `apply_locked`
from the installation, like `cache_type_*`.

- **Engine** (`llama-engine`): `LlmEngine::state_seq_save_file` /
  `state_seq_load_file` wrap `llama_state_seq_save_file` /
  `llama_state_seq_load_file`. llama.cpp's `.bin` serializes a sequence's cells
  with their positions and an **advisory** token header (never checked against
  the cells), and on restore validates the KV geometry (layer count, type and
  row size per layer) - a mismatch is `0` bytes, which becomes an error, never
  an abort.
- **Peer** (`infer_server`): `POST /v1/session/slot`
  `{action: save|restore|erase, session_id, slot_name, dest_session_id?}`. The
  gateway reaches each hop (`generation::slot_call_step`) along the same path
  as the session lifecycle: a route pinned to the tunnel goes straight through
  it, otherwise the inference addresses and, on a transport failure, the
  tunnel - a hop only the relay reaches also saves and restores.
  `save` writes **`prompt_seq` truncated at `prompt_len`** - never `gen_seq`,
  the volatile tail does not enter a snapshot - plus a `<name>.json` sidecar
  with the model's identity (`model_label`, layer range, `n_embd`) that
  `restore` checks **before** touching the engine. `restore` creates a new
  `retained` session, with `n_past`/`prompt_len` at the saved boundary; the head
  writes the real tokens in the sidecar, the following hops a vector of the right
  size. The route is also replicated to the TP workers (`replicate_slot_call`),
  which serve the same range and write their own copy. `/health` publishes
  `slot_persist_enabled` and `slot_count`; the capability is
  `inference/slot-persist-v1`, only announced when a directory is configured.
- **Gateway** (`gateway::snapshot_deployment` / `restore_deployment` /
  `erase_snapshot`): a deployment snapshot is all or nothing. The gateway
  resolves the pipeline (`resolve_plan_for_model`, the same planner as
  generation), requires `inference/slot-persist-v1` on every hop and calls
  `/v1/session/slot` on each with the name `"{snapshot}__{node_id}"` (a single
  directory keeps each peer's slice without collisions). A failure on any hop
  erases the ones that already wrote; on `restore`, hops that come back with
  different token boundaries release the new session on all of them and return
  an error. Operator: `GET /admin/slots`,
  `POST /admin/slots/{name}?action=save|restore|erase` (see `http-api.md`).

The directory is flat and shared by every model: nothing is organised per
model on disk. A KV state only fits the model and layer range that wrote it,
and that is enforced per snapshot - the sidecar check above on restore, and the
model plus hop sequence in the name of every automatic snapshot
(`PrefixRouteKey`). Snapshots of a model no longer served stay until the TTL,
the byte budget, or `POST /api/ui/kv-cache/clear` remove them.

Restoring reproduces the same physical state as a live retained session (cells
beyond `prompt_len` may remain in `prompt_seq`, exactly as on the normal path),
so the next turn follows `plan_prefix_reuse`'s append-only rule unchanged.
Recurrent/hybrid engines: the `.bin` restores the whole sequence at once, with
no partial removal, so the mechanism applies to every served model.

#### Persistent prefix cache (automatic)

With `slot_persist_prefix_cache` on (a `pom.json` field or
`POM_SLOT_PERSIST_PREFIX_CACHE`; on by default, a `pom.json` without the field
reads `true`) and `slot_save_path`
configured on **every** hop, the gateway makes the prefix cache survive restarts,
with no manual call:

- **Spill.** At the end of each turn, after `rewind_sessions` retains the
  prompt, the gateway writes an automatic snapshot in a detached task (off the
  answer's path): `snapshot_deployment` with the name `auto__<key_hash>`, where
  `key_hash = hash(PrefixRouteKey + cache_key)`. A small index
  (`prefix-auto-index.json`, next to the `.bin` files in this gateway's
  `slot_save_path`) keeps `(route, prompt, tokens, bytes, updated_at)` per
  `key_hash`.
- **Debounce.** If the retained prefix grew less than
  `SLOT_AUTO_MIN_GROWTH_TOKENS` (256) since the last snapshot of the same
  lineage, the turn only renews the ancestor's stamp and does **not** rewrite -
  each spill is a KV serialization plus a fan-out.
- **Supersession.** On write, `PrefixDiskIndex::record` drops every entry of the
  same route whose prompt is a strict prefix of the new one and erases its
  `.bin` files (`erase_snapshot`). A growing conversation keeps **one** `.bin`,
  not one per turn. A conversation that branches loses the intermediate point
  and falls to whatever ancestor is left or to the cold prefill.
- **Reload on miss.** When the in-memory `PrefixCache` does not have the
  conversation (typical right after a restart), the gateway looks in the index
  for the longest **strict** prefix of the requested prompt; if found,
  `restore_deployment` recreates the KV on every hop in a new session, the
  `PrefixCache` adopts that session as a hit (`PrefixCache::adopt`) and the turn
  proceeds as a normal hit - the head reuses through the same append-only rule.
  The trace publishes `prefix_reuse_status = "disk"`.
- **Limits.** `slot_auto_max_bytes` (default 2 GiB, summed across slices) with
  LRU eviction by `updated_at`, and a TTL configurable through
  `slot_auto_ttl_secs` in `pom.json` or `POM_SLOT_AUTO_TTL_SECS` (default 7
  days). `updated_at` is renewed on every write and every restore, so an active
  conversation does not expire. An evicted `.bin` is erased with
  `erase_snapshot`.
- **Boundaries.** The index is **local to the gateway**: if the gateway role
  moves to another machine, the new gateway does not see the entries and falls
  to the cold prefill (orphan `.bin` files leave by TTL/LRU). A `restore` that
  fails (a file vanished on a hop, incompatible geometry) forgets the entry and
  proceeds cold - reuse is an optimization, never a reason for the request to
  fail.

### 3.1.2 Host-memory KV cache (`cache_ram_mib`)

A peer has `max_sessions` live KV sequences (one by default). A retained
session that loses its sequence - the scheduler reclaims it for another
conversation (`evict_retained`), or the idle collector takes it (`reap_idle`) -
used to have its KV wiped, and the conversation's next turn paid a cold
prefill on that peer. With the host-memory cache the peer first copies the
session's `prompt_seq` state to RAM (`LlmEngine::state_seq_get_data`,
`InferState::park_session`) and keeps it under the same `session_id`. This is
llama-server's `--cache-ram`: the parked entries are extra slots that cost RAM
instead of KV context.

- **Return.** On chunk 0 of a prefill that asks for `reuse_prefix`, a session
  that is not live but is parked is copied back into a free sequence
  (`unpark_session`, `state_seq_set_data`) as a retained session, and the turn
  proceeds through the same append-only rule as any hit. The scheduler treats
  the parked session as a new one, so the sequence it needs is admitted the
  usual way. Nothing changes for the gateway, the wire or the other hops.
- **Budget.** `cache_ram_mib` in `pom.json` or `POM_CACHE_RAM`, with the
  llama-server values: a positive ceiling in MiB, `0` for off, absent or `-1`
  for as much as the machine allows. Least recently parked entries leave first.
- **The user comes first.** Whatever the setting, half of the host RAM stays
  **available** to whoever uses the machine: a state is parked only if the host
  still has that half available after the copy, and the cache never holds more
  than the other half. A host whose half is under 6 GiB (less than 12 GiB of
  RAM) parks nothing. Available memory is sampled every 2 s in the background
  (`HostMemory`), so the parking path never waits on the operating system; a
  host not sampled yet parks nothing.
- **What is not parked.** A session that is not retained, has no prompt, holds
  media or M-RoPE cells (same eligibility as a disk snapshot), or belongs to a
  tensor-parallel group, where the ranks keep their sessions in lockstep.
- **What forgets an entry.** `/v1/session/release`, a reset of the same
  session id, `POST /v1/session/cache/clear`, and unloading the model: the
  bytes belong to the loaded context and never outlive it. Surviving a restart
  is the job of the disk snapshots above.
- **Observability.** `/health` publishes `cache_ram`: `enabled`, `limit_bytes`
  (`null` when the host is too small or not measured), `user_reserve_bytes`,
  `bytes`, `entries`, `tokens` and the `parked`/`restored`/`evicted` counters.

The eviction copies the KV out of the context, a multi-gigabyte read for a
long conversation, so `run_scheduler` reclaims through `spawn_blocking`.
Each hop parks and restores on its own; a hop that no longer has the entry
answers as it always did for a lost session and the gateway rebuilds cold.

### 3.2 Executor selection

Before loading the weights, `executor::select` reads the GGUF header. Only an
`expert_count > 1` classifies the model as MoE. In `auto`, any dense model or one
with missing metadata stays on `dense/llama.cpp`. The persisted field
`NodeConfig.inference_mode` reaches the selector before loading; when the field
is not given, the persisted default is `dense` (`llama.cpp`). An explicit
`dense` or `moe` choice beats the brand-derived `POM_INFERENCE_MODE`
environment; in `auto`, that environment can still force a mode and, without an
override, the metadata chooses. `moe` refuses a dense GGUF with an explicit
error.

The current MoE path shows up as `moe/q4_0` in `/health` when the selector picks
that mode and opens its own runtime in `crates/llama-engine/src/moe/`: the
dispatcher detects the GGUF's dtype, computes the top-k router and uses the
global LRU. Received hidden states are transformed before the local decode;
residuals produced from tokens go through the same bridge before moving on to
the next hop. The wire stays f16 and no weight is transported. Attention,
remaining operations and final sampling still delegate to `LlmEngine`, so this
is a partial wiring of the block, not a full MoE transformer. Q4_0, NVFP4,
K-quants and IQ-quants use their own loaders with validation of the GGUF order
`[input, output, experts]` before the forward. With the `cuda` feature, Q4_0
uses the heterogeneous CUDA bridge and the resident cache partitioned by
`CUDA0`, `CUDA1`, ...; each shard gets the experts through a stable placement
and the route joins the cards' partials in parallel. Without CUDA, the CPU
decoder is an explicit choice in telemetry and in the startup log. The CUDA path
stays reserved for dtypes with a validated bridge, and there is no silent native
fallback when the external MoE selection fails.

`path/engine mismatch` in the UI is a real telemetry label: `selected_path`
records the chosen path (`moe/q4_0` or `dense/llama.cpp`) and `executed_engine`
records the executor that actually ran. The flag shows up when they diverge,
for example when the selected MoE path falls back to `llama.cpp`; it is not a
symptom of the remote transport or the network. A complete lack of samples
keeps the engine unknown and is not called a divergence.

Before the events reach the HTTP adapters, the gateway also applies per-request
stop strings. `StopMatcher` keeps the suffix that may still be the prefix of a
stop, removes the stop itself from the output and ends the generation with
`finish_reason=stop` and the matching sequence. Without stop strings the path
keeps emitting the events as before.

### 3.3 Local scheduler and continuous batching

Each inference process has its own scheduler for the `llama.cpp` context. It
receives the frames after authentication and wire decoding, applies
backpressure before creating a session and answers `429` with
`Retry-After: 1` when `max_queue_requests` is reached. The default is a queue of
128, a 0 ms collection window, `max_batch_sequences` capped by `max_sessions`
and `max_batch_tokens` capped by `n_batch`. A positive window stays available in
the configuration for workloads that prioritize forming microbatches.

In each hop's telemetry, `queue_ms` starts at the `queued_at` marked by the
handler right before `SchedulerHandle::submit_prefill` or `submit_step`, and
ends at the worker's entry, before acquiring the state and engine mutex. The
fixed delay used to come from `tokio::time::sleep(cfg.batch_window)` in
`run_scheduler` when the queue was not full yet. `lock_wait_ms` starts after
that and measures the mutex wait separately, so it must not be used to explain a
uniform queue across different GPUs. With the 0 ms default there is no timed
wait; a positive value keeps exactly the opt-in window. The relay also had a
fixed 4 ms `sleep` outside that metric, now replaced by an immediate
asynchronous flush.

When the entries are compatible, the worker builds a single `llama_batch` with
one physical sequence per session. Prefill and token decode live in one lane;
hidden states in another. A session shows up at most once per microbatch, and
`BatchLayout` uses `item_id` and the global row range to tie each residual and
logits position to the right answer. Speculation, rollback and Tensor
Parallelism stay on the individual path until they have batch contracts of
their own.

The scheduler is local to the peer. Two replicas serving the same range have
independent queues and KV; routing must pick one replica and keep that affinity
during the session. Two ranks of the same TP group are another unit: they need
to run the same batch coordinated by the TP barrier, which is why multi-sequence
execution is still disabled in the current TP.

### 3.4 Node telemetry

The inference server accumulates its internal timers in microseconds in
`NodeTimings`. When building the `node_timings` metadata, the public `*_ms`
fields use fractional milliseconds (`us / 1000.0`) so a step shorter than 1 ms
is not lost. `backend_call_ms` only measures llama.cpp's evaluation call;
`queue_ms`, `scheduler_wait_ms` and `lock_wait_ms` describe waits before it. The
call may include host dispatch and is not a pure measure of GPU kernels.
`backend_sync_ms` separately measures `llama_synchronize`, which can wait for
device work already queued and output transfers. D2H capture, logits, sampling
and the block's other boundaries stay separate. In residual capture,
`residual_capture_ready_wait_ms` is the wall time from the scheduler's query
callback to the readiness callback. It includes the graph evaluation and the
sync done by the scheduler; it is not idle waiting. The
`ggml_backend_tensor_get` call is published as `residual_capture_tensor_get_ms`
and includes the API's wall time. The CUDA backend may do `cudaMemcpyAsync`
followed by `cudaStreamSynchronize` inside that call, so the field does not
separate DMA from sync. `residual_capture_postprocess_ms` measures arranging and
recording the tensor. `residual_capture_copy_ms` stays as a compatibility alias,
and `residual_capture_d2h_sync_ms` is the sum of ask-to-ready and `tensor_get`,
not a measure of an isolated D2H copy. `backend_sync_ms` stays the explicit
`llama_synchronize` barrier, measured apart.

In the decode's hidden-state answer, `residual_wire_f8_conversion_ms` measures
the `f32_to_f8_bits` loop when f8 is the selected dtype and is zero for the
other dtypes. It does not include building the buffer. After it,
`residual_wire_frame_serialize_ms` measures validation, the header JSON,
allocation and frame assembly. They are sequential intervals inside the
encoder, with no double counting. `residual_wire_encode_calls` and the
capture/byte counters let you check the denominator used in the aggregate. The
profiled encoder's equivalence is tested against the f8 and f16 bytes of the
normal path. `logits_clone_ms` stays zero: the tail only keeps the adjusted
logits of the repeated ids. `repetition_penalty_ms` measures building those
overrides; `top_k_ms` measures selection and refinement when `top_k` reduces the
vocabulary. With `top_k` disabled, there is no temporary id vector: walking the
distribution goes into `softmax_ms`.

`POM_PROFILE_SYNC` is on by default and controls whether the diagnostic barrier
runs. `backend_sync_ms` is a duration and must not be used alone to deduce the
peer's configuration state.

### 3.5 Conversation template and completions

The gateway tells the request format apart before starting the prefill. OpenAI
Chat (`/v1/chat/completions`), Anthropic (`/v1/messages`) and
`POST /api/generate` send `chat_template: true` in the first prefill's
metadata. The head, the only owner of the tokenizer, applies the GGUF's
`tokenizer.chat_template` once and opens the assistant's turn; the other peers
only get the hidden state and do not reformat the prompt. OpenAI Chat also sends
`thinking_enabled` per inference in that same metadata. The gateway normalizes
its absence to `true`; on the head, the request only turns thinking on when
`thinking_enabled` is true and `NodeConfig.no_thinking` is false. The field is
copied into the metadata of the existing relays, so hub, ring and direct reply
keep the decision without creating another channel. For Gemma4, the renderer
reads the template from the GGUF and applies `enable_thinking` and
`add_generation_prompt` in the flow the model supports, without inserting
markers of another format. The tail ends on EOS or on the end-of-turn token from
the same GGUF's vocabulary, without sending that marker to the client.

`/v1/completions` stays a raw completion and sends `chat_template: false`. That
explicit flag takes precedence over the server's legacy default, which existed
for internal calls without metadata. `no_thinking` only controls opening the
reasoning block and no longer decides whether the prompt is a conversation. So
thinking mode and conversation framing cannot be confused. The gateway's prefix
key includes the value of `thinking_enabled`, preventing a prefix formatted with
the opposite mode from being reused.

### 3.6 Completion sampling

The tail prepares the distribution in `LlmEngine` in a single pass without
copying the full logits vector: it keeps sorted overrides only for the valid,
distinct ids of the final `repeat_last_n` window. It returns argmax when the
temperature is not positive. With a positive temperature and `top_k` active, it
selects the `K + R` largest raw logits, where `R` is the number of overrides,
joins every adjusted id and refines the top-k by the final score. That set
contains exactly the K winners even when the penalty raises logits. The index
scratch uses `u32` when the vocabulary fits that type and keeps a `usize`
fallback for larger slices. When `top_k` does not restrict the vocabulary, the
tail walks logits and ids directly, without creating another id vector. It
computes a stable softmax only over the survivors, normalizes, sorts by
probability in the vector itself and keeps the smallest prefix whose mass
reaches `top_p`; after renormalizing that nucleus, it does the multinomial draw
with the `Mutex`-protected LCG.

The overrides of the window carry three penalties, applied in llama.cpp's
order: `repeat_penalty` scales the logit (divides a positive one, multiplies a
negative one), then `frequency_penalty` is subtracted once per occurrence of
the token in the window and `presence_penalty` once per distinct token. All
three are off at 1, 0 and 0, and with all off no override is built. `min_p`,
when positive, runs after the top-p nucleus: candidates below that fraction of
the most likely one are dropped and the rest renormalized.

The deployment forms show, in each property's description, the name llama.cpp
gives it (`apps/frontend/src/lib/llamaCppNames.ts`); a property without one is
POM's own. Sampler settings of llama.cpp that POM's sampler does not implement
are absent from the forms: `typical_p`, `top_n_sigma`, XTC, mirostat and
dynamic temperature.

`reasoning_budget` is llama.cpp's `--reasoning-budget`: the tokens a thinking
block may take, -1 for no limit (the default) and 0 to close it at once. The
machine that samples resolves the tokens of `<think>` and `</think>` when the
deployment is loaded. Before sampling, `forced_reasoning_token` looks at the
tokens generated in this request: if a thinking block is open and has reached
the budget, the end tag is returned instead of a sampled token, one token per
step until the tag is whole, and the answer then goes on freely. A block is
open when a start tag was generated after the last end tag, or when the request
started inside one: thinking is on for it and the model's template ends the
prompt with the start tag (`thinking_forced_open`, checked once by rendering
the template), which each prefill records per session (`note_thinking`),
because the sampling machine never sees the prompt's tokens. llama.cpp's
`--reasoning-budget-message` has no counterpart. A model whose thinking tags
are not `<think>` and `</think>` is not limited.

DRY (`dry_multiplier`, `dry_base`, `dry_allowed_length`, `dry_penalty_last_n`)
is llama.cpp's sampler against repeated runs, ported to the same sparse
overrides. For the tokens so far, it finds every earlier place where the
current ending already appeared; the token that followed it there would extend
a run of `match` tokens and loses `multiplier * base^(match - allowed_length)`
from its logit once `match >= allowed_length`. A run does not reach across a
sequence breaker: the tokens this model has for llama.cpp's default breakers
(newline, `:`, `"`, `*`), resolved when the deployment is loaded, so the frame
of a list or a table is not taken for a repetition. Runs are counted up to 50
tokens and searched in the last `dry_penalty_last_n` tokens (0 is the whole
history, capped at 8192). `dry_multiplier = 0`, the default, turns it off.

`n_ubatch` is the physical micro-batch of a context under the memory fit:
`min(n_batch, n_ubatch)`, and at most 128 on the main context of an external
MoE executor. Its default, 512, is the value that was a constant before it
became a deployment setting. With the fit off the micro-batch stays equal to
`n_batch`. A smaller value shrinks the compute buffers in VRAM, including those
of the MTP context.

`seed` is where the sampler's generator starts. It is applied once, when the
deployment is loaded (`set_sampler_seed`), and the sequence then runs on across
requests; it is not restarted for each one, unlike llama.cpp's per-request
seed. The default, `DEFAULT_SAMPLER_SEED` (9600629759793949339), is the fixed
state POM has always started from. It travels as text in JSON because a browser
cannot hold every 64-bit integer in a number.

Repetition is handled by the sampler only: the penalties and DRY. An earlier gateway-side
detector that stopped a generation after repeated windows of output
(`finish_reason: "cycle"`, `--cycle-detection`, the `cycle_detection_*`
deployment fields) was removed: it was not a setting any LLM engine has, and
old deployments that still carry those fields load with them ignored.

Ties at the `top_k` cut are broken by the original id and do not grow the set
beyond K. The top-p cut also keeps only the minimal prefix, even with ties, so
the chosen mass is observable and deterministic before the draw.
`repeat_last_n=64` stays the default: the penalty is unigram, not a detector of
n-gram or whole-block cycles, so widening the window would not guarantee
preventing the observed cycle and would change the sampling policy.

`sampler_profile_regressions::sampler_cost_profiles_on_a_qwen_vocabulary` is an
ignored benchmark that compares the current path with the previous
implementation, on the same synthetic input of 151,936 logits with ties. It
measures `top_k` on, the deployed defaults (`temperature=0.7`, `top_p=0.9`,
`top_k=40`, penalty 1.15 over 64 tokens) and `top_k` off, and requires the same
candidate distribution. Run it with
`cargo test -p llama-engine --release sampler_cost_profiles_on_a_qwen_vocabulary -- --ignored --nocapture`;
over 100 rounds on the measuring host, preparing candidates for the deployed
defaults went from about 237 us to 203 us per token and the top-k stage from
227 us to 200 us; with `top_k` off, time went from 3.03 ms to 2.76 ms. The input
is synthetic and times vary per host, so they are not a universal latency
target.

In TP, `handle_prefill` and `handle_step` also serialize the path of a frame with
a single item using `tp_batch_gate`. The gateway may send that frame through the
`BatchCoordinator`, and another request must not replace the global decode while
the ranks are still waiting for the collective. The fan-out children of an
already protected batch use `BATCH_CHILD_HEADER` so they do not try to acquire
the same lock again. When re-encoding a single item's answer, the handlers keep
`item_id` and `batch_id`; without them the coordinator cannot tie the answer to
the waiting request.

### 3.8 Sub-range executed per hop

`PipelineStep` separates the range the peer loaded (`start_layer`/`end_layer`)
from the range that step must execute (`exec_start`/`exec_end`). The planner
fills the second with the coverage still missing from the current position.
Those two fields travel in the `meta` of each frame item, both in prefill and in
`/v1/step` decode; the `frame/v1-batch` envelope does not change. Without the
fields, the receiver keeps executing the whole slice.

The handler checks the sub-range against the loaded range before queueing the
work. When `exec_start` matches `start_layer`, an `exec_end` smaller than
`end_layer` moves the residual capture to the matching local block. The
callback copies the residual before the following operations and lets the graph
continue; so a required head can run after the capture without changing the
hidden state sent to the next peer. In DeepSeek4, the graph calls that boundary
`l_last-N`; the `l_out-N` alias is still accepted by POM. When the request goes
up to `end_layer`, the normal capture of the whole slice is used. An inverted
sub-range or one outside the slice gets HTTP 400; starting above `start_layer`
is also refused explicitly, because the current engine always enters through
the slice's first block and cannot skip the head already loaded.

When `llama_model_n_embd_out` differs from `llama_model_n_embd`, the context
does not turn on `cparams.embeddings`. llama.cpp's generic extraction computes
the read with the metadata's output width, which can be larger than the tensor
materialized in the graph and cause an out-of-bounds read `GGML_ASSERT`. The
`l_out`/`l_last` callback stays the only capture path POM uses.

The input path keeps the residual's shape up to `llama_decode`: receipt
validates the shape, the relay keeps the axes, and the Rust `llama_batch` is
allocated with the effective width. The fork published in `pom-tensor-parallel`
also carries that explicit width through the allocator and the graph input. For
an expanded input, the normal graph uses the physical width reported by
`n_embd_out` and reinterprets it directly as `[n_embd, streams, tokens]`,
without repeating the first range. For the canonical input, the allocator and
the graph keep using `n_embd_inp`; a width that is neither canonical nor the
reported expanded one is refused with the geometry in the error.

That is why the planner only picks a range that starts exactly at the executed
position. Overlapping ranges in the inventory can form alternative entries, but
each path of a request advances through contiguous sub-ranges. When there are
equivalent replicas at the entry, the gateway picks the one with the lowest RTT
measured from the node that received the request and keeps that choice for the
session.

The ring relay and the direct reply pick the next `PipelineStep`'s range when
building the next frame. A microbatch with different capture limits is refused,
since a single graph cannot stop at two blocks. That partial execution is a
capability of the current binary, not a fallback for old peers.

`prefill::BatchRoute` is the compatibility key for the transport's next step:
model, deployment, phase, relevant configuration and next hop must match for two
items to share a frame. The effective choice of replicas is recorded by
`pipeline::RouteAffinity`, which rejects a different route in the middle of the
same session.

The `/health` endpoint exposes `scheduler.queue_depth`, the effective limits,
the size and tokens of the last batch, plus the accumulated counters. That makes
it possible to tell local queueing from network time and decode time.

### 3.9 Typed decisions (`probe`)

`POST /v1/systemone` (`backend/decisions.rs`) runs each typed question as one
generation with `max_tokens=1` and a `prefill::DecisionProbe` in
`GenerationInput::probe`. The gateway puts the labels in the head's metadata
(`probe`) and every hop forwards them; the last peer, before sampling, reads
the full-vocabulary softmax of the first token of each label (with and without
a leading space, `llm::probe_candidates`) and answers `probe_result`
(`labels`, `probabilities`, `mass`, `probe_ms`). `mass` is the share of the
distribution that fell on the labels; `probe_ms` is that peer's forward pass
plus the read. Probe jobs are never batched with other sessions.

**Prompt order and prefix reuse.** The rendered prompt is: fixed system turn,
then the question head (instructions and answer format), then the state as
compact JSON or text, then the options. Everything before the state is the
same for every request that asks this question, so `decisions.rs` uses that
text as the prefix-cache key and sends its byte length to the head as
`retain_prefix_bytes`. The head bounds the stable boundary of section 3.1 by
it (`prefix::bounded_stable_len`, the same token-common-prefix measurement as
`stable_prefix_len`, cut before the state), so the retained `prompt_seq` holds
only the question and the next decision with the same question and a new
state is an append-only hit. Keying by the full prompt, as chat does, could
never hit: the state changes on every call. Questions of one request share
only the system turn; they are not batched against each other's state. The
ignored test `decision_prefix_tests` (run with `POM_JEV_TEST_MODEL=<gguf>`)
checks this with a real tokenizer and chat template: with Qwen2.5-0.5B the
head retains 64 of 112 prompt tokens and a new state reuses them, while the
unbounded boundary reuses nothing.

**Chunking.** Decisions use the same chunked prefill and in-flight window as
chat (`prefill_chunk_tokens`, `prefill_max_inflight_chunks`). When a hop runs
over a slow route (relay or libp2p tunnel), the hidden state of the prompt
dominates the time to the answer; chunking overlaps compute with transfer but
does not reduce the bytes. What reduces them is prefix reuse, a compact state
and the wire dtype.

**Trace.** On success the gateway copies `probe_result` to `RequestTrace::probe`
and builds `RequestTrace::jev` (`DecisionProbe::trace_summary`): `enabled`,
`question`, `question_type`, `candidates`, `labels`, `options`, normalized
`probabilities` per label, `selected`, `selected_option`, `confidence`
(normalized top probability), `mass`, `probe_ms`, `probe_tokens`,
`prefill_ms` and `cache_hit_tokens`. `refresh_telemetry` expects no decode
spans for such a trace. A decision always stops at its single token with
`finish_reason: "length"`; the request log records that as `ok`, not
`incomplete` (`audit_status_for_summary`).

## 4. Ring mode vs direct reply

**Ring** (`relay/v1-chain`): a stack of nested HTTP calls. Each hop holds its
own answer open while it calls the next, and the result comes back retracing
the way out. That is **2N network crossings per token** in a chain of N hops,
and the math gets worse when the head does not live on the machine that received
the request: measured on 2026-08-14 between two sites (RTT 30 ms), 25 tok/s with
the head local against 14 tok/s with it remote - exactly one extra RTT per
token.

**Direct reply** (`relay/v2-direct`, `inference/direct.rs`): the chain does not
come back. The gateway sends with the frame a `reply_to` - the addresses through
which it itself is reachable, plus a **ticket** (a UUID v4 drawn per frame).
Each hop forwards `reply_to` intact; whoever finishes (the tail, or the hop that
failed to forward) makes **one** POST to `/v1/relay-result` straight to the
gateway. That is **N+1 crossings**, for any N and any entry point. What each hop
returns to its caller is only an immediate acceptance (`direct_ack`,
`{"accepted": true}` wire-encoded), off the critical path - the gateway is the
one waiting, on the ticket.

- `ReplyInbox`/`Ticket`: a ticket -> oneshot channel map that lives in
  `GatewayState`. `Ticket`'s `Drop` removes a not yet delivered ticket on
  **any** exit path (error, expired deadline). When crossings are expected,
  `deliver` releases the result and only keeps the snapshot for 30 s for late
  receipts; after that the unknown ticket is **410 Gone**, and since the ticket
  is unique per frame it can never be confused with the next frame's.
- `DirectReply::new` returns `None` when the node knows no address of itself -
  then the ring takes over instead of dispatching a frame that never comes back.
  The addresses it may use come from `self_reply_hosts()`: `self_dial_host`
  first (advertised address > local IP > loopback), then `self_addr_candidates`,
  turned into URLs by `self_reply_urls(hosts, gateway_port)`.
- `await_result_with_ack`: a biased `select!` between the inbox's result (the
  normal case), the head's ack (only relevant when it fails, or when an old peer
  bubbled the whole answer - a present `token_id` counts as the result), and the
  frame deadline the caller computes with `relay::frame_deadline` (a hop died
  after accepting). If the result wins, the ack continues in a detached task so
  the first hop's transport evidence is not lost.
- `deliver_result`: the tail tries each `reply_to` URL in order; intermediate
  candidates get `DELIVERY_TIMEOUT = 500 ms` and the last one 30 s. Three
  consecutive failures move the URL to the end for 5 min. `410` stops trying
  (the inbox no longer exists); if all fail, the gateway closes on the deadline -
  there is no third path. `deliver_failure` delivers a `RelayFailure` in place
  of the result, so the gateway names the node that died instead of waiting for
  the whole deadline.
- `forward_direct`: forwarding runs in its own task (the caller was already
  accepted), but the prefill sequence waits for the predecessor `chunk_index`
  before transmitting. So computing the next chunk can happen in parallel
  without letting a later hidden state be applied first. If forwarding fails, the
  hop itself reports straight to the gateway naming the node that really died.
- A failure that arrives with `error` in place of the token (`direct_failure`)
  names the dead hop and marks **that one** (not the head) with
  `routes.mark_failure`.

**Batched remote transport and the cause of the per-token crossing.** The
investigation confirmed the per-token crossing in `step_ring`:
`generate_stream_inner` awaited each `prefill::step`, and `step_ring` made a
`/v1/step` to the head before asking for the next token. On the tunnel,
`send_frame_via_tunnel_with_auth` measures the whole answer and fills
`response_headers_ms` with the same `round_trip_ms`. So `gateway: to headers`
is the time to the tunnel's ACK/answer, not the time the gateway spent writing
SSE to the frontend. The gateway code already had a separate pump for the
client and a bounded channel, so the hypothesis that SSE writing caused the
774 ms was not confirmed.

The new transport sends a `/v1/step-stream` control frame once to the remote
head. The peer's producer calls the same `handle_step` and keeps inferring; a
separate emitter drains at most eight tokens already in the queue and makes one
POST per batch to `/v1/remote-stream-result`, with no sleep or timer. The
callback queue has capacity 64: a slow client blocks the emitter when it fills
up and then the producer, instead of letting memory grow without bound. In
sliced plans, the head keeps the other hops inside the network. The trace counts
`remote-stream` per callback, records bytes and adds up the node times; the
control crossing shows up separately. The wait breakdown does not add
overlapping intervals: `producer_wait` is the cycle until production,
`channel_send_wait` is the `Sender` reservation, `queue_wait` is the time from
the ready item to the emitter and `delivery_ms` is the previous callback.
`node_timings.mtp_draft_ms` makes the synchronous `nextn` visible; so
`5108 ms / 59` with channel and queue close to zero is production cadence, not
network. The content still comes from the existing sampler and UTF-8 assembler,
in the same order.

**`prompt_progress`**: with no way back, the head can no longer overlay
`more_chunks`/`n_prompt_tokens` on the answer going through it (in the ring that
was `overlay_head_authoritative_fields`). It sends both **forward**
(`prompt_progress_for_forward`) and the tail copies them into the result
(`apply_prompt_progress`). Getting this wrong silently truncates every prompt of
more than one chunk at the first one. The head's immediate ACK also carries that
nested structure; `prefill::run_direct` reads it to send the next chunk with
`start_pos = next_pos`, including when there is overlap and when the first chunk
reuses a prefix. `direct_lab.rs` refuses later chunks without `start_pos` and
covers both situations.

**Telemetry** (`direct::hop_entry`/`accumulate`/`record_transport`): each hop
describes itself on the way out and the caller sends separate evidence of the
crossing. `transport_traversals` holds one entry per call, in order, with
`transport` (`local`, `libp2p-tunnel`, `http`, `https` or `quic`),
`transport_fallback`, time and bytes sent and received. `transport_fallback`
only marks a call that tried one transport and fell to another after a failure;
a tunnel chosen on purpose (route pinned to it or a peer that only answers
through it) is not a fallback. The Chat shows the sum as "transport fallback"
(`https_fallback_calls`, an old internal name), with the reasons. The trace copies each
entry to the matching hop and phase, without charging the residual to the first
hop. The result does not wait for late receipts: if some evidence has not
arrived yet, the request proceeds with `transport_trace_complete=false` and the
UI makes the absence visible. A local hop on the same machine can correctly have
zero network; that is not an attribution to the first hop. If the direct result
beats the head's ACK, `await_result_with_ack` keeps the ACK in a detached task
and reconciles the receipt when it arrives, without delaying the client.

## 5. Speculative decoding

There are **two** mechanisms, with separate operator switches, and never both
in the same round (the batch is a single one, and mixing the sources would make
the acceptance rate impossible to attribute). The gateway chooses in
`generate_stream_with_stops`: with both on and a capable plan, MTP wins.

| | `speculative_decoding_enabled` | `mtp_enabled` |
|---|---|---|
| what drafts | `find_draft`: looks for a repetition of its own ending in the text already generated | the `nextn` block trained inside the GGUF |
| uses model weights | no - it is an array search | yes, a trained head |
| when it hits | repetitive output (code, lists, JSON) | any text, if the model carries the block |
| capability | `speculate/v1-lookup` (every hop) | `speculate/v1-mtp` (**the tail**) + `speculate/v1-mtp-sliced` on all |
| requires | nothing beyond the request | the tail with the `nextn` block (or a ready `draft`) |

The field that tells them apart on the wire is `speculate.source` (`"lookup"` /
`"mtp"`), read by `infer_server::Speculate::from_meta`. Absent means an old
gateway and counts as `lookup`, which was the only speculation that existed when
it was written.

### 5.1 Prompt lookup

`prefill::step_speculative` (prompt lookup): the head drafts up to `k` tokens
from its own recent history (searching in a window of `m` tokens; the longest
draft wins, and `m` degrades to `m-1, ..., 1`). The whole batch (`1 + draft`) is
decoded by the pipeline in a single step and the tail verifies
(`longest_match`): it returns `accepted` (the confirmed prefix + one extra real
sample), `accept_count` and `batch_width`.

- After each round the gateway fans out `rollback_sessions` to **every** hop with
  `accept_count`/`batch_width` (`handle_rollback` + `rollback_from` drop from the
  KV what was not confirmed; an unknown session is a no-op, not an error). Every
  hop decoded the whole batch, so every hop needs the rollback - not just the
  tail.
- **`kv_can_shift` is not the rollback guard.** That field exposes the K-shift
  of llama.cpp's reference `--ctx-shift`. A ready tail (`mtp_ready=true`) can
  produce `next_draft` even when it is false; the real speculative rollback uses
  `kv_remove` (`llama_memory_seq_rm`) after each batch and fails closed when the
  engine refuses the removal. `should_draft` still only decides whether the head
  builds the batch, based on `mtp_ready` or a ready draft.
- **The real proof is no longer static, it is each round's `seq_rm`.**
  `handle_rollback` (`/v1/session/rollback`) calls `kv_remove`
  (`llama_memory_seq_rm`) and returns `{rolled_back, reason}` in the body -
  `rollback_sessions` (`gateway/`) reads that body (200 OK is not success by
  itself, `handle_rollback` always answers 200); if any hop returns
  `rolled_back: false`, the gateway **ends the generation** with what was
  already confirmed (`partial: true`) instead of continuing on a cache that may
  have kept the refused draft without removing it - safer than risking the
  model attending to tokens the client never confirmed.
- **A speculative round has two legitimate answers.** The head says it refused
  to draft by **omitting `draft`** from the frame; without `draft`, the tail
  answers in the old one-token format (`{done, token_id, text}`) instead of
  `{done, accepted, accept_count, batch_width}` - `speculated` stays `false` and
  `batch_width` is 1. `prefill::speculative_step_from_meta` accepts both forms
  and also reads `speculation_active` and `speculation_disabled_reason` from the
  new callbacks. Their absence keeps compatibility with old peers.
- **Once speculation is refused, the gateway stops insisting** for the rest of
  the request (`speculating`, in
  `generate_stream_with_stops`): it switches to calling `prefill::step`, which
  already uses the direct reply (N+1 crossings). It is not just saving the
  `speculate` field - insisting traps the chain in the bubbled ring (2N
  crossings per token).
- The gateway clamps `speculative_k` to `n_batch` (`speculative_k_of`), because
  the speculative batch is also a single batch on the head.
- The legacy `step_speculative` route stays on the ring/hub, without using
  `relay/v2-direct`. Sliced MTP is still allowed on the legacy path when everyone
  announces `speculate/v1-mtp-sliced` and the tail announces
  `speculate/v1-mtp`, and it also applies to `remote-stream` with two or more
  hops. Each hop does a local rollback on the reply already in flight. A peer
  without the new capability degrades to the plain sampler with the reason
  `peer_without_sliced_mtp_wire`.

### 5.2 Multi-token prediction (MTP) - the engine's `mtp` module

The `nextn` block takes the trunk's hidden state at position `p` with the
embedding of the token at `p+1` and predicts the token at `p+2`. It is the draft
head LM Studio turns on when it offers "MTP".

The MTP context's `h` buffer uses `n_embd_out`, the physical width reported by
llama.cpp. In architectures with stream axes, using only `n_embd` would lose
part of the state before calling `llama_decode`; in models without expansion the
two values are equal.

- **The embedded engine already knows how to run it.** `llama-cpp-sys-2`
  0.1.154 ships `LLAMA_CONTEXT_TYPE_MTP` and `llama_model_n_layer_nextn` in the
  bindings. The other four entry points (`llama_{set,get}_embeddings_nextn`,
  `llama_set_nextn_layer_offset`) exist in `libllama.a` but **not** in the
  bindings, because they live in `llama.cpp/src/llama-ext.h`, outside
  `llama.h`'s `extern "C"` - the only header the crate's `wrapper.h` includes.
  Bridge: `inference/llama_mtp_shim.cpp`, compiled by `crates/node/build.rs`.
- **It costs one context on the VRAM path, and a sibling model on the
  fallback.** The draft context is created on top of the target's weights
  (`llama_init_from_model` with `ctx_type = MTP`); when it needs to go to RAM, a
  second CPU-only model keeps the whole `nextn` graph on the host. The extra
  context cost is still a one-layer KV cache, and the GGUF can share mmap pages.
- **VRAM is tried with a combined budget.** `fit_plan` reserves the MTP
  context's computable KV per device together with the main context (see
  "KV cache and VRAM fallback" above). The cost follows `n_ctx` and the
  effective microbatch, with no fixed constant; with more than one GPU each
  reservation goes to its own device, not into a global sum.
- **There is an explicit fallback to RAM.** If creating the MTP context fails
  in VRAM and `mtp_cpu_fallback_enabled` is on, loading is repeated with a
  CPU-only MTP model and with `offload_kqv=false` and `op_offload=false` on the
  MTP context. The target is then reloaded without the `nextn` weights of the
  VRAM attempt, so that memory also goes back to the GPU. An override only on
  the `nextn` tensors was investigated, but is not enough because the scheduler
  can also pick the layer's and the output head's device. The default is on; the
  operator can turn it off in the deployment when the local measurement does not
  pay for the CPU/GPU synchronization.
- **The smaller microbatch keeps the pairing.** `observe` builds each row with
  explicit position, sequence, token and hidden state: row 0 gets the
  `pending_h`, and the following rows get the target's `h` at the previous
  position. The target emits `h` for every position and the draft only asks for
  logits at the positions needed. The draft's `llama_decode` can split that
  internal batch into several `n_ubatch`, but does not recompute nor infer the
  correspondence from the microbatch boundaries. The probe
  `mtp::tests::the_nextn_head_drafts_from_a_real_model` crosses a microbatch
  with `fit_memory=true` and validates the observe, draft and accept cycle on a
  real GGUF when run with `--ignored`; the policy tests cover `n_batch=4096` with
  the fit on and off.
- **It works on the whole model and on the sliced pipeline.**
  `NodeConfig::mtp_effective` decides per node (whole model, or the tail of a
  sliced pipeline that serves the last layer); `PipelinePlan::supports_mtp`
  checks the plan. The machine that drafts is the one serving the last layer -
  in the sliced case, the tail, which has the `nextn` block; the head builds the
  batch.
- **The tail drafts in its own answer.** The draft travels backward **at no
  extra crossing cost**: in the remote stream, the tail attaches `next_draft` to
  the callback that would be sent anyway and the head installs it in the next
  internal `step`'s `speculate.draft`. The gateway receives that field and the
  state metadata, but does not reinject the draft in this mode. On the legacy
  `relay/v2-direct` path, the gateway still does the injection on the next
  round. The real cost to measure in the sliced case is the acceptance rate -
  not the network.
- **The batch's composition travels to the tail.** The head echoes in the reply
  the decoded batch (`tokens` field) and the requested draft width (`mtp_k`);
  middle hops forward it; the tail feeds MTP's `observe` with that composition
  (`decode_hidden(..., tokens)`). Without it - an old peer that does not echo -
  `observe` turns MTP off with a warning: the answer stays correct, just not
  speculative.
- **A round without speculation does not run `observe`.** On the tail, a decode
  with neither `mtp_k` nor an MTP draft (every token of a `remote-stream` with
  two or more hops, and the rounds after the gateway stops speculating) uses
  `decode_hidden_without_mtp`: the `nextn` head does not decode and the sequence
  is forgotten in the MTP context (`MtpDrafter::forget`, total removal), so a
  later `observe` starts from an empty sequence instead of a hole of positions.
  Before, `observe` ran per token for nothing, and it was the first sync point
  after the tail's `llama_decode`: in a 2026-09-28 measurement the tail's
  `decode` took 34 ms per token against 8 ms of `backend_call_ms`, with no field
  saying where the rest was. `node_timings.mtp_observe_ms` now measures
  `observe`, and the `POM_PROFILE_SYNC` barrier runs before it, so
  `backend_sync_ms` isolates the trunk finishing on the GPU.
- **Acceptance is per-request telemetry.** Each `step` publishes in
  `node_timings` the counters `mtp_draft_tokens_proposed`,
  `mtp_draft_tokens_accepted` and `mtp_draft_tokens_rejected`. `proposed` is the
  size of the draft actually submitted for verification, so each verified token
  ends up accepted or rejected. The gateway sums the counters of every hop and
  leg in `RequestTrace` and computes `mtp_acceptance_rate` as
  `accepted / proposed`; without an MTP proposal the rate is `null`. Counting
  happens once per verified batch, outside the per-token loop, and changes
  neither the parameters nor the speculation criterion.
- **It requires the new binary on every hop.** `supports_mtp` asks for
  `speculate/v1-mtp-sliced` on each step (the `tokens`/`mtp_k`/`next_draft`
  protocol only exists in a new build - an old peer in the ring would get a
  `speculate` it does not understand) **and** `speculate/v1-mtp` on the tail.
- **A ready hybrid/recurrent model drafts even when `kv_can_shift=false`.** In
  the remote stream, each hop does a local rollback with `kv_remove` before the
  answer goes up, with no extra endpoint, and ends with an error if the
  effective removal fails. On the legacy path, the `/v1/session/rollback`
  fan-out stays.
- Sliced MTP uses `step_stream` when the head is remote: a control frame starts
  the cycle and the callbacks carry the tokens and the state. If the head is
  local, the gateway keeps the direct path `prefill::step` + `relay/v2-direct`.
- The final slice is the only one that carries the block because
  `splitter::split` zeroes `<arch>.nextn_predict_layers` outside it - see
  `docs/subsystems/slicing.md`.
- **A separate draft GGUF is the second source of `nextn` weights.** Not every
  model embeds the block in its own file - Unsloth distributes the one for
  Qwen3.8-Flash-Next (`qwen4exp`) in a separate GGUF
  (`mtp-<model>-shared-*.gguf`) that loads as an independent `LoadedModel`
  (`LoadedModel::load_mtp_draft`) and becomes the same `companion` as the RAM
  fallback in `mtp_model_for_attach` - the two never coexist. Discovery follows
  the `mmproj` convention (a sibling file, `mtp-` prefix,
  `catalog::find_mtp_draft_sibling`), runs directly on the machine that loads the
  model (`run_apply`/`reconfigure_local`, never the central catalog) and has no
  switch of its own: it uses `mtp_enabled`. **A prerequisite the draft file does
  not supply on its own**: the target must export `h_nextn` from its own graph,
  which is per-architecture code in the fork
  (`build_arch_graph`/`res->t_h_nextn`); `qwen4exp.cpp` got that in
  `pieceofmindcloud/llama.cpp` (`pom-tensor-parallel`), gated by
  `cparams.embeddings_nextn && !cparams.embeddings_nextn_masked` so the early
  cut of the last layer's rows (`inp_out_ids`) does not regress when MTP was not
  requested - without it the architecture would pay the FFN/hyper-connection
  cost per position on every prefill, not only when someone reads the hidden
  state. There is no static Rust-side check of that capability: the generated
  list of MTP architectures only certifies who builds the `DECODER_MTP` graph
  (the draft's capability), not who exports the target's `h_nextn`; a target
  without that export simply fails on the first `observe` and turns MTP off for
  that session with a warning, the same degradation that already exists for an
  old peer that does not echo the batch composition in sliced MTP. Multi-node
  slicing of the draft is not implemented, for the same limit as the bullet
  above (MTP with a separate draft only works today with one machine serving
  the whole model). `llm::config::fit_plan` decides `MtpFitMode` before loading,
  and until 2026-09-18 it only looked at the **main** file's
  `has_nextn_tensors` - a defect confirmed live on two CUDA machines that
  reported "MTP unavailable" even with the right external draft already on disk,
  because `fit_plan` never checked `EngineConfig::mtp_draft_path`. Fixed:
  `fit_plan` now treats a valid external draft as MTP availability as good as an
  embedded `nextn`, without running the `DECODER_MTP` architecture check against
  the target in that case (it does not apply - the draft is what builds that
  graph). For `qwen4exp`, the fork also needed the `mtp_only` pattern of
  `qwen35.cpp`/`deepseek4.cpp` ported to `qwen4exp.cpp` (the trunk and the
  model-level `hc_head_*` become `TENSOR_NOT_REQUIRED`, the tail gets
  `layer.nextn.*`, `build_arch_graph` gets a `graph_mtp`) - without it the draft
  file itself did not load. See `crates/llama-engine/src/mtp.rs` for the full
  mechanics.

  **Shared drafts borrow the target's embedding and output head.** Some
  published drafts (flagged `<arch>.nextn_shared_target_tensors = true`) leave
  `token_embd.weight` and `output.weight` out, and llama.cpp loads a draft as an
  independent model, so such a file fails with `check_tensor_dims: tensor
  'token_embd.weight' not found`. Before loading, `load_with_cache` passes the
  draft through `llama_engine::mtp_draft::ready_draft_path`: a draft that has
  `token_embd.weight` is used unchanged; otherwise a completed copy is written
  that keeps the draft's metadata and tensors and appends the missing tensors
  read from the node's own target file (the final slice or the whole model,
  multipart included). The copy lives in `.mtp-drafts/` next to the target, is
  named after the draft plus a fingerprint of both files (path, size, mtime),
  is reused while neither changes, and replaces older copies of the same
  draft. The original files are never written. A target without
  `token_embd.weight` (a middle slice) is refused by name, and that reason
  reaches `mtp_unavailable_reason`. The model catalog skips `.mtp-drafts/`.

  **Attaching the draft is not enough - both loading the weights and the MTP
  context can blow VRAM even when the fit already decided RAM.**
  `LoadedModel::load_mtp_draft` alone always tries VRAM (`MtpFitMode::Vram`,
  `DEFAULT_GPU_LAYERS`) without measuring free memory - unlike the target, whose
  `fit_plan` measures the remaining VRAM and reduces layers before loading. That
  `fit_plan`'s decision (`vram_plan.mtp_mode`) reaches the external draft in two
  layers, fixed in separate rounds of the same investigation (2026-09-18):

  1. **Loading the weights.** When `vram_plan.mtp_mode == MtpFitMode::Ram` (the
     main VRAM is already saturated), `load_with_cache` goes straight to
     `LoadedModel::load_mtp_draft_cpu` (`n_gpu_layers=0`) instead of asking for
     GPU for the draft's weights - asking anyway only repeats the `cudaMalloc`
     that already failed for the target, now on the draft's weights (~2.3 GiB
     in the unsliced whole model test). Reproduced live on a CUDA server:
     `mtp_fit_mode=Ram` in the fit log, followed by
     `llama_model_load: error loading model: unable to allocate CUDA1 buffer`/
     `cudaMalloc failed: out of memory` inside `load_mtp_draft`. A reactive
     fallback with the same function applies when `load_mtp_draft` (VRAM) fails
     even starting from `MtpFitMode::Vram` (VRAM changed between the fit and
     this load).
  2. **Creating the context.** Even with the weights already in RAM (through
     layer 1 above, or organically because `llama.cpp` found no room tensor by
     tensor), the first attach attempt always used the target's
     `cfg.offload_kqv` - with `op_offload=true` the scheduler still reserves a
     compute buffer on the GPU for weights already on the CPU, and that
     worst-case reservation (full ubatch) may not fit
     (`cudaMalloc failed: out of memory`, reproduced live in an earlier test on
     the same server). Fixed: when the weights already go into RAM, the context
     is born with `offload_kqv=false`; when the weights fit in VRAM but the
     context still fails, `load_with_cache` retries with `offload_kqv=false`
     (the same fallback the embedded RAM path already uses).

  Both layers only fire when the operator allows the RAM fallback
  (`mtp_cpu_fallback_enabled`); neither reloads weights beyond what is needed -
  layer 2 never reloads any weights, it only recreates the context.

**The two details that are the correction.** `seq_rm` runs before each decode of the draft (in
`observe` and in `draft`): the previous round left in the draft's cache the positions it bet on,
and the verification may have refused part of them; rewriting over them without erasing would leave
two cells for the same position and the attention would read both. And the batch's token pointer is
ours: `llama_batch_init` allocates `token` **or** `embd`, never both, and the MTP needs both, so
the pointer goes back to the original before the `llama_batch_free`, which would free Rust's
memory. `MtpDrafter::draft` decodes the head once per drafted token and returns the ids; `p_min`
cuts at the first doubtful guess.

**What is not measured yet.** The gain the block promises is real upstream (21 -> 35 tok/s on
Qwen3.8-27B-Q3_K_S, measured outside this project). Here the implementation is complete and green
in the unit tests, but there is no end-to-end measured tok/s number for POM's own pipeline.

### 5.3 Kimi-K3 recurrent state

Kimi-K3 is hybrid: it alternates recurrent KDA layers and full-attention MLA
layers. When `n_rs_seq > 0`, `llama_memory_hybrid` reserves snapshots per
sequence and `init_batch` calls `llama_batch_allocr::split_equal` keeping each
sequence's final `n_rs_seq + 1` tokens in the same ubatch. The token position
can be close to `750000` without overflowing the allocator's math: that path's
indexes and offsets are 64-bit. A `516`-token ubatch with six sequences is also
a valid geometry (`6 * 86`), produced by the graph's rectangular reservation,
not a write 4 bytes past a 512 buffer.

Rollback support must exist in two layers. The native classifier must accept
`LLM_ARCH_KIMI_K3`; otherwise `llama_init_from_model` silences `n_rs_seq` to
zero. On top of that, each KDA layer must write the `1 + n_rs_seq` convolution
windows into the snapshot groups and run the scan through
`build_recurrent_attn`, which restores the state matching the sequence and
position. Writing only the last window and copying `build_delta_net`'s final
state leaves the hybrid cache incompatible with the rollback the gateway does
after a speculative round. The failure can show up later, while building a
ubatch, as the SIGSEGV in `llama_batch_allocr::ubatch_add`, even though the
origin is the incomplete recurrent state.

The native test `test-hybrid-memory-batch-kimi-k3` generates a minimal K3 GGUF
and repeats four decodes with six sequences, `n_ctx=750000`, `n_ubatch=512` and
a three-token rollback/replay. `test-batch-alloc` separately covers the
allocator's geometry in a large context and random split shapes.

When the operator asks for MTP and the draft context cannot be created, the
runtime keeps the main inference available, publishes `mtp_ready=false` and
`mtp_unavailable_reason` in `/health`, and the Dashboard's details tab shows the
state and the reason on the machine card. The reason distinguishes a model
without a `nextn` block, a lack of VRAM with the fallback refused and a lack of
memory on both sides. When ready, `/health` publishes `mtp_allocation` as `vram`
or `ram`, always with the placement measured in the context's buffers. A later
`observe` failure also turns the head off and updates that reason, so the
degradation does not depend on reading the log.

## 6. Pipeline planning (`routing/pipeline.rs`)

`plan_pipeline` walks from `0` to `total_layers`; at each position it picks the
live peer that announces `inference_port` and the same `model` covering the
position, with the score **`layers_count * 100 - current_load * 10`** (more
layers and fewer in-flight requests win). It returns `None` at the first
coverage gap - then the gateway fails (see section 7). `LoadTracker::acquire`
returns a RAII `LoadGuard` that decrements on drop: the load balance never stays
stuck on an error. With replicas, the delivery time planner described in
`parallelism.md` ("Choosing the pipeline by delivery time") picks among the
complete paths; this greedy planner is the fallback.

- `ready_for_pipeline` (gateway) probes each peer's `/health` with a 300 ms
  timeout **before** planning - an old UDP announcement must not poison the plan
  nor leave a request waiting on a hop that no longer serves HTTP. The probe
  tries each inference address, the host that already answered first
  (`Network::inference_bases`), and last the libp2p tunnel (3 s): only the first
  announced address used to leave out of the plan a peer behind an unreachable
  IPv6, and the request failed with "Gap at layer 0". The same answer updates
  the executable identity and range when gossip has not propagated the most
  recent `Announce` yet.
- `resolve_step_url` picks each hop's address in order: the route measured in
  `RouteTable` (if it is still an announced candidate) -> `inference_url` (the
  observed address, the only one proven before the first probe) -> the first
  candidate. It pins the choice per session - the KV cache on the other side is
  per session, and switching addresses midway is worse than the slow route.
- The plan is built per request; `PipelinePlan::hop_chain` gives
  "abc123→def456", the proof of distributed execution the gateway puts in
  `source`.

## 7. Fallbacks on the gateway

The gateway **only has the pipeline path**. `generate_stream_with_stops` fails
with "Nenhum pipeline válido para o modelo X (verifique a cobertura de camadas
0..N)" ("no valid pipeline for model X (check the layer coverage 0..N)") when
`plan_pipeline` returns `None`; the `/api/generate` handler answers 503 with
"Inference failed. Error: ..." and `source: "none"`.

There is no fallback to Ollama: `OllamaClient` only resolves models
(`/api/tags`) and feeds the `ollama_available` field of `/health`. That field
uses the last cached value and schedules a probe of at most 500 ms at most every
15 s; `/health` does not wait for a connection to the optional service. A prompt
without a valid pipeline does **not** fall to the local Ollama nor to a peer's -
the request fails. (Old documentation described a three-level fallback; that
was an earlier state of the code, corrected on 2026-08-15.)

## 7.1 Plugin inference services

A deployment may name a plugin's inference service instead of POM's own
engine (`RuntimeParams::inference_service`, for example `"strata"`). That is
an explicit, separate deployment class, never a fallback: the plan for such a
model is one step on one machine - or, for a service that publishes `stages`,
one step per layer slice (see "Split across machines" below) - and the gateway
runs it through that plugin or fails.
`crates/node/src/inference/plugin_engine.rs` holds the three pieces.

- **Target node.** The runtime supervisor calls `plugin_engine::run` instead
  of `infer_server::run`. It binds the inference port first, asks the plugin
  to load the model through the local plugin host (`engine.load` with
  `model_path`, `model_label`, `n_ctx`, `max_sessions`, `kv` and the NVIDIA
  GPU indices of the machine), reads `engine.status` every 2 s and answers
  `/health` with the fields the llama.cpp server publishes: `model_loaded`
  only once the plugin is `ready`, the layer range, `n_ctx_per_session` (the
  whole window: the engine gives it to every request) and
  `capabilities: ["engine/plugin-v1:<code>"]`. The health watcher, the slice
  journal, the restart budget, idle unloading and `/v1/models` therefore work
  unchanged. When the generation ends (undeploy, redeploy, stop) it calls
  `engine.unload`; an `error` state, or `idle` after the engine had served,
  ends the generation with the plugin's reason and the supervisor restarts it
  within its budget. While a plugin engine serves, the node announces
  `engine/plugin-v1:<code>` (and the libp2p tunnel capability) instead of the
  frame, relay and speculation capabilities, so no gateway can plan it as a
  hidden-state hop. `Network::plugin_engine_capabilities` is that rule, and
  every builder of the node's capabilities uses it: the announce, the
  gateway's own peer entry and `/api/ui/node`.
- **Gateway.** After planning, `generate_stream_inner` checks
  `plugin_of_plan`: a plan whose steps all announce the same
  `engine/plugin-v1:<code>` and run whole slices in layer order is run by
  `plugin_engine::generate` on its last step (the machine with the head). It
  sends the prompt the protocol adapter
  already rendered, with the same `chat_template` and thinking flags a
  llama.cpp head receives, plus the client's `max_tokens`, stop sequences and
  numeric sampling fields (`temperature`, `top_p`, `top_k`, `min_p`,
  `presence_penalty`, `frequency_penalty`). The generation route answers one
  JSON object per line - `{"t"}` text, `{"p"}` prompt progress, `{"done"}`
  with finish, prompt, completion and cached tokens, the engine's llama.cpp-style
  `timings` (with the MTP drafts, `draft_n` and `draft_n_accepted`) and the
  `stop_sequence` that ended it, or `{"error"}` - and the text becomes
  `GenEvent::Token`; reasoning comes back inside `<think>` tags and tool calls
  in the model's own template form, as a llama.cpp pipeline returns them, so
  the gateway's tool-call parser reads them. The summary is the one a
  pipeline leaves: `source` is `pipeline:<chain>` and its `RequestTrace`
  (`plugin_engine::engine_trace`) has a hop per stage, each executed by the
  `plugin` engine on path `plugin/<code>`, with the request's prompt call and
  decode rounds (generated tokens less the accepted drafts), the MTP draft
  counts, and the engine's prompt and decode times on the last stage. The
  engine reports the request as a whole, so the earlier stages carry no time
  of their own. The request audit, the statistics and the process log the
  admins receive are the same whatever the engine. The target node logs every change of the
  engine's state, and the plugin's own output reaches the node's log through
  the plugin host's stderr. Images and typed
  decisions (`/v1/systemone` labels) are refused for this class; the retain
  boundary of retrieval is only a cache hint and is ignored.
- **Between nodes.** When the gateway is another machine it posts to
  `POST /api/internal/plugins/:code/engine/generate` on the target (signed
  control envelope, live peer, body up to 32 MiB). The target checks that its
  runtime serves that inference service, takes the plugin's loopback
  generation server from `engine.upstream` (`{status, port, token}`) and
  streams the answer back unchanged. The plugin's server never listens beyond
  loopback.

**Split across machines.** A service that publishes `stages: true` also runs
POM layer slices, one per machine (`crates/node/src/inference/plugin_stage_link.rs`).
Every machine of such a deployment runs `plugin_engine::run` on its slice; the
engines exchange their own hidden states, never POM's hop protocol of section
2, and the node only carries them:

- A later stage's runtime (`prepare`) waits for the live peer of the same
  `deploy_id` whose slice ends at `start_layer - 1` and serves the same
  plugin, asks it for the stage's link secret
  (`GET /api/internal/plugins/:code/engine/link/:deploy`) and opens a relay on
  loopback. Its `engine.load` gets `link: {token, prev_port}`; the first
  stage's gets `link: {}` and its plugin makes the secret, which
  `engine.link` (`{status, port?, token?}`) returns to the local node only.
- The relay reads the engine's frames (a little-endian `u32` length, then the
  bytes, both ways) and posts each one, signed and along the measured route,
  to `POST /api/internal/plugins/:code/engine/link/:deploy/:conn` on the
  earlier machine (up to 64 MiB a frame); the answer's body is the answer
  frame, and a wait while the earlier stage loads is retried. That route keeps
  one loopback connection per `conn` to the port of `engine.link` and passes
  the frames through; an empty body closes `conn`.
- Both routes take only a signed call from the live peer that serves the next
  slice of the same deployment. The node reads nothing of a frame but its
  length, and no engine listens beyond loopback.

Only the last stage takes requests: the earlier one answers `engine.upstream`
with an error.

## 8. direct_lab.rs - the "law"

`direct_lab.rs` (cfg(test)) keeps the whole fix, measured: **the cost per token
does not depend on where the pipeline's entry point is.** Fake hops speak the
real protocol (`wire`, `relay::send_frame`, `infer_server::forward_direct`,
`direct::deliver_result`; the fake gateway serves the production
`/v1/relay-result` handler), driven by the real `prefill::run`/`step`. The only
pretense is the engine (it sleeps instead of decoding).

The network is emulated per site: each hop and the gateway live in a site, and
every message between different sites costs `LINK_MS = 50` one way, charged on
arrival and again on the way back. So the bench measures crossings, not the
machine where it runs. The test
`the_cost_per_token_does_not_depend_on_where_the_entry_point_is` requires, for
N=2,3,4 with the gateway at the head, in the middle and at the tail, legs per
token ≈ the theoretical minimum (0.45 leg tolerance); that both ends tie; and
that the worst entry (the middle) costs at most one leg more than the head. The
baseline proves it is a fix, not behavior that was always there: the bubbled
ring with the remote head pays 4 crossings (the case that hurt, the drop from 25
to 14 tok/s). It also asserts that each frame is delivered directly exactly
once (prefill and decode), not counting the transport receipts, and that a
prompt of more than one chunk crosses whole, including with prefix reuse.

- **No test coverage**: the four failure modes (middle hop dead, tail dead, lost
  direct reply, expired deadline) have a design and code in `direct.rs`, but
  **no** end-to-end test - a maintainer decision on 2026-08-14 (quota).
  Speculative decoding also still has no coverage on the direct detour (see
  section 5).
- Speculation has its own bench, `spec_lab.rs` (cfg(test)): the real
  `generate_stream_with_stops` against fake hops that decide with the production
  `should_draft`/`find_draft`/`longest_match`. The engine is faked (without a
  GGUF it proves the contract between processes, not what llama.cpp does with
  the batch); that is why it does not exercise real `n_rs_seq`/`seq_rm`. Since
  the fake hops implement single-item frames, the fixture only announces
  compatible capabilities and does not enter the `BatchCoordinator`; only
  llama.cpp does that, see the module doc for what changed on 2026-08-16. It also
  checks the aggregated `GenerationTimings` summary on the gateway's generation
  path, including a one-token answer.

## 9. Prompt limit

`gateway::prompt_budget_of(n_ctx, max_sessions, max_new_tokens)`:

```
n_ctx - max_new_tokens
```

`n_ctx` is the full window of each session. `max_sessions` (default **1**) only
multiplies the total capacity reserved in `llama_context`, so the runtime uses
`n_ctx_total = n_ctx * max_sessions` and each session keeps `n_ctx`. The gateway
measures the prompt (`estimate_tokens` ≈ chars/3, pessimistic) **before** any
peer sees the prompt and refuses with a message citing the flags. `/health`
exposes both values as `n_ctx_total` and `n_ctx_per_session`.

**The session window is enforced, not just estimated.** The KV cache is
unified (`kv_unified`), so llama.cpp lets a sequence grow beyond `n_ctx` by
using another session's cells - and reports `n_ctx_seq = n_ctx_total`, which
produces the warning `n_ctx_seq (...) > n_ctx_train (...) -- possible training
context overflow` when the total exceeds the training context. The warning is
about the whole pool; each session's window is still `n_ctx`, enforced at three
points:

- **gateway, after prefill:** `OutputBudget::within_session_window` recomputes
  the total with the **real** prompt size (`n_ctx_per_session - prompt`). A
  prompt denser than the 3-characters-per-token estimate (code, CJK) no longer
  lets the generation cross the window; reaching it ends with
  `finish_reason: "length"`.
- **peer, at prefill:** `check_prompt_fits_session` refuses a tokenized prompt
  that fills the whole window, leaving no room for the answer.
- **peer, at decode:** `check_decode_fits_session` refuses to decode with the
  session already at the window's last position - the safety net for a caller
  that does not go through the gateway and for the head that generates ahead in
  remote-stream.

That admission applies **also on a gateway that loads no model at all**. Until
2026-09-08 it only existed when there was a local `NodeConfig`; a gateway-only
node let the prompt cross whole, and the peer's decode aborted the process with
`CUDA error: out of memory` in `ggml-cuda.cu` - no refusal, no message, and
taking down every other request in progress. Without a local config the gateway
uses the `RuntimeCapacity` announced by the peer serving the model, the same one
`/v1/models` publishes, through `runtime_capacity_for_model`. The check sits
before the health probe and planning: an already inadmissible request does not
justify a network round trip, much less a prefill. A **pending** capacity is a
lack of information, not a permission - in that case the peer still decides.
`prompt_budget_error_of(n_ctx, max_new_tokens, prompt)` is the pure function
shared by both paths; the local variant adds the note that `n_batch` does not
limit the prompt.

The OpenAI chat adapter does the same check before starting the SSE and answers
HTTP 413 when the budget is exceeded. Without that preflight, generation emitted
an error event inside an HTTP 200; clients such as OpenCode waited for a valid
answer and only showed a run without an answer. Requests that fit the budget
keep the SSE and the distributed path unchanged.

`POST /api/generate` accepts an optional `max_tokens`. When absent, it uses 512;
when present, it requires a value greater than zero. The effective limit is the
smaller of the request and the `n_ctx` space left after the prompt. The first
token returned by the prefill already counts, so a request for 64 never
generates 65 by adding an extra decode round.

- `n_batch` does **not** enter the math: the prompt goes in chunks, so `n_batch`
  limits a chunk (`min(prefill_chunk_tokens, n_batch)`), not the prompt. Before
  prefill was chunked, `n_batch` in the formula capped every prompt at 512 tokens
  however large `n_ctx` was.
- Why the limit exists: going past it makes llama.cpp abort the **whole
  process** with `GGML_ASSERT(n_tokens_all <= cparams.n_batch)` - a large prompt
  takes down the peer and every request in flight. Refusing early keeps the node
  up. Measured numbers: OpenCode 1.18.21 sends ~2200 characters in the title
  call and ~54k in the default agent, with `max_tokens: 32000`, tools and
  `stream_options.include_usage`. The config that made the default agent work:
  `n_ctx` 32768, `n_batch` 3072 and `max_new_tokens` 256 in the deployment -> a
  budget of 32512 tokens; without a compatible window the node answers HTTP 413
  before the SSE.

### 9.1 The two output budgets and the continuation loop

The deployment's `max_new_tokens` **was never a backend limit**. The
`infer_server` does not know it: it decodes as long as the gateway asks for
`/v1/step`. It was always a gateway cut - and cutting there, silently, was the
defect. Measured on the cluster on 2026-08-27, `max_tokens: 4000` against a
deployment of 1000 returned 1000 tokens and `finish_reason: "length"`, with no
error at all; with a declared tool, the cut fell **inside** the `<tool_call>`
and the answer came back without `tool_calls`, with the unfinished call degraded
into loose text. A harness that treats `max_tokens` as a terminal condition -
opencode does - drops the turn there and the file is left half written.

Today there are **two numbers**, never a single one (`gateway::OutputBudget`):

| | who decides | what it limits |
|---|---|---|
| **leg** (`leg`) | the deployment's `NodeConfig.max_new_tokens` | **one** backend generation |
| **total** (`total`) | `min(request, n_ctx - prompt)`, recomputed with the real prompt after prefill | the **whole request** |

Exhausting the leg does not end the answer. `gateway::note_leg_boundary` counts
the leg and the loop keeps decoding on the **same** KV session, on the same
connection, in the same SSE stream: nothing is re-prefilled, nothing is resent,
the client has no way to tell the stitch apart. Only the **total** ends it, and
only it produces `finish_reason: "length"` - whether it is the client's
`max_tokens` or the session window. `x_pom.continuation_legs` says how
many legs the answer used - `1` is a single generation.

That applies to `stream: true` and `stream: false` through the same path:
`stream: false` stitches internally and returns a single answer. Tool call
parsing happens on the **complete** output in both - the `TagScanner` is only
closed (`finish()`) at the real end -, because an intermediate chunk is an
unfinished tag by construction.

**The ceiling requested by the client is enforced and inviolable.** Nothing
goes past it, for any reason, not even a tool call open at the cut token.
Reaching it is a **terminal stop**, with the matching stop reason. That is both
providers' rule, checked in the live documentation on 2026-08-29: `max_tokens`
is "an enforced per-response ceiling the model has no knowledge of", and when it
is reached the output "truncates mid-way and requires a retry". There is no
continuation, slack or overflow in either API.

There used to be a slack here that let generation go past the ceiling to close
an open call. It was **removed**: what the providers offer instead of overflow
is the [task budget](http-api.md#task-budget), which tells the model instead of
stretching the ceiling.

**The node's capacity is derived from the runtime, it is not a silent cut.**
The effective `n_ctx` confirmed by `/health` is announced in `/v1/models` as
`context_length` and `max_completion_tokens`. The gateway uses the smallest
context confirmed among the peers of the model's planned pipeline and only
reduces the budget when the prompt takes part of that window. There is no
additional absolute capacity in the configuration file.

The remaining context keeps tightening the ceiling **without** refusing, and
that is deliberate: it comes from an estimate of the prompt's tokens
(`estimate_tokens`), and refusing on an estimate would reject valid requests.
`x_pom.output_limits` tells the truth, which is the right channel for an
approximate number.

**A cut inside a call returns the call structured and incomplete.** That is
both providers' shape, checked in the live documentation on 2026-08-29.
Anthropic returns the last block as `tool_use` with incomplete `input` and
`stop_reason: "max_tokens"`, and tells the client to check exactly that pair to
repeat the request with a larger `max_tokens`. OpenAI returns
`tool_calls[].function.arguments` - documented as a string that "is not always
valid JSON", to be validated before executing - together with
`finish_reason: "length"`. Neither discards the call, and neither degrades it
into text.

`TagScanner::finish_truncated` recovers what it can: the tool's name and the
parameters already emitted, including what was being written at the moment of
the cut. The arguments come out as an **unfinished** JSON string, cut where the
model was cut - it does not parse on purpose, and that is what makes a client
that ignores the stop reason fail instead of executing half a call. Anthropic's
`input`, typed as an object, carries the same recovered content.

| situation | the client gets |
|---|---|
| cut inside the call, name already emitted | the call **structured and incomplete**, `length`/`max_tokens`, `x_pom.truncated_tool_call: true` |
| cut before the name closes | only the text before the opening; a half name is not a tool |
| no call open | truncated text, `length`/`max_tokens`, nothing else changes |

Raw markup is never an answer: the scanner holds the call's body until it knows
what it is, so nothing from inside the tag leaks as prose. In streaming the
arguments go out in fragments as the model generates (see `http-api.md`,
"Incremental argument streaming"), and what already went out counts: the close
on a cut emits **only what is missing**, so a normal partial fragment and a
terminal truncation are never confused. `x_pom` tells the truth:
`truncated_tool_call` says whether the cut fell inside a call, and `partial` is
`true` whenever that happened or there was a transport loss.

**Why there is no re-prefill.** The loop does not reopen a generation: it
continues the one already open. The alternative - ending the leg, reopening
with the generated text as a prefix and counting on the prefix cache - would
cost re-prefilling what already went out, on every leg, and would take exactly
the same KV in the end. There is no advantage at all in paying for it.

**Capacity of the pipeline's peers.** Once `/health` proves the load,
`runtime::spawn_health_watch` publishes in `Announce.runtime_capacity` the
effective per-session `n_ctx` returned by `/health` (the engine's total divided
by `max_sessions`), and the leg's `max_new_tokens`. The gateway uses the
smallest value among the planned pipeline's peers both in `/v1/models` and in
request validation. Apply, error, stop and thermal unload clear the announcement
before the restart, so an old capacity does not stay in the catalog. An idle
deployment stays in the catalog as selectable, even without
`runtime_capacity`, while a restarting peer without `idle` stays hidden until it
confirms its health. An idle peer chosen by the plan is woken by the
`reload_model` event in parallel with the other planned idle peers and the
request waits for the new `/health` for up to 15 minutes; a live peer on an
alternative route does not block admission. During the transition, the runtime
waits for the previous generation's supervisor to finish shutting down the
server and the model's FFI release before binding the new port. That release
uses `spawn_blocking`, so `health` and the other HTTP routes do not get stuck on
`llama_free`. Shutting down the QUIC endpoint uses a one-shot channel, not a
notice that can be lost before the first poll, and has a 2 s deadline. The
supervisor's wait has a 5 s deadline; if the old generation does not end, the
new one is not started and the handle stays registered for another attempt. The
immediate range announcement is best-effort and has a 1 s deadline, so a DNS or
UDP failure does not stop the local server from starting. Without a matching
peer, the local configuration stays the fallback for routes without a
deployment.

**Known limit.** `context_available` still uses the per-session window; with a
pool (`max_sessions > 1`) each context gets `n_ctx / n`, so that number is
optimistic. The published capacity must be read again after any restart or
deployment change.

The bench is `crates/node/src/backend/continuation_lab.rs`.

## 10. Each hop's channel in telemetry

`SentFrame` and `PhaseTrace` carry the transport's identity and each crossing's
measurement, and they show up in their own row of the Chat telemetry table.

- **`transport_summary`** - a bounded summary of this hop's crossings, grouped
  by transport (`local`, `libp2p-tunnel`, `http`, `https`, `quic`,
  `remote-stream` or `unknown`). Each entry reports calls, bytes sent and
  received, minimum, median, p95 and maximum RTT, plus how many calls arrived
  through a fallback on that transport. The summary also reports the total
  `fallback_calls` and `transport_switches` grouped by origin/target pair, so a
  switch during the run does not disappear in the aggregate. The direct path
  gets the receipts through messages authenticated by the ticket, but the answer
  does not wait for late receipts; `transport_trace_complete=false` makes the
  loss explicit. The samples used for quantiles are capped internally, so the
  production answer does not grow with the number of tokens. For QUIC, each
  entry also sums `quic_request_body_send_us` and counts
  `quic_request_body_send_calls`: the wall time awaited in `send_data` and
  `finish`, before waiting for the answer headers. A batched frame shares one
  physical operation, which is attributed only once. That time measures local
  acceptance of the body with stream backpressure, not confirmation of remote
  delivery.
- **`telemetry`** - a summary of the distributed collection's integrity. The
  `remote-stream` callback keeps each hop's participation and its `span_count`,
  while `ReplyInbox` keeps a snapshot for a short window to attach transport
  receipts that arrive after the answer. Duplicates do not enter twice; after
  expiry, a late delivery is refused and does not retroactively become unknown
  time.
- **`tls` / `tls_calls`** - whether the hop was encrypted. In `PhaseTrace` it is
  a **count**, not a boolean: a phase adds up several calls, and if only some of
  them went over TLS the screen shows the fraction (`TLS on 3/4 calls`) instead
  of rounding to "encrypted". That is how a half-done policy switch shows up, and
  also how a call site left out of the conversion to the pinned client gives
  itself away.
- **`tls_handshake_ms`** - measured **on the server**, around the accept loop's
  `TlsAcceptor::accept` (`backend/serve.rs`), which is where the time is exact.
  It comes back in the answer's `meta` and is aggregated like the
  `node_timings` already are.

**What that number is, and why the screen says it plainly.** The handshake
happens **once per connection**, and connections stay in `reqwest`'s pool:
almost every hop reports zero. That is the truth, not a missing measurement.
Without the label, whoever looked at a zeroed hop would conclude TLS is free and
whoever looked at the hop that opened a connection would conclude it is
expensive per token - two wrong readings of the same number. The per-request
cost after the handshake is the record layer's encryption, diluted inside
`response_body_ms` and not separable without instrumenting the socket; the
screen names that limitation instead of inventing a column.

**The persistent channel of section 2.1 is not a separate public transport.**
It uses the same pinned HTTP or HTTPS - `tls` follows the cluster's policy -,
but a crossing through it opened no request: the frame went into an already open
body. A crossing that shows up as `http` or `https` with
`transport_fallback=true` on a hop that announces `transport/link-v1` is the
channel having failed and the generation having continued - which is the
requested behavior, not a defect. `tls_handshake_ms` stays zero on the channel's
crossings for the usual reason: the handshake happened once, at opening.

**A hop through the libp2p tunnel reports `tls: false`.** It is encrypted - by
Noise, end to end -, but not by TLS, and saying `true` there would claim a
guarantee that hop did not give.

In the remote transport, the `remote-stream` record represents a batch
callback, not a new inference round trip. `bytes_received` is the batch's body
and `processing_ms`/`node_timings` are aggregates of the tokens it contains;
`round_trip_ms` stays zero because the callback is delivered to the gateway's
inbox, which already separates the client's wait from the generation. The
summary also publishes open, lifetime, frames and bytes, first delivery, frame
latencies min/p50/p95/p99/max, remote compute total/min/p50/p95/p99/max and
stream wait total/p50/p95. The wait also publishes totals and quantiles for
production, downstream, channel reservation, queue and previous callback. The
first control frame keeps the measurement of the transport used to start the
stream.

## 11. Defects noted

- **SIGSEGV when starting the context, regardless of logs (2026-09-04)**: on an
  Apple M4, the release binary was started with the same command for
  `target/qwen2.5-0.5b-instruct-q4_k_m.gguf` and
  `target/gemma-3-270m-it-Q8_0.gguf`, using `--memory 16000MB`,
  `--n-ctx 1024`, `--n-batch 512`, `--n-gpu-layers 0`, `--no-discovery` and
  `--single-peer`. Both loaded the weights, reserved the context and ended
  before accepting requests. The LLDB backtrace recovered in both was
  `EXC_BAD_ACCESS (code=1, address=0x0)` in `libsystem_platform.dylib` /
  `_platform_strlen + 4`; the stack above the libc frame was not recoverable by
  that run's unwinder.

  Exact commands of that task's tree, with paths relative to the repository
  (the `--model` path they use was removed later; today the reproduction is a
  single-node deployment through the UI with the same parameters):

  ```bash
  lldb --batch -o 'settings set target.env-vars POM_HOME=target/bench-home POM_UI_DIST=apps/frontend/dist' -o run -o 'bt all' -o quit -- ./target/release/pom --model target/qwen2.5-0.5b-instruct-q4_k_m.gguf --memory 16000MB --port 52777 --inference-port 18181 --no-discovery --single-peer --start-layer 0 --end-layer 23 --n-ctx 1024 --n-batch 512 --max-new-tokens 64 --n-gpu-layers 0 --no-thinking
  lldb --batch -o 'settings set target.env-vars POM_HOME=target/bench-home POM_UI_DIST=apps/frontend/dist' -o run -o 'bt all' -o quit -- ./target/release/pom --model target/gemma-3-270m-it-Q8_0.gguf --memory 16000MB --port 52778 --inference-port 18182 --no-discovery --single-peer --start-layer 0 --end-layer 23 --n-ctx 1024 --n-batch 512 --max-new-tokens 64 --n-gpu-layers 0 --no-thinking
  ```

  To isolate the finding from that change, `origin/feat/implantar3` at
  `4868f79764f906eeb5e71e49e028a0245b6376a4` was exported to a clean tree,
  built with the same `llama.cpp` checkout and run with the same pair of
  commands, only swapping the binary for `target/clean-origin-pom` and the ports
  for `52779/18183` and `52780/18184`. Qwen and Gemma reproduced the same
  `EXC_BAD_ACCESS` at `_platform_strlen + 4`. The problem was not investigated
  in that task.

- (not a defect, it is today's behavior) Port in use: `runtime::free_port` finds
  whoever holds the port (`lsof -ti :PORT` on Unix, `netstat -ano`+`taskkill`
  on Windows) and kills it (SIGKILL) before starting the server. If it cannot
  (an unidentifiable holder, or a port still held after 5 s), the reason goes to
  `status.detail` as `Failed` on the first attempt. The history lives in
  `<project_root>/infer_server_<port>.log` (`runtime::log_file_path`, pointed to
  by `status.log_path`), because stdout does not keep it.
- **No E2E coverage**: the four failure modes of the legacy direct mode still
  have no end-to-end bench; the new remote transport has unit tests for batch,
  order, error, termination and backpressure.
- **Result of the 2026-09-04 review:** the reference measurement on
  `origin/feat/implantar3` had 1022 green, eight red and took 609.55 s. After
  that fix, the same measurement took 27.72 s, with 1049 green and five red.
  `direct_lab::the_cost_per_token_does_not_depend_on_where_the_entry_point_is`,
  `direct_lab::direct_prefill_preserves_the_absolute_position_with_and_without_reuse` and
  `link_lab::an_unavailable_channel_degrades_to_the_per_request_path` turned
  green. In both `direct_lab` tests, the cause was the route memory of
  `84e7141`: the candidate list stayed empty and delivery was never tried. The
  delivery count now separates results from transport receipts, and late
  telemetry does not hold the result. The `link_lab` test also turned green with
  the ladder that does not disable the per-request route.
- Still red, with no change of expectation in that task, three of
  `deploy::whole_route_tests` (catalog/slice state) and
  `webui::tests::merge_parts_joins_two_slices_into_a_whole`
  (`head_count_kv_by_layer` metadata). Those are other subsystems'
  expectations and stay noted without a drive-by fix.
- The count above repeats the review's measurement with
  `--skip prefix_reuse_lab`. The tests
  `spec_lab::a_budget_above_n_batch_is_not_truncated` and
  `prefix_reuse_lab::unknown_session_recovery_is_bounded` now isolate,
  respectively, the generation budget and the cold attempt limit; the
  total of frames stays determined by the prefill window.
- **The `webui` tests that forward to a fake machine are flaky under load**
  (2026-09-03): each round of the full suite drops **one** of them, and not
  always the same one - seen
  `webui::leave_tests::leaving_for_a_peer_is_forwarded_to_that_peer` and
  `webui::peer_runtime_tests::the_peers_health_is_forwarded_whole`, both passing
  alone. They bring up a fake server on an ephemeral loopback port
  (`gateway::testkit`, a client with a 600 s timeout, so it is not a deadline)
  and check the forward arrived. The cause was not investigated here; what is
  measured is that the flakiness does not depend on the persistent channel - it
  shows up the same on the tree without it.
- **The forwarding hop's `network_ms` includes the next hop.** When the head is
  the gateway itself (`local` transport) and forwards to the next peer inside
  the same call, its `round_trip_ms` covers the next hop's whole round trip, and
  the head's `network_ms`/`net_rtt_*` show as network what is waiting on the
  downstream. Seen on 2026-09-29 (Qwen3.8, between two CUDA machines): a head
  with `round_trip_ms` 3041 = 573 of compute + 2419 of the next hop, and a
  "local network" of 208 ms per call in prefill. The per-hop totals do not add
  up with the clock.

Fixed (2026-09-18), kept here for context: the external MTP draft of
`qwen4exp` (Qwen3.8-Flash-Next) did not attach -
`check_tensor_dims: tensor 'output_hc_norm.weight' not found` in
`load_mtp_draft`. The cause had three parts in
`llama.cpp/src/models/qwen4exp.cpp` (fork `pieceofmindcloud/llama.cpp`,
`pom-tensor-parallel`): `load_arch_tensors` never computed `mtp_only` (the
pattern `qwen35.cpp`/`qwen3next.cpp`/`deepseek4.cpp` already use to free the
trunk when the file is only the draft); the architecture never declared the
tail's `layer.nextn.*` tensors; and `build_arch_graph` never dispatched to a
`graph_mtp` on `params.gtype == LLM_GRAPH_TYPE_DECODER_MTP`. Fixed by porting
the `qwen35.cpp`/`deepseek4.cpp` pattern to `qwen4exp.cpp` (the three pieces
above) and fixing the width of the `h_nextn` exported by the target, which came
out already collapsed by the final mixer instead of the wide `n_embd*hc` state
the draft head expects. Proof by synthetic reproduction (failing before, MTP
context reserving after) in
`crates/llama-engine/src/llm/model.rs::mtp_draft_tests`.

## Version note

Documented from the sources on the `fm/rdma-gateway-remoto-por-token` branch
(2026-09-01) and kept up to date since: `inference/infer_server/`,
`inference/prefill/`, `inference/direct.rs`, `inference/direct_lab.rs`,
`inference/spec_lab.rs`, `inference/relay.rs`,
`inference/runtime.rs`, `routing/pipeline.rs`, `backend/gateway/`,
`backend/openai.rs`, `backend/messages.rs`, and the AGENTS.md files of
`inference/`, `routing/`, `backend/` and the root. The engine modules named here
(`llm`, `wire`, `mtp`, `collective`, `gguf`, `multipart`, `kv_budget`, `moe`,
`sysmon`) are not in this repository: they are the `llama-engine` crate of the
private `pom-plugin-llama.cpp` repository, re-exported by
`crates/node/src/main.rs` (`pub use llama_engine::{...}`), which is why the node
code reaches them as `crate::llm::`, `crate::wire::` and so on. Where code and documentation
diverge, the code wins and the divergence is noted as a defect.
