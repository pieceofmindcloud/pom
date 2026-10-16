# FreeToken goal and the MoE path

This document follows the `feat/ftk` goal. It records the measurements,
decisions and limits of the new MoE path without changing the contract of the
dense path, which keeps using `llama.cpp`.

Machine names are generic on purpose: "the CUDA test server" is the
multi-GPU Linux CUDA machine used for most measurements, and "the operator" is
whoever ran the cluster.

## Source and scope

The primary source of the study was an external FreeToken study report (kept
outside this repository), read in full before the work. FreeToken was used as a
source of design and algorithms, not code. No weight travels over the network
because of slicing.

The goal will be delivered in cohesive phases:

1. local measurements and baseline documentation;
2. KV budget in bytes, context rebuild without rereading weights, prefill
   cancellation and stop strings;
3. MoE detection with a reversible fork;
4. an own MoE engine with one quantization format at a time and a global LRU
   cache;
5. CPU/GPU co-execution based on measured bandwidth, initially in the CUDA
   NVFP4 executor.

### Phase 1: S9 delivered

The gateway now accepts `stop` in the OpenAI format and `stop_sequences` in the
Anthropic format, plus the equivalent field in `/api/generate`. All of them go
through a common incremental `StopMatcher`. It only releases text when the next
suffix can no longer complete any stop; on a match, it drops the stop and the
text after it, ends the generation and propagates `finish_reason=stop` plus
`stop_sequence`. The protocol between peers did not change.

The unit tests cover a stop split across events, a pending prefix, UTF-8,
duplicates and multiple stops. The native suite was run after materializing
locally the `llama.cpp` required by the build script; the consolidated numbers
are in the validation section at the end of this document.

Dense models must stay on the existing path. Selecting the MoE path will be an
explicit decision based on GGUF metadata, with a test that forbids a dense model
from entering the new engine.

### Phase 2: S1, prefix cache between turns

Even without the performance baseline, following the decision recorded by the
operator, the prefix cache was implemented for state safety and work reuse.
`PrefixCache` is global to the gateway, with a nominal capacity of 32 entries
and LRU among idle entries. Busy leases are not evicted, so the total can exceed
the capacity until a generation ends. The policy keeps neither weights nor
tensors: each entry only keeps the model and hops key, the prompt text, the
`session_id` and the number of tokens confirmed by the head. A lease marks the
session as busy to stop two generations from using the same KV.

On a miss, an idle entry can be recycled so the cache does not require a second
KV slot when `max_sessions=1`; the new prefill resets that sequence before
decoding. Busy entries are never shared.

When the prefill finishes successfully, the gateway sends `/v1/session/rewind`
to every peer of the plan. Each peer removes the generated suffix from the KV
and confirms the resulting `n_past`. Only after all confirmations does the lease
enter the cache; any failure drops the entry and releases the session. On the
next turn, a hit keeps the route and the session, signals `reuse_prefix` on the
first chunk and the head tokenizes the new prompt. It computes the longest
common token prefix, rolls the KV back to that position and propagates
`start_pos`/`next_pos` on the following chunks. The peers that do not tokenize
only get the hidden state, but run at the explicit position, avoiding silent
holes between stages. (Today generation runs on a separate KV sequence and reuse
is append-only; see `inference.md`, section 3.1.)

The fields are additional metadata of the existing inference frame; the binary
envelope and the public APIs did not change. The differential self-test of
`prefix_cache_lab.rs` runs operations against a simple reference and requires a
proxy that changes `cached_tokens` to be detected. The pure cache test also
covers the longest prefix, busy leases and LRU eviction. The native suite was
run with the local symlink documented in `build-and-run.md`. No performance
gain is claimed; the numbers were left for a later measurement by the operator.

### Phase 3: MoE detection and a reversible fork

`gguf::read_info` now reads `<architecture>.expert_count` and
`<architecture>.expert_used_count`. Classification is only positive when
`expert_count > 1`; file name, load failure and architecture names without that
evidence do not select MoE. In `auto`, a dense or missing header goes
explicitly to `dense/llama.cpp`.

The `POM_INFERENCE_MODE` override accepts `auto`, `dense` or `moe` when the
deploy is in `auto`; an explicit choice persisted by the deploy beats the
environment. `dense` forces the existing path even for a MoE GGUF. `moe`
refuses a model without `expert_count > 1`, instead of silently diverting. This
phase's MoE path was initially `moe/llama.cpp-delegated`, a temporary
delegation to `LlmEngine`. Phase 4's later wiring replaced that operational
selection with `moe/q4_0`; the reference to the delegation only remains to
explain the historical state and the telemetry test that guarantees a
delegation is never labeled as the MoE engine. The classification tests cover
dense, MoE, override and missing metadata. The isolated GGUF and executor tests
passed in a `rustc` harness with 15 cases. The `cargo test -p node` suite also
passed with the local `llama.cpp` symlink materialized during validation.

### Phase 4: global cache, GGUF loader and the first MoE block

The design was reimplemented in Rust from FreeToken pinned at
`bd372b630a028e3faa51f4ab0ef6a98c2f2de501`. The `ExpertCache` instance is
shared by the server and indexes each entry by model, layer, expert and format.
An `ExpertLease` counts active references; eviction only picks the idle entry
with the lowest `last_used`, so a request in progress does not lose its weight.

`admission::MemoryAccount` does not use a VRAM constant. It subtracts from the
measured free baseline the weights, fixed buffers and the activation reserve,
and crosses that ceiling with a new free memory read before each miss. The local
probe uses `MemAvailable` on Linux, `vm_stat` on macOS and
`GlobalMemoryStatusEx` on Windows. A future VRAM probe can implement the same
trait without changing the policy.

The first format is packed Q4_0. `format_q4_0.rs` validates the GGML geometry,
mmaps the GGUF, copies only the requested expert's banks into the cache, decodes
the 32-element blocks, runs gate-up, gated SiLU and down. With
`--features cuda`, `block.rs` keeps the admitted payload in the resident cache
and the heterogeneous bridge runs the three phases on the GPU; without CUDA, the
same decoder runs explicitly in `CPUReference`. The loader accepts both a fused
`ffn_gate_up_exps.weight` and separate `ffn_gate_exps.weight` +
`ffn_up_exps.weight`, recombining the latter into the executor's internal
payload. `router.rs` picks top-k deterministically and renormalizes the
weights; `block.rs` connects that selection to `Q4_0Executor`, using a global
key per model, layer, expert and format. No line of FreeToken was copied.

The isolated harness passed with 28 tests, including a synthetic GGUF parser,
an F32 router, the block's forward, a cache hit, global eviction, measured
memory, active leases and geometry rejections. That proves the engine's pure
boundaries, not a complete model. The parser also accepts large GGUF arrays,
bounding them by the file's remaining space instead of an arbitrary ceiling.
The HTTP runtime now detects the dtype of the expert banks, opens the matching
own decoder and computes the router logits before applying the block to the
received hidden state or to the residual that will be forwarded. Q4_0 and NVFP4
share the same cache and dispatch boundary, and Q4_K (12), Q6_K (14),
IQ3_XXS (18), IQ3_S (21), IQ2_S (22), IQ4_XS (23) and MXFP4 (39) now also go
through that selection. The K-quant decoder validates the GGUF order
`[columns, rows, experts]`, the 256-element blocks and the fused or separate
convention of the gate/up banks before copying an expert. The frame stays f16
and carries no weights. In the separate convention, gate and up have the GGUF
geometry `[hidden, intermediate, experts]`; the loader must not swap `dims[0]`
and `dims[1]` when turning that metadata into `hidden` and `intermediate`. The
callback also validates the rank and the dimensions received from llama.cpp and,
on a divergence, reports the observed and the expected shape. The NVFP4 decoder
applies the same dimension rule: in GGUF, a weight matrix is
`[input, output, experts]`. So gate/up use `[hidden, intermediate, experts]` and
down uses `[intermediate, hidden, experts]`; swapping those axes produces a
runtime with the wrong hidden width and a refusal on the first forward, instead
of truncating the residual. Attention, the operations that do not belong to the
wired block and the final token sampling still delegate to `LlmEngine`; so this
delivery does not claim to be a complete MoE transformer. The operator took on
the end-to-end proof with a real model, to be validated outside that
environment, without changing the dense path.

The K-quant block was validated locally with Q2_K, Q3_K, Q4_K and Q6_K
fixtures, in both naming conventions, and with `MoeRuntime::open` followed by
`forward_batch`. The initial failure was an inversion in the validator: GGUF
declares the column dimension in `dims[0]`, while the mat-vec contract receives
`rows, columns`. The validator now checks both dimensions and the experts axis
before the forward. The rectangular test uses `hidden=2048` and
`intermediate=512`, so it fails if the axes are swapped. There was no physical
proof of Qwen3.6 in that worktree yet.

On 2026-09-02, the proof of a real Q6_K block closed the cause of the
`k_quant` path's NaN: the GGML layout is `ql[128]`, `qh[64]`, `scales[16]` and
`d[2]` at the end of the 210 bytes. The Rust decoder and `k_dot_q6_k` read `d`
at the start, also shifting every other field. The fix uses the real
`block_q6_K` offsets in both executors. The proofs
`q6_k_gate_up_eight_experts_hidden_2048_is_finite` and
`heterogeneous_q6_k_gate_up_eight_experts_hidden_2048_is_finite` use finite input
and eight experts, and the first also checks the expert indexes loaded from the
GGUF. CUDA validation requires `nvcc` on the GPU machine; there was neither a
Qwen3.6 GGUF nor a CUDA compiler in that local worktree.

That validation also fixed a pre-existing inconsistency in the IQ decoder:
`IQ4_NL` has 32-element GGML blocks, while the other prioritized IQ formats use
256-element blocks. The bytes per row computation and the size passed to the
dequantizer now respect that difference; the local dequantization proof no
longer reads past an 18-byte block.

The prioritized IQ-quants use the same loader and the same cache boundary. The
own executor keeps parser, mmap, router, top-k and mat-vec; expanding the block
uses the GGML dequantization primitives linked by the native crate, without
duplicating the official codebooks in Rust. Local fixtures cover IQ3_S, IQ2_S
and IQ4_XS, both gate/up names and the runtime's end-to-end run. The CUDA bridge
covers IQ3_XXS, IQ3_S, IQ2_S and IQ4_XS; a dtype outside the external decoders
is still refused by the dispatch with its ID and name.

### Phase 4A: F32, F16 and BF16

The scalar dtypes `F32` (0), `F16` (1) and `BF16` (30) now have their own CPU
decoder in `format_plain.rs`. The loader validates rank 3, the logical order
`[columns, rows, experts]`, every dimension, dtype and the mmap limits before
copying an expert into the cache. The fused `ffn_gate_up_exps.weight` and the
separate `ffn_gate_exps.weight` plus `ffn_up_exps.weight` conventions are
accepted; down still requires `ffn_down_exps.weight`. The scalar router can be
F32, F16 or BF16 and is decoded by the dtype declared in the tensor itself.

The own block applies the same top-k, LRU cache and hidden-state contract as the
other formats. A quantized dtype handed to this decoder is refused with the GGML
ID and name, and the general dispatcher now routes 0, 1 and 30 to this family
instead of trying Q4_0.

There was no F32, F16 or BF16 MoE GGUF available in that worktree for a
physical proof. The synthetic fixtures cover the three dtypes, both naming
conventions, the forward with cache and the refusal by name, so these formats
are **implemented, but not proven on a real GGUF**. No performance number is
claimed.

### Phase 4B: Q4_1, Q5_0, Q5_1 and Q8_0

The classic blocks `Q4_1` (3), `Q5_0` (6), `Q5_1` (7) and `Q8_0` (8) have their
own CPU decoder in `format_legacy_quants.rs`. Block sizes and dequantization
formulas follow the active definition in `ggml-common.h` and `ggml-quants.c`:
F16 scales and minimums, a separate fifth bit for Q5 and signed quants for Q8.
The loader validates divisibility by 32-element blocks, the
`[columns, rows, experts]` dimensions, a uniform dtype, offsets and file
limits. Fused and separate gate/up share the same cache and router path, but no
dtype is accepted by another's decoder.

The local tests cover a reference vector for each block, the expert's forward,
both naming conventions and the `MoeRuntime` forward. There was no
Q4_1/Q5_0/Q5_1/Q8_0 MoE GGUF available in that worktree, so these formats are
**implemented, but not proven on a real GGUF**. No performance number is
claimed.

There is no performance number in this phase. Native validation was run with
the local `llama.cpp` symlink documented in `build-and-run.md`; that artifact
does not enter the commit and does not replace the end-to-end proof with a real
model.

### Phase 4C: MXFP4, TQ1_0, TQ2_0, Q1_0 and Q2_0

The low-width formats `MXFP4` (39), `TQ1_0` (34), `TQ2_0` (35), `Q1_0` (41) and
`Q2_0` (42) have their own decoder in `format_low_bits.rs`. Each format declares
its block, packed size and dequantization formula. MXFP4 uses an E8M0 exponent
per block; TQ1/TQ2 use their ternary codebooks; Q1/Q2 use the matching GGML bit
fields. The loader validates dtype, rank, the `[columns, rows, experts]`
geometry, offsets and mmap limits before admitting any expert into the cache.

The path accepts fused and separate gate/up, computes the per-expert slice
without confusing a row's bytes with a block's bytes and wires the executor to
`MoeRuntime`. There are vector, mmap source, cache, router, naming convention
and end-to-end forward tests for the five dtypes. There was no real MoE GGUF of
these formats available in that worktree; so they are **implemented, but not
proven on a real GGUF**. No performance number is claimed.

### Phase 4D: remaining K-quants

`format_k_quants.rs` now also covers `Q2_K` (10), `Q3_K` (11), `Q5_K` (13) and
`Q8_K` (15), besides `Q4_K` and `Q6_K`. Each format uses its own GGML block size
and field layout: quantized scales and minimums for Q2/Q4/Q5, high bits for
Q3/Q5 and Q8's scalar block. Dispatch by dtype, the bytes-per-row computation and
the `[columns, rows, experts]` validation are shared without accepting a block
with another format's decoder.

The tests cover dequantization of the four new formats, per-expert slices with
fused and separate gate/up, cache and the `MoeRuntime` forward for all six
K-quants. There was no real K-quant MoE GGUF available in that worktree; these
formats are **implemented, but not proven on a real GGUF**. No performance
number is claimed.

### Phase 4E: remaining IQ-quants

`format_iq_quants.rs` now covers `IQ2_XXS` (16), `IQ2_XS` (17), `IQ3_XXS` (18),
`IQ1_S` (19), `IQ4_NL` (20) and `IQ1_M` (29), besides `IQ3_S`, `IQ2_S` and
`IQ4_XS`. The executor selects the packed structure and the C dequantization
routine matching each GGML dtype; no IQ codebook is used for another. Parser,
mmap, geometry validation, cache, router and top-k stay in the own Rust engine.

The tests cover the nine active IQ-quants, a truncated payload, refusing a dtype
outside the enumeration, fused and separate gate/up and the `MoeRuntime`
forward. There was no real IQ MoE GGUF available in that worktree; the six new
formats are **implemented, but not proven on a real GGUF**. No performance
number is claimed.


### Heterogeneous UD crossing

`format_heterogeneous.rs` keeps the dtype and geometry of each expert role
separately. The callback and the Rust inventory speak the same GGML shape
`[columns, rows, experts]`, while the route's ids and weights stay in token-major
order `[token, slot]`. On the CPU, the batch temporarily groups the entries by
expert and walks each gate, up and down row once for every token that selected
it; that avoids the repeated full decoding that made large combinations fail as
a timeout in prefill. The output is rebuilt at each token's original position
and keeps the route's weights, for both fused and separate banks.

That is the combination observed on the real `deepseek4` on the CUDA test
server: IQ3_XXS (18) on gate/up and MXFP4 (39) on down, with 256 experts and
top-6. Those two formats have external CPU decoders and a heterogeneous CUDA
bridge; the plan keeps the experts out of llama.cpp's weight buffer and batched
execution does not fall back to token-by-token execution. For a combination
without a CPU decoder or a CUDA bridge, `MoeRuntime::external_executor_plan`
returns the reason with the dtypes and the server refuses the load before
opening the model; there is no silent return to the native MoE.

### Hop wiring and telemetry

`infer_server` keeps the dense path on `llama.cpp`. When the GGUF selection is
MoE, `MoeRuntime` is only opened for that path and the local router picks the
selected decoder's experts. Ownership of the boundary block is explicit: a
received hidden state or a residual bound for the next hop is only handed to the
MoE runtime when the selected path is MoE; on the terminal hop the residual
capture also calls the runtime before sampling. The executor is only recorded
after the forward returns successfully, so a MoE selection without a real call
still shows up as a divergence. The wire keeps f16 as the default for the
hidden state; no weight travels over the network. Attention, logits and the
remaining operations stay in `LlmEngine`, which provides the remaining compute
boundary around the own block.

Each inference answer publishes `selected_path`, `executed_engine`, the
`executed_engines` list and the `engine_mismatch`/`mixed_execution` flags. The
first comes from the GGUF selection; the others are only recorded after the
llama.cpp decode or the MoE forward return. `/health` publishes `selected_path`
and the MoE executor's effective state (`moe_requested`, `moe_executor`, dtypes
and reason), because a selection without an executor must be visible even
before a request. The gateway aggregates the fields per phase of each hop,
without deducing them from the model name, the architecture or timings. The UI
shows the selected path and the observed engine; mixing and divergence stay
visible as a diagnosis, never reduced to a single label.

### CUDA executor diagnosis

Phase 5's first instrument also publishes `moe_executor`, `moe_executors`,
`moe_executor_dtypes`, `moe_executor_reason` and `moe_forward_ms`. The backend
only enters those fields after `MoeRuntime::forward_batch` returns
successfully. `cuda` means the CUDA NVFP4 executor; `cpu-reference` means the
reference decoder; in that case telemetry lists the concrete GGML dtypes and
says there is no CUDA bridge; `not-run` is a MoE selection with no recorded
forward and `mixed` reports more than one backend on the same hop. The gateway's
`PhaseTrace` keeps the fields per hop and per phase. A binary without the `cuda`
feature uses the own CPU executor for the supported formats. NVFP4 still refuses
the load with the executor's message by name, unless `POM_MOE_NVFP4_BACKEND=cpu`
explicitly asks for the reference executor.

On the real Qwen3.6-35B-A3B-UD-IQ3_S, the bridge is now allowed to publish
`moe_executor=cuda`, `moe_executor_dtypes=[21,22,23]` and no CPUReference
reason, after the differential proof of the three dtypes on the real GPU. Q4_0
uses the same CUDA contract with GGML dtype `2`; validation keeps 32-element
blocks for that format, instead of applying the 256 requirement used by the
K/IQ-quants. Q5_K enters through GGML dtype `13`, a 176-byte block of 256
elements (`d[2] dmin[2] scales[12] qh[32] ql[128]`): it is Q4_K with a high bit
plane, and `k_value_q5_k`/`k_dot_q5_k` reproduce the CPU decoder's
`dequantize_q5_k_block`.

### One dtype without a bridge drops the whole route

`Ornith-1.5-35B-A3B-Q5_K_S` measures 2 Q4_0 dtypes and 13 Q5_K in the experts:
while Q5_K had no kernel, the whole route fell to `CPUReference` even with the
Q4_0 bridge ready. Dynamic quants (UD) mix dtypes **per expert role**, so the
bridge's coverage is checked by the concrete dtype in telemetry
(`moe_executor_dtypes`), never by the file name. Proving the gain on a model
whose dtype already had a bridge misleads the measurement.

### `offloaded 42/42 layers to GPU` does not talk about the experts

With `moe_external_executor` on, llama.cpp loads the experts' metadata but
**does not allocate their weights** (`EngineConfig::moe_external_executor`).
So its `offloaded N/N layers to GPU` is true about what it owns itself -
attention, norms, output - and omits most of the model. On the CUDA test server
that showed up as `offloaded 42/42` next to only ~1.6 GB of model buffers on GPU
(CUDA0 615.83 MiB + CUDA1 978.08 MiB) for a 35 B model, with `CPU_Mapped` at
333.44 MiB: the operator's reading, "the whole model is in RAM", was right. The
experts are housed by POM's MoE executor, and where they live depends on the
chosen executor: in the device-resident cache when there is a CUDA bridge for
**all** dtypes, mmapped in RAM when it is `CPUReference`. The `infer_server`
startup log says so explicitly so the two lines do not contradict each other in
the reader's eyes. The physical proof of that GGUF had not been run in that
worktree yet, so the expected telemetry is a contract to verify, not an already
claimed result.

To separate a failure in the received data from a failure in the Rust read, run
the process with `POM_MOE_ROUTE_DIAGNOSTICS=1`. Each callback entry gets two
adjacent lines, `callback_entry_default` on the default target and
`callback_entry` on the `moe.route_diagnostic` target, as the first action,
before the validations. The pair tests the sink and a possible target filter at
the same point of the callback. Both log the raw pointers and counts; each
`false` return also gets a `callback_exit` line with the reason of the path that
aborted, including null arguments, geometry, normalization, metadata and
forward. The first call of each request and any non-finite route get the
detailed sample, without depending on the layer index. It logs raw values of
token 0, slot and offset in token-major order, pointers and bytes of the regions
read, the element count declared by llama.cpp versus expected by POM, the
gate/up/down metadata dtypes and the dtypes assumed in the callback's arguments.
After normalization, it logs the sample and the NaN counts again. The variable
is opt-in, off by default and does not change execution. When the process loads
its first `LoadedModel`, an extra line on the default target logs once the
variable's raw value and the gate's effective boolean. So the next deploy's log
shows whether the `OnceLock` recognized the environment, before the first route.

Before normalizing the route, the callback also validates that the input hidden
state is finite. After the forward, it validates the executor's output before
copying it to llama.cpp. NaN or infinity publishes a failure with layer, index
and stage (`hidden_input_non_finite` or `forward_output_non_finite`), stopping
the request instead of silently contaminating the next layer.

The ignored test
`llm::moe_route_tests::global_callback_emits_the_diagnostic_on_an_external_thread_and_records_the_gate`
installs the global dispatcher with an equivalent `.init`, creates a
`std::thread` outside the Tokio runtime and requires in the same sink the
configuration, `callback_entry_default`, `callback_entry` with the MoE target and
`callback_exit`. It passed locally with `RUST_LOG=info`; that proves the
transport in the test process, but does not replace the confirmation on the
deployed binary.

The `llama.cpp` callback receives the route in the token-major order that
results from `ggml_get_rows` with dimensions `[1, experts_used, tokens]`: slot is
the fastest index within each token. The Rust normalization validates and keeps
that order, without transposing. The unit test
`llm::moe_route_tests::the_llama_cpp_get_rows_route_already_arrives_in_token_major_order`
pins the contract against the physical sequence produced by the C++ builder.

The `resolve_fused_ops: ... enabled` messages do not prove a fused operation was
used: the probe also prints `enabled` when it finds no node of that type. In
`qwen35moe.cpp`, the MoE FFN still goes through the generic builder and the
shared expert goes separately through `build_ffn`; the real presence of fused HC
must be shown by the reservation's nodes or by the callback entry logs.

### Phase 5.1: CUDA bridge per dtype

This was the formal phase of the per-dtype bridge. The CUDA executor covers
NVFP4 (40), scalars, Q2_K/Q3_K/Q4_K/Q6_K and IQ3_XXS (18), IQ3_S (21), IQ2_S
(22), IQ4_XS (23) and MXFP4 (39) in the heterogeneous bridge. The differential
proof of Q2_K and Q3_K was run on the real GPU; the E2E of the Qwen3.6 GGUF was
still pending. The remaining IQ-quants, remaining K-quants, legacy and low-bit
stay in `CPUReference`, with the choice visible in telemetry. When a
heterogeneous combination contains a dtype without a CUDA bridge, the whole
layer uses `CPUReference` if there is an external decoder; without a decoder,
the load is refused before the model. Each tensor keeps its dtype and the
matching decoder is chosen per tensor.

Done criterion per block: a numeric proof against the GGML oracle for each
dtype, per hop/phase telemetry showing `cuda` without a CPU
`moe_executor_reason`, an E2E proof with a real heterogeneous GGUF when
available and a measurement on the CUDA test server of prefill, decode, H2D/D2H
bytes, allocations and bandwidth. The comparison must separate kernel and
transfer costs; no performance gain will be claimed just from a green suite.

### Phase 5.2: Q2_K/Q3_K on Qwen3.6 and parallel CPU fallback - 2026-09-04

The E2E reproduction provided by the operator set the baseline on the same
`Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf` on the CUDA test server: the `moe/q4_0` path
selected `cpu-reference` because of the Q2_K (10) and Q3_K (11) dtypes, with
`0.3 tok/s`. The `llama.cpp` path, under the same conditions, delivered
`160 tok/s`. The trace attributed `91936 ms` of `llama_decode` to the local
backend, not to the transport.

This phase adds the CUDA bridge for both formats. Q2_K uses 256-element blocks
of 84 bytes, with scales, quants, `d` and `min`; Q3_K uses 256 elements and 110
bytes, with high bits, quants, packed scales and `d`. The kernel does the
product directly in the GGML layout, without reinterpreting Q2_K or Q3_K as
Q6_K. The Rust validator, the heterogeneous bridge's geometry and the executor
choice now accept `10` and `11`; Q5_K and Q8_K stay outside the bridge and
cannot be sent to the kernel as another format.

The CPU fallback stopped running the route serially: distinct experts are
processed in scoped workers, each with its own output vector, and aggregation
happens in a stable expert order. The single-token route uses the same parallel
primitive. That keeps the sum deterministic and turns a worker panic into an
inference error. The layer 0 `POM_MOE_LAYER_INPUT` diagnostic is off by default;
it is only enabled with `POM_MOE_LAYER_INPUT_DIAGNOSTIC=1` (also `true`, `yes`
or `on`).

The own CUDA validation on the CUDA test server was run with `CUDAARCHS=120`,
`nvcc` 12.9 and a separate target. The executor's differential tests passed:

| proof | result |
|---|---:|
| Q2_K, Q3_K, Q4_K and Q6_K against the CPU decoder | 1 green test |
| heterogeneous route Q2_K gate + Q3_K up + Q4_K down against the CPU decoder | 1 green test |
| CUDA comparison tolerance | `abs <= 1e-3` or `rel <= 1e-5` |

The post-bridge E2E measurement of the Qwen3.6 GGUF still depended on the
operator putting that build in the shared deployment. Until that run, the
throughput numbers honestly were:

| path | before | after | reference |
|---|---:|---:|---:|
| MoE | `0.3 tok/s` | E2E pending | - |
| llama.cpp | - | - | `160 tok/s` |

The contract expected after deployment is `moe_executor=cuda`,
`moe_executor_dtypes=[10,11,21,22,23]` and no `moe_executor_reason`. The
per-phase breakdown must record TTFT/prefill, decode, `host_batch_prepare_ms`,
`cuda_kernel_ms`, H2D/D2H and tok/s; the CUDA suite does not replace that
measurement.

### Phase 5.3: batched Q6_K prefill and speculation - 2026-09-04

The operator's second pair of measurements isolated the prefill's cause. On the
same `Ornith-1.5-35B-Q6_K.gguf`, on the CUDA test server, with `n_batch=512`,
the Q6_K dtype already had a CUDA bridge, but MoE only delivered `7.7` and
`17.0 tok/s` of prefill, against `183.0` and `183.5 tok/s` for `llama.cpp`.
Generation stayed at `15.9` and `15.4 tok/s` on MoE, against `36.2` and
`38.2 tok/s` on dense. TTFT was `6.57 s` and `6.41 s` on MoE, against `3.21 s`
and `574 ms` on dense.

| path | turn 1 | turn 2 | note |
|---|---:|---:|---|
| MoE, generation | `15.9 tok/s` | `15.4 tok/s` | `2.3x` to `2.5x` below dense |
| MoE, prefill | `7.7 tok/s` | `17.0 tok/s` | `3,615.538 ms` and `4,883.604 ms` |
| llama.cpp, generation | `36.2 tok/s` | `38.2 tok/s` | reference |
| llama.cpp, prefill | `183.0 tok/s` | `183.5 tok/s` | reference |

In decode, the latency per call was close (`56.157` to `62.733 ms` on MoE,
against `54.744 ms` on dense), but dense produced several tokens per call: `14`
calls for `27` tokens, against `60` for `61` and `196` for `197` on MoE. That
makes MTP/speculation eligibility a second priority. MoE sampling also
accumulated `33.495 ms` of top-k over 61 tokens and `113.09 ms` over 197 tokens;
the measured bytes were about 28 times larger.

The first fix slice of that measurement acted on the largest measured hole: the
CUDA K pipeline keeps a device workspace per executor and reuses buffers across
calls, only allocating when the batch's capacity grows. Q6_K gate/up and down
use a cooperative per-row reduction with 128 threads; Q2_K and Q3_K use the same
dtype bridge and the heterogeneous route also uses cooperative reduction for the
covered formats. `FIT_MAX_UBATCH` stays `512`.

The synthetic benchmark at the real Q6_K geometry (`hidden=2048`,
`intermediate=512`, 28 tokens and 224 routes) measured `12.233 ms` of wall time,
with `11.653 ms` of kernel, `0.118 ms` of H2D, `0.194 ms` of D2H and
`0.157 ms` of allocation at the first capacity. It is a measure of the
executor, not an E2E promise. The CUDA suite on the CUDA test server passed with
`325` tests and zero failures.

On that phase's date the post-slice E2E still had to be collected on both
models. The historical table stayed pending until the new binary served the
same request:

| path | before | after | reference |
|---|---:|---:|---:|
| Qwen3.6 MoE | `0.3 tok/s` | E2E pending | llama.cpp `160 tok/s` |
| Ornith Q6_K MoE | `15.9`/`15.4 tok/s` | E2E pending | llama.cpp `36.2`/`38.2 tok/s` |

With the new evidence, the work order is: fix MTP in MoE's metadata-only
context; confirm speculation per call; attack the base cost of `5.8x` per call,
including the structural duplication and the early synchronization; only then
attack the prefill, which is still the worst gap per call. Telemetry stays
mandatory to confirm `moe_executor`, `moe_executor_dtypes`,
`moe_executor_reason`, MTP, TTFT, phases and tok/s after the deploy.


### Phase 5.4: telemetry scope and the first prefill slice - 2026-09-04

The operator's post-deployment pair used `Ornith-1.5-35B-Q4_K_M.gguf` and
confirmed the CUDA bridge was active (`moe_executor=cuda`). The result was still
far from dense: on the long turn, `11.3 tok/s` on MoE against `140.7 tok/s` on
llama.cpp; prefill stayed at `16.1 tok/s` against `1,343.6 tok/s` and the p50
per call was `2,515.7 ms` against `135.8 ms`. In decode, there were `197` calls
for `198` tokens on MoE, against `62` for `197` on dense; the p50 was
`84.6 ms` against `20.8 ms`. The 19-token prefix hit did not remove the
`3,106.96 ms` cost of the MoE prefill. That pair is the next slice's E2E
baseline, not an isolated proof of the bridge's gain.

The Q6_K pair with CUDA confirms the priority, but does not prove the bridge's
gain: Q6_K was already among the covered dtypes before the change. On the long
turn, MoE went from `15.4` to `16.4 tok/s`, while the prefill measured `17.0`
and `23.4 tok/s` against llama.cpp's `619.0 tok/s`. The CUDA prefill's p50 was
`1,826 ms` against `54.6 ms`, a 33x gap; decode was `50.3 ms` against `21.7 ms`
and there were still `196` calls for `197` tokens, against `73` for `197`. The
bridge's proof is still the Qwen `Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf`, which used
to fall to `CPUReference` because of Q2_K/Q3_K.

The apparent contradiction between `selected path: moe/q4_0` in the environment
and `llama.cpp` on the hops was investigated in the code. The `/health` used by
the environment card publishes the process's current state:
`InferState.inference_path`, the installed MoE executor and the effective
placement. Each `NodeTimings` of the trace, on the other hand, is created during
the request and publishes `selected_path`, `executed_engine`,
`executed_engines`, `engine_mismatch` and `mixed_execution`. They are distinct
scopes. The code also refuses an external MoE executor without a callback and
refuses an installed callback that observed no forward, so no silent dense
fallback is allowed in the current binary.

So the lines summarized as "llama.cpp engine" do not allow concluding a path
deviation without the `selected_path`, `engine_mismatch`, `mixed_execution`,
`node_id` fields and the raw trace of the same answer. The proven conclusion is
that the environment label was being used as if it were proof of the request,
although it is only a current snapshot. The telemetry card now shows `current
state` and `in this run` separately, shows `selected_path` in each phase and
warns when a current snapshot diverges from the observed path. If a new raw
trace brings `selected_path=moe/q4_0` and `executed_engine=llama.cpp`, it shows
up as an explicit divergence and must be treated as an old process or a real
deviation, never as a MoE result.

The first prefill optimization slice keeps the whole batch in the CUDA executor
and swaps the scalar gate/up and down kernels for a cooperative 128-thread
per-row reduction for Q2_K, Q3_K, Q4_K and Q6_K. The synthetic benchmark, with
`hidden=2048`, `intermediate=512`, 28 tokens and 224 routes, measured:

| dtype | wall | kernel | H2D | D2H | allocation |
|---|---:|---:|---:|---:|---:|
| Q4_K | `12.471 ms` | `11.907 ms` | `0.118 ms` | `0.194 ms` | `0.153 ms` |
| Q6_K | `11.517 ms` | `11.080 ms` | `0.116 ms` | `0.098 ms` | `0.152 ms` |

That measure is of the isolated executor on the CUDA test server, with
`nvcc 12.9`, `CUDAARCHS=120` and a separate target; it is not yet the E2E
after. `FIT_MAX_UBATCH` stays at `512`. The CUDA suite passed with `325` tests
and the new batched Q4_K differential passed; the E2E number after that slice
was left for the operator's integrated deployment.

### Phase 5.5: MTP on the MoE model and a control without speculation - 2026-09-04

The operator's control on the same `Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf`, with MTP
and speculation off on both engines, isolated the base cost. MoE did `16.9`
tok/s against llama.cpp's `93.7` on the long turn; both produced one token per
call, but the call's p50 was `57.4 ms` against `9.9 ms`, a `5.8x` gap. Prefill
stayed separate and much worse: `1,105.8-3,065.9 ms` per call on MoE against
`37.8-42.3 ms`, or `26x-73x`. So MTP does not explain the base cost, although it
can multiply the difference: in the previous pair dense did `3.8` tokens per
call.

The MoE deploy's `/health` showed `mtp_ready=false` with
`llama_decode no contexto MTP falhou rc=-1` ("llama_decode in the MTP context
failed rc=-1"). The cause is the combination of the metadata-only model with the
fork: `llama_init_from_model` inherits `moe_external_executor`, but the `nextn`
context had no callback. The fork refuses that decode so it does not try native
experts without buffers. The fix in `MtpDrafter` records whether the context
uses external ownership and `LlmEngine::set_moe_runtime` installs the runtime's
callback both on the target and on the MTP context. The CPU companion used when
the MTP context falls back to RAM stays native and does not get that hook.

The speculation path already existed: `should_draft` requires `mtp_ready` for
MTP, `mtp_draft` builds the batch and the tail checks each position through the
normal sampler. So once the external context is ready, the same path produces
larger batches on MoE again without changing the answer's distribution. The
telemetry expected after deployment is `mtp_ready=true`, without
`mtp_unavailable_reason`, and `speculation_active=true` when the draft reaches
the probability threshold.

The external MoE callback is already the replacement point of `build_moe_ffn` in
the fork used by the project: when external ownership is active, the builder
returns the callback's node and does not add the experts' native matmuls. In
Rust, the hidden/residual path now also fails closed if the external executor
is active without a hook, instead of running a second forward on the sidecar.
That keeps correctness and stops an incompatible binary from turning an install
failure into silent duplication. The vendored fork was not changed in that
slice.

The order of measurement and optimization stays: (1) confirm working MTP on
MoE; (2) confirm speculation per call; (3) reduce the `5.8x` base cost,
including any proven duplication and the early synchronization; (4) attack the
prefill, which is still the largest absolute gap. The E2E measurement after the
MTP fix was left for the operator's integrated deployment.

### Phase 5.6: diagnosis of the session reservation - 2026-09-04

Reviewing the window between the good and the bad binary found no regression on
the dense path. `99d8977` only installs the external callback on MTP when
`moe_external_executor` is active; `74ab819` keeps log capture off the path when
it is disabled; and `84e7141` queries the route map on send, not during the
local `llama_decode`. So none of those commits explains the jump from `20.8 ms`
to `87.5 ms` per call.

The controlled counterfactual was run on the CUDA test server with binary
`4868f79`, the real GGUF `Ornith-1.5-35B-Q4_K_M.gguf`, GPU `99`,
`n_batch=512`, MTP on and the same prompt and parameters. The only variable was
`max_sessions`:

| configuration | node_p50_ms per call | note |
|---|---:|---|
| `max_sessions=1` | `20.944 ms` | in line with the operator's good binary |
| `max_sessions=2` | `105.3-124.4 ms` | same binary, no code change |

In the second case, `/health` confirmed `n_ctx_total=500224`,
`n_ctx_per_session=250112`, `kv_seq_max=4` and `max_sessions=2`; the measured
usage of the two RTX 5060 Ti stayed at about `15.3 GiB` per card. The difference
reproduces the fit and KV reservation effect that contaminates the comparison,
although the own harness has a higher absolute latency than the operator's
deployment. The result rules out the hypothesis of a regression introduced by
the MTP callback on the dense path and identifies `max_sessions=2` with a
`250k` window as the cause of the measured case.

The details card now shows a localized warning when the effective `/health`
combines multiple sessions with KV measured on a device: total context, context
per session, fit state and the latency risk from VRAM pressure. A single session
or KV on the CPU does not produce the warning. The change is diagnostic and does
not change dense selection, MTP, transport or the MoE executor. To compare
throughput with the baseline, pin `max_sessions=1` or record explicitly the
total context and the VRAM usage of each run.

#### E2E procedure for the operator's deployment

When the integrated deployment is ready, the operator must measure each model in
two separate selections, `dense` and `moe`, keeping the same machine, GPUs,
`n_ctx`, `n_batch=512`, `n_gpu_layers`, number of sessions, temperature, prompt
and `max_tokens`. The selection must be made in the deploy and the process must
be restarted through the deployment's normal cycle; do not try to switch
`POM_INFERENCE_MODE` on a process that is already serving.

In each selection, first confirm the inference port and keep the executor
telemetry:

```bash
MODEL=Ornith-1.5-35B-Q6_K.gguf
GATEWAY=http://<gateway-host>:8080
INFERENCE=http://<gateway-host>:8092

curl -fsS "$INFERENCE/health" |
  jq '{selected_path,moe_requested,moe_executor,moe_executor_dtypes,moe_executor_reason,n_ubatch_effective,mtp_ready,mtp_unavailable_reason}'
```

Then run the same request in each mode. `tee` keeps the raw JSON; `jq` prints
generation, TTFT, prefill, decode, call count and the per-hop telemetry:

```bash
MODE=dense # repeat with MODE=moe after selecting the matching deploy
PROMPT='Write hello world 10 times.'
curl -fsS "$GATEWAY/api/generate" \
  -H "authorization: Bearer $POM_API_KEY" \
  -H 'content-type: application/json' \
  --data "$(jq -nc --arg model "$MODEL" --arg prompt "$PROMPT" \
    '{model:$model,prompt:$prompt,max_tokens:64}')" \
  | tee "ornith-q6k-${MODE}.json" \
  | jq '{tokens_generated,tokens_per_sec,duration_ms,timings,trace:{input_tokens,output_tokens,hops:[.trace.hops[] | {node_id,start_layer,end_layer,prefill:(.prefill | {selected_path,executed_engine,executed_engines,engine_mismatch,mixed_execution,processing_ms,calls,node_p50_ms,node_p95_ms,moe_executor,moe_executor_dtypes,moe_executor_reason}),decode:(.decode | {selected_path,executed_engine,executed_engines,engine_mismatch,mixed_execution,processing_ms,calls,node_p50_ms,node_p95_ms,moe_executor,moe_executor_dtypes,moe_executor_reason})}]}}'
```

Repeat the same block for `Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf`. In the MoE result,
require `moe_executor=cuda`, dtypes `[14]` on Ornith and `[10,11,21,22,23]` on
Qwen, without `moe_executor_reason`; on dense, require
`moe_executor=not-requested`. On Qwen MoE after this fix, also require
`mtp_ready=true` and `mtp_unavailable_reason=null`. Compare
`timings.generation_tokens_per_sec` and `tokens_per_sec` as generation,
`timings.prefill_tokens_per_sec` as prefill and `timings.ttft_ms` as TTFT. Also
record from `trace.hops[].prefill` and `.decode` the fields `processing_ms`,
`calls`, `node_p50_ms`, `node_p95_ms`, bytes and `moe_executor_diagnostics`.
Keep the full log interval to check that `POM_MOE_LAYER_INPUT` only shows up
when `POM_MOE_LAYER_INPUT_DIAGNOSTIC=1` was explicitly requested.

#### E2E procedure for the impact of logs

The impact test must compare the same model, machine, plan, prompt and
parameters with capture off and on. The switch is applied by the principal,
without a restart, and the generation call stays on the public route:

```bash
MODEL=Ornith-1.5-35B-Q6_K.gguf
GATEWAY=http://<gateway-host>:8080
TOKEN='<cluster-connection-key>'
PROMPT='Write hello world 10 times.'
BODY=$(jq -nc --arg model "$MODEL" --arg prompt "$PROMPT" \
  '{model:$model,prompt:$prompt,max_tokens:64}')

for enabled in false true; do
  curl -fsS -X POST "$GATEWAY/api/ui/network/logs" \
    -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' \
    --data "{\"logs_enabled\":$enabled}" >/dev/null
  curl -fsS "$GATEWAY/api/generate" \
    -H "authorization: Bearer $POM_API_KEY" \
    --data "$BODY" >/dev/null
  for run in $(seq 1 10); do
    curl -fsS "$GATEWAY/api/generate" \
      -H "authorization: Bearer $POM_API_KEY" \
      --data "$BODY" > "cluster-logs-${enabled}-${run}.json"
  done
  jq -s '{runs:length,
    mean_generation_tok_s: ([.[].timings.generation_tokens_per_sec // 0] | add / length),
    mean_total_tok_s: ([.[].tokens_per_sec // 0] | add / length),
    mean_ttft_ms: ([.[].timings.ttft_ms // 0] | add / length)}' \
    cluster-logs-${enabled}-*.json
done
```

Keep the twenty JSON files and record in the report the system, commit, model,
GPU, `n_ctx`, `n_batch=512`, `n_gpu_layers`, sessions, temperature and the ten
values on each side. Compare the median and the mean of
`generation_tokens_per_sec` and `tokens_per_sec`; also check that
`GET /api/ui/cluster-logs` contains lines when `enabled=true` and that the drop
counter stays explicit. Repeat swapping `MODEL` for Qwen3.6 if that model's MoE
measurement is also part of the delivery. Local capture uses a bounded
`try_send` queue, so any increase must be investigated without turning the send
into a synchronous wait.

The historical investigation of the Gemma4 measurement found, before the current
external callback wiring, a divergence older than any kernel optimization. The
context captured `l_out-29`, but in
`vendor/llama-cpp-sys-2/llama.cpp/src/models/gemma4.cpp` that tensor is created
after `build_moe_ffn`, `ffn_post_norm`, the residual sum and `build_cvec`. The
capture callback copies the tensor and returns `true`; so the `llama_decode` call
continues through the graph. Then `infer_server::apply_moe_hidden` called the
`MoeRuntime` after `take_residual`. That added a pass over the experts to
llama.cpp's full pass, instead of replacing `build_moe_ffn`.

The minimal counterfactual was identified in that version's callback itself:
returning `false` on finding `l_out-29` would only drop the nodes after that
tensor, but would not stop the `build_moe_ffn` that already produced it. So that
counterfactual does not remove the duplicated work; the structural fix needed to
move the boundary to before the MoE FFN and inject the output into the rest of
the block. The fork's current source already does that replacement in
`build_moe_ffn`: with `moe_external_executor`, the builder returns the custom
node before the native matmuls. Rust also stops applying the sidecar to the
hidden state when the hook is installed and refuses closed if the external
executor has no hook. So the historical profile must not be used to claim
duplication in the current binary; it must be repeated on the integrated
deployment and checked through the `executed_engine`, `engine_mismatch` and
`mixed_execution` fields. The measurement also showed a second possible cause,
not yet quantified in isolation: the current CUDA executor allocates buffers and
copies hidden, gate/up and down from host to GPU on each call, synchronizes the
kernels and copies the result back. That profile should only be optimized after
separating the transfer cost from the kernel cost.

The Gemma4 renderer reads `tokenizer.chat_template` from the GGUF itself,
validates the markers `<bos>`, `<|turn>`, `<turn|>`, `<|think|>`, `<|channel>`
and `<channel|>`, and reproduces the `enable_thinking` and
`add_generation_prompt` control. There is no Qwen fallback nor an invented
system block when the template does not ask for one. A Gemma4 template without
the expected markers is refused by name. Generation ends on EOS or on the
end-of-turn sequence tokenized by the same vocabulary; the control marker is not
sent to the client. The ignored test
`gemma4_hello_world_request_uses_moe_and_stops_on_its_turn` exercises the real
`/api/generate` route when `POM_FTK_E2E_URL` points to a Gemma4 gateway.

The real Gemma4 inspected on the CUDA test server uses the separate names, with
dimensions `[2816, 704, 128]` for gate/up, `[704, 2816, 128]` for down,
`dtype=40` (NVFP4), 30 layers, 128 experts and top-k 8. The same request
`Escreva hello world 10 vezes.` ("write hello world 10 times") was validated on
the real route: HTTP 200, 55 tokens generated, `input_tokens=22`,
`complete=true`, 6.42 tok/s, and prefill/decode with `executed_engine=moe`,
with no divergence or mixing. The pure tests of the executor, the separate
names parser and the telemetry still cover the local boundaries; the physical
validation confirms the NVFP4 wiring on the real model.


### CUDA baseline observed on the CUDA test server

On 2026-08-25, with `pom=b8e24aa`, `llama.cpp=03902a5`, the same request
`Escreva hello world 10 vezes` ("write hello world 10 times") was run on Gemma4
NVFP4, first with MoE and then with the dense path. The MoE request had
`input_tokens=21`, `output_tokens=30`, `prefill_ms=35655`,
`decode.processing_ms=49706` over 29 calls and `moe_executor=cuda`, dtype
`[40]`, with no divergence. That amounts to `1697.857 ms/token` of prefill and
`1714.000 ms/token` per decode call. Dense had `input_tokens=21`,
`output_tokens=55`, `prefill_ms=337` and `decode.processing_ms=2414` over 54
calls, or `16.048 ms/token` and `44.704 ms/token`, respectively.

In the MoE trace, the backend time and the call measured on the node match:

| phase | backend total (`processing_ms`) | backend components | `node_decode_ms` |
|---|---:|---:|---:|
| prefill | 35649.000 ms | 35649.057 ms | 35649.066 ms |
| decode | 49706.000 ms | 49697.058 ms | 49697.246 ms |

That confirms there is no hidden second run, but does not say yet which
internal share of the CUDA executor dominates. The diagnostic instrumentation
separates allocation, H2D, kernel, launch, synchronization, D2H and free.

### Internal CUDA profile observed on the CUDA test server

The profile was run on an isolated instance, without touching the service on
ports 8080/8092, with `pom=8187aad`, `llama.cpp=03902a5`,
`POM_MOE_CUDA_PROFILE=1` and the real GGUF `Gemma4-26b-NVFP4.gguf`. The OpenAI
request had `input_tokens=20` and produced `hello`; the trace confirmed
`executed_engine=moe`, `moe_executor=cuda`, `profiled=true` and
`engine_mismatch=false`.

The CUDA counters below are the accumulated times published by the executor.
On the decode row, the end-of-prefill counters were subtracted from the
end-of-decode counters: the runtime keeps the diagnostic per model, so the row
represents the 240 experts selected for that single decode token, not the 5040
accumulated calls. Percentages are only of the sum of the instrumented
categories, not of the node's total time.

| phase | calls | backend/enqueue | H2D | kernel | alloc | launch | sync | D2H | free |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| prefill, 20 tokens | 4800 | 33784.602 ms | 10189.819 ms | 19882.414 ms | 324.379 ms | 59.956 ms | 302.153 ms | 49.750 ms | 269.494 ms |
| decode, delta of 1 token | 240 | 1738.734 ms | 621.394 ms | 967.158 ms | 16.860 ms | 3.174 ms | 14.713 ms | 2.589 ms | 13.685 ms |

The sum of the instrumented categories was `31077.965 ms` in prefill and
`1639.573 ms` in decode. The rest up to backend/enqueue was, respectively,
`2706.637 ms` and `99.161 ms`, so it is not attributed to the kernel by
inference either. Within what was measured, the kernel accounts for `63.98%` of
prefill and `58.99%` of decode; H2D accounts for `32.79%` and `37.90%`. So the
transfer is large, but it is not the largest category measured in that run.

### Structural diagnosis of the CUDA path

The LRU was not orphaned. `MoeRuntime::open` creates an
`ExpertCache::measured_system` instance, shares the same `Arc<ExpertCache>`
across every block and `Nvfp4Executor::run_with_loader` does a `lookup` before
`admit`, including on the CUDA path. What the LRU keeps is the raw NVFP4 payload
in RAM. It keeps neither CUDA buffers nor a device lease.

For the real Gemma4, the geometry observed in the GGUF and in the hook is
`hidden=2816`, `intermediate=704`, 30 layers, 128 experts and top-k 8. The
current path does, per decode token:

| operation | count |
|---|---:|
| expert calls | 30 x 8 = 240 |
| `gate_up` H2D copies | 240 |
| `down` H2D copies | 240 |
| H2D bytes of weights only | 802897920 (765.7 MiB) |
| H2D bytes including hidden | 805601280 (768.3 MiB) |
| kernel launches | 480 |
| `cudaDeviceSynchronize` | 480 |
| `cudaMalloc` and `cudaFree` | 1200 of each |
| output D2H copies | 240 |

Each selected expert crosses PCIe once per weight tensor on every call, even
when the `ExpertCache` records a hit. The profile measured `805601280` H2D bytes
per decode token, of which `802897920` are weights: 240 `gate_up` copies and 240
`down` copies, or `3,345,408 bytes` of weights per expert use. In the 20-token
prefill, there were `4800` expert calls and `9600` weight H2D copies, totaling
`16112025600` H2D bytes. Also, `Nvfp4CudaExecutor::run` repeats the payload
header validation on the CPU on every call.

The LRU was effectively active: at the end of prefill there were `1279` misses,
`3521` hits and `1279` admissions; in decode there were `22` more misses and
`218` hits, reaching `1301` entries and `4352417440` bytes resident in RAM. But
the decode delta still transferred the weights of the 240 uses, including the
218 hits. So the objective answer to the PCIe question is: a weight tensor
crosses once per selected use, not once per LRU miss. A given expert key crosses
as many times as it is selected in routing; on this token, the aggregate was 480
weight transfers.

The path that produced that profile had a concrete divergence in
`cuda_kernels.cu`: the buffer copied to the device (`d_gate_up`) was not the
pointer passed to `gate_up_kernel`; the kernel got the host pointer `gate_up`.
The differential CUDA test reproduced the consequence before the fix with
impossible sizes (`262144 != 262128`), consistent with corruption caused by the
host/device boundary. The fix passes `d_gate_up` to the kernel, without changing
the payload format or the computation. The cost profile had to be repeated after
that fix before attributing the prefill's `19882.414 ms` of kernel or the
decode's `967.158 ms` to the NVFP4 computation itself.

The validation commit `22f6c23` also aligned the CUDA computation with ggml's
numeric convention. Each matmul input is quantized in Q8_0 blocks of 32 values,
with the scale rounded to FP16; the NVFP4 dot accumulates the integer products
per sub-block and applies both scales after the sum. The GeGLU activation uses
the same cut and the same FP16 rounding as the ggml path before being quantized
for `down`. That is a convention of the ggml pipeline, not a Gemma4 or NVFP4
rule: dimensions and payload still come from the tensor's metadata, and a CUDA
format that does not have that contract must refuse the dtype.

The real validation on the Gemma4 GGUF (`hidden=2816`, `intermediate=704`)
passed against the ggml oracle on every layer and stage. The GPU bridge also
passed the differential CUDA tests with a uniform input and with an input in
the GELU region, both comparing the same payload and hidden between Rust CPU and
CUDA. The `cargo build --release --features vulkan,cuda` build passed on the
CUDA test server in 9m01s; the filtered CUDA test passed with
`2 passed, 0 failed` and the real proof passed with `1 passed, 0 failed`. The
two remaining warnings are `Nvfp4Trace` not consumed outside the diagnostic and
the `fp4_value` helper, both already known. The real GPU proof with the GGUF
must be repeated before any grouped optimization, because the unit CUDA test
does not replace the model's real geometry.

### CUDA kernel diagnosis before grouping

The current kernel uses per-output parallelism: `gate_up_kernel` launches one
thread per gate/up row and `down_kernel` one thread per output row, in blocks of
128 threads. Each thread walks every block and sub-block of its row. There is
parallelism across rows, but no cooperation between threads for a matrix, no
shared memory tile and no expert batch. The read was not designed as a coalesced
tiled load: adjacent threads walk distinct rows and repeat the same quantized
bytes for each output. The real degree of coalescing still needs Nsight
Compute; the current code does not allow claiming efficient coalescing.

E2M1 and UE4M3 unpacking happens inside `dot_nvfp4` on every expert use. The
activation's Q8_0 quantization is also redone per call, and GeGLU is recomputed
before `down`. So an LRU hit leaves neither weights nor activations unpacked on
the GPU. The earlier profile, taken before the numeric alignment, measured
`480` launches per decode token; those numbers must not be used as the gain of a
correct implementation until the profile is repeated. The next approved step is
grouping the work while keeping the numeric proof first; F5-E2 and VRAM
residency come after.

### First validation of CUDA grouping

Commit `379ce6a` grouped a route's active experts into one CUDA call: the hidden
state is quantized once, the payloads are copied into a contiguous block and
gate/up, GeGLU and down use `(expert, row)` indexes. The grouped call does five
kernel launches and two synchronizations for the whole batch, instead of
repeating the sequence per expert. The weighted sum stays in Rust, in the
routing's original order. Grouping is generic by the batch's count and
geometry; there is no branch per architecture or dtype.

The first CUDA test revealed a defect specific to the new indexing: the Q8_0
quantization compared the batch's global index with the length of a single
vector, leaving the vectors from the second one on without values. `379ce6a`
separates the offset within the vector from the global index. The two-expert
case failed before and passed after the fix, without changing the quantization
formula.

In the final review `fb8da96`, validated on the CUDA test server, the results
were:

- `cargo build --release --features vulkan,cuda`: passed in `3m22s`, with the
  two known warnings of `Nvfp4Trace` and `fp4_value` not consumed;
- the three synthetic CUDA tests, including the batch, `3 passed, 0 failed`;
- a CUDA batch with two experts extracted from the real Gemma4 GGUF, dimensions
  derived from the file: `1 passed, 0 failed`;
- a real layer-by-layer comparison against the ggml oracle of the same Gemma4:
  `1 passed, 0 failed`.

The real batch test compares the same bytes and hidden state between CUDA and
the CPU decoder, while the oracle proof compares the CPU decoder with
llama.cpp. That separation keeps the CUDA test from validating the oracle
itself. The grouped E2E measurement is recorded below. The cache stays in RAM,
with no expert residency in VRAM; F5-E2 stays after grouping.

Conclusion of that measurement: the LRU is active on the hook path, but it is
only a RAM cache. The CUDA path repeats the weight transfer for every selected
expert. The `gate_up` pointer fix is a numeric correctness step, not an
optimization; after it, telemetry must be collected again before comparing the
kernel's cost. The numbers above stay the pre-fix baseline; no optimization was
done in that step.

### E2E measurement of CUDA grouping

On 2026-08-25, the measurement was repeated on the CUDA test server with
`pom=340efc8`, `llama.cpp=03902a5`, an RTX 5060 Ti and the real GGUF
`Gemma4-26b-NVFP4.gguf`. To get the internal breakdown without restarting the
serving node, an isolated instance of the same binary was used, on the same GPU
and on separate ports, with `POM_MOE_CUDA_PROFILE=1`. The main service stayed
up with Gemma4 in MoE mode. The request was `Escreva hello world 10 vezes.`,
with `max_tokens=64`; it produced 30 tokens in 10 coherent lines and had
`input_tokens=22`.

With grouped MoE, the trace showed `executed_engine=moe`, `moe_executor=cuda`,
`engine_mismatch=false`, `mixed_execution=false`, `prefill_ms=6962`,
`decode.processing_ms=5396` over 29 calls and a total duration of `12525 ms`.
So prefill was `316.455 ms/token` and decode was `186.069 ms/token`.

| phase | backend total | node decode call | H2D | CUDA kernel | expert calls |
|---|---:|---:|---:|---:|---:|
| prefill, 22 tokens | `6957 ms` | `6956.219 ms` | `1758.498 ms` (`79.932 ms/token`) | `157.755 ms` (`7.171 ms/token`) | `5280` |
| decode, 29 tokens | `5396 ms` | `5390.302 ms` | `1892.275 ms` (`65.251 ms/token`) | `234.272 ms` (`8.078 ms/token`) | `6960` |

On the decode row, H2D and kernel are the delta of the counters accumulated at
the end of prefill. The `6960` experts are `29 x 240`, and H2D was `23294062080`
bytes in total for the delta, or `803243520 bytes/token` (`766.033 MiB/token`).
Grouping reduced the launches to five per batch, but still copies the selected
payloads to the GPU.

The dense pair, on the same instance and with the same request, produced the
same text with `executed_engine=llama.cpp`, `engine_mismatch=false`,
`prefill_ms=398`, `decode.processing_ms=1402` over 29 calls and a total duration
of `1970 ms`: `18.091 ms/token` of prefill and `48.345 ms/token` of decode.
Expert H2D/kernel does not apply to the dense path, which does not run the MoE
executor.

Compared to the pre-grouping baseline of `1689.3 ms/token` in prefill,
`1738.7 ms/token` in decode, `621.4 ms/token` of H2D and `967.2 ms/token` of
kernel in decode, the measured grouping was an isolated improvement: about
`5.34x` in prefill, `9.34x` in decode, `9.52x` in H2D and `119.73x` in the
kernel. The main node ended serving on `8080/8092`, with Gemma4 NVFP4 active in
MoE mode; the isolated instance was shut down after the measurement.

### Closed time breakdown after grouping

To separate CUDA sub-measurements that may overlap, the same request was
repeated on an isolated instance of the validation branch `0d6d70f`, built on
the CUDA test server with `--features vulkan,cuda`. The main service was not
restarted. The measurement used `POM_MOE_CUDA_PROFILE=1` and added two
non-overlapping walls: `runtime_forward_ms` for the whole MoE forward and
`cuda_call_wall_ms` for the whole CUDA FFI call. The text was again ten lines of
`hello world`, with 30 output tokens and 22 input tokens.

| phase | backend total | MoE forward | llama.cpp rest | total per token |
|---|---:|---:|---:|---:|
| prefill, 22 tokens | `8039.369 ms` | `7802.851 ms` (`97.058%`) | `236.518 ms` (`2.942%`) | `365.426 ms/token` |
| decode, 29 calls | `5979.008 ms` | `5481.247 ms` (`91.675%`) | `497.761 ms` (`8.325%`) | `206.173 ms/token` |

The rest is the measured difference between the backend wall and the MoE
forward, not a cost estimate. Inside the MoE forward, the partition also closes
exactly:

| non-overlapping component | prefill | decode, delta of 29 calls |
|---|---:|---:|
| cache lookup | `1.263 ms` | `3.784 ms` |
| expert read from the GGUF | `501.936 ms` | `92.603 ms` |
| admission into the RAM cache | `696.284 ms` | `57.419 ms` |
| batch preparation on the host | `3622.166 ms` | `2166.206 ms` |
| CUDA call wall | `2836.915 ms` | `3086.384 ms` |
| result copy on the host | `6.197 ms` | `7.706 ms` |
| weighted aggregation | `2.229 ms` | `2.258 ms` |
| remaining runtime orchestration | `135.860 ms` | `64.887 ms` |
| **MoE forward total** | **`7802.851 ms`** | **`5481.247 ms`** |

So the largest single prefill component is batch preparation on the host
(`45.055%` of the backend), followed by the CUDA wall (`35.288%`). In decode,
the CUDA wall is the largest component (`51.620%`), followed by batch
preparation (`36.230%`). Expert reading and admission stop dominating in
decode, because the LRU is active: prefill had `1448` misses and `3832` hits;
the decode delta had `177` misses and `6783` hits. Even so, each decode token
measured `803243520` bytes (`766.033 MiB`) of H2D, because the cache is in RAM
and not resident in VRAM.

The internal sub-measurements of the CUDA wall, which must not be added to the
table above, were in prefill: H2D `2246.346 ms` for `17671357440` bytes, kernel
`378.780 ms`, allocation `95.688 ms`, launches `253.452 ms`, sync `6.492 ms`,
D2H `15.196 ms` and free `66.257 ms`. In the decode delta: H2D `2618.950 ms`,
kernel `224.304 ms`, allocation `100.680 ms`, launches `27.006 ms`, sync
`16.399 ms`, D2H `21.769 ms` and free `76.018 ms`. The CUDA wall is the measure
for the 100% sum; those internal phases explain its content and their clocks
may overlap.

The legacy field `node_moe_forward_ms` stayed zero in the decode trace, even
though `runtime_forward_ms` grew by `5481.247 ms`; the breakdown uses the
runtime's wall counter and records that telemetry divergence instead of hiding
it. The observed executor was `cuda`, dtype `[40]`, `executed_engine=moe`,
`engine_mismatch=false` and `mixed_execution=false`. The largest next measured
target is reducing the transfer and the batch preparation, but that measurement
neither starts F5-E2 nor claims its gain.


### Prefill diagnosis after scalar coverage - 2026-08-25

The operator's most recent benchmark measured three valid rounds of Gemma4 on
the gateway: `38.4 tok/s` of prefill and `23.0 tok/s` of decode. That amounts to
`26.042 ms/token` and `43.478 ms/token`, respectively. The historical
`llama.cpp` reference is `49.0/20.4 tok/s` and FreeToken's is
`359.1/47.7 tok/s`. So the current prefill is `1.276x` slower than `llama.cpp`
and `9.352x` below FreeToken; decode is `1.127x` above `llama.cpp` and still
`2.074x` below FreeToken. The new benchmark brought neither the prompt size,
the effective `n_ubatch` nor the internal trace, so those numbers do not allow
attributing the difference to a single hypothesis.

The hypotheses were checked against the real path:

1. **Token-by-token prefill.** Refuted at the server level. `prefill::run`
   sends a whole chunk and `prefill_blocking` calls `decode_tokens` once with
   every token of that chunk. The chunk's default is 512 and it is capped by
   `n_batch`; `llama_batch` gets the whole batch. That does not mean a single
   physical computation: `llama.cpp` processes the batch in microbatches and
   the external hook is called inside each one. Grouped CUDA gets the
   microbatch's `n_tokens` and processes that batch's routes in four kernels,
   not one call per token.

2. **`n_ubatch=128`.** Confirmed as a potential limiter, not as a sufficient
   cause. A temporary candidate reduced `n_batch=512` to 128 when
   `fit_memory=true` and `moe_external_executor` was on; for a prompt longer than
   128, the graph was repeated in `ceil(tokens/128)` microbatches and the MoE
   callback repeated route preparation, descriptor transfers and
   synchronization per microbatch. The instrumented profile we have as a full
   breakdown had 22 tokens, so it fit in a single microbatch, and even so batch
   preparation dominated. The approved exception stays restricted to the main
   MoE context: the dense path and MTP keep the operational limit of 512. The
   share attributable to the microbatch stays a separate decision for the MoE
   context.

3. **Per-layer synchronization in prefill.** There is synchronization in the
   external executor, but it is not the observed bottleneck. Each CUDA FFI call
   records `sync_count=1` and runs `cudaDeviceSynchronize` after the four
   kernels; since the callback is inside the graph, that happens per layer and
   microbatch. In the historical profile, `sync_ms=6.492 ms` in prefill, against
   `cuda_call_wall_ms=2836.915 ms`; in the decode delta it was `16.399 ms`
   against `3086.384 ms`. That does not support the hypothesis that the sync
   wait was the main cost of that load, but it is not evidence about the
   current state.

The breakdown below belongs exclusively to the historical 22-token profile,
earlier than the state that produced `38.4 tok/s`. It is recorded for
reproduction, but deliberately excluded from any conclusion about the current
prefill:

| component | time | share of the backend total |
|---|---:|---:|
| `llama.cpp` rest outside the MoE forward | `236.518 ms` | `2.942%` |
| cache lookup | `1.263 ms` | `0.016%` |
| expert read from the GGUF | `501.936 ms` | `6.244%` |
| admission into the RAM cache | `696.284 ms` | `8.661%` |
| batch preparation on the host | `3622.166 ms` | `45.055%` |
| CUDA call wall | `2836.915 ms` | `35.288%` |
| result copy on the host | `6.197 ms` | `0.077%` |
| weighted aggregation | `2.229 ms` | `0.028%` |
| remaining runtime orchestration | `135.860 ms` | `1.690%` |
| **backend total** | **`8039.369 ms`** | **`100%`** |

That table does not prove where the time is today. The new profile was still
pending because the GPU was being used by the concurrency benchmark. The right
collection must repeat exactly the current three rounds with
`POM_MOE_CUDA_PROFILE=1` and record the prompt size, `n_batch`, `n_ubatch`, the
number of callbacks and the equivalent breakdown. Only after that collection,
with the card free, is it possible to decide between growing the microbatch,
reusing the host preparation or changing the synchronization.

### Formal gap analysis: FreeToken versus the current MoE path

This analysis was done on 2026-08-25, without changing code and without
accessing the CUDA test server. The source on the FreeToken side is the
external study report (kept outside this repository), which read commit
`bd372b630a028e3faa51f4ab0ef6a98c2f2de501`. The source on the local side is the
commits and profiles recorded in this document. The order below is by observed
impact, not by ease of implementation.

| priority | gap | FreeToken has | POM has | observed cost and expected gain |
|---|---|---|---|---|
| 1 | VRAM-resident expert LRU | `moe/offload_cache.py` keeps a global expert cache on the device, with admission by the free memory budget. The hot payload does not need to cross PCIe on every selection. | `moe/cache.rs` keeps the canonical payload in host RAM. The CUDA executor uses temporary staging and transfers the weight even on an LRU hit. | Direct evidence: `803243520` H2D bytes per decode token (`766.033 MiB`), with `240` expert uses per token. In the closed profile, H2D was `2618.950 ms` over 29 tokens (`90.309 ms/token`) inside `cuda_call_wall_ms=3086.384 ms` (`106.427 ms/token`). If residency removes those copies, that is the direct ceiling of the CUDA wall's reduction on that load, not a promise to reduce the whole backend: host preparation remains. |
| 2 | CUDA kernels for each quantization family | The report lists its own banks in `_BANK_SCHEMAS`: `bf16`, `fp8_block`, `q4_0`, `nvfp4`, `mxfp4` and `ds_fp4`, with CUDA/Triton kernels outside llama.cpp. | The CPU executor covers several GGUF families; the CUDA bridge covers NVFP4, scalars, Q2_K/Q3_K/Q4_K/Q6_K and IQ3_S/IQ2_S/IQ4_XS after differential proofs, while the other dtypes publish `CPUReference` with a refusal by name. | The historical baseline of Qwen UD-IQ3_S on `CPUReference` is not a forecast of the current gain: the IQ bridge was only accepted after comparing the three stages on the real GPU. |
| 3 | Batch preparation off the hot path | The report shows a GPU-centric scheduler that hands bank views to the kernels and overlaps the next layer's prefetch. It publishes no separate metric called `host_batch_prepare`; so there is no evidence to claim FreeToken has zero cost at that step. | The Rust callback builds batches and descriptors on the host on every forward. Telemetry measures that cost as `host_batch_prepare_ms`, before the CUDA call. | In the historical profile, it was the largest prefill component: `3622.166 ms`. That was not confirmed in the current `38.4 tok/s` state; the real gain of reusing descriptors and buffers depends on the new profile. |
| 4 | Compute-versus-transfer policy based on measured bandwidth and a real double buffer | `moe/benchbw.py` measures RAM/PCIe; `moe/bench_profile.py` writes a per-GPU profile and computes the hybrid fraction `pcie/(pcie+cpu)`. `layers/moe.py` starts the next layer's prefetch and the budget reserves two sets of buffers. | `BandwidthProfile` persists the bandwidths, `coexecution.rs` splits unique misses and runs CPU/GPU lanes in parallel on NVFP4; `DoubleBuffer` validates the two-slot reservation. | The wiring still needs this revision's CUDA proof. The H2D and kernel numbers recorded in the document are historical and are not a promise of gain. |
| 5 | CUDA Graphs for the quantized MoE | FreeToken captures and recaptures CUDA graphs for the batch shapes and the cache, avoiding repeated build and launch cost. | There is no CUDA graph capture/replay for the Rust MoE callback. llama.cpp can use its own graphs for the rest of the transformer, but that does not cover the external executor. | After grouping, CUDA launches cost `27.006 ms` over 29 tokens (`0.931 ms/token`) and sync `16.399 ms` (`0.565 ms/token`). So graphs can reduce launch and allocation overhead, but the maximum explicitly measured in that category is small next to the `74.697 ms/token` of host preparation and `90.309 ms/token` of H2D. |

#### Item 1: VRAM cache

The operator's point is confirmed and it is structural. A hit in our LRU means
we avoid rereading and unpacking the payload from the GGUF, not that the device
already has the weight. The `lookup -> admit -> staging H2D` path keeps the hit
in RAM, but repeats the PCIe crossing. The earlier profile also measured
`805601280` bytes per token before grouping; grouping reduced launches, not the
volume of transferred weights. F5-E2, which removes the experts' residency from
the llama.cpp loader, avoids double residency in the model, but does not turn
the Rust LRU into a VRAM cache. They are distinct problems and both must show up
in the measurement.

#### Item 2: measured bandwidth and double buffer

This point is also confirmed, with a scope correction. FreeToken does not simply
always pick the GPU: it measures CPU and PCIe bandwidth per format and GPU,
ignores another card's profile and uses the computed fraction to decide the
hybrid work. Our executor has the diagnostic fields and the documented intent,
but neither a bandwidth profile loaded by the runtime nor two alternating CUDA
buffers in production. The report's double buffer is mainly prefetch between
layers during prefill; it is not equivalent to just grouping experts into one
call.

#### Item 3: CUDA Graphs

Also confirmed, but the current impact is smaller than the H2D copy and the host
preparation. Grouping already reduced the number of launches. So a graph should
be measured after the buffer layout is stable and after VRAM residency;
capturing a dynamic route before that would risk frequent recaptures and hide
the cost that really dominates.

#### Item 4: kernels per format

The report confirms six of FreeToken's own banks, not a proof of every existing
GGUF dtype. The right comparison is: we have a broader CPU decoding layer and a
refusal by name per tensor; the CUDA bridge only covers the formats that passed
the proof, currently NVFP4, scalars, Q4_K, Q6_K, IQ3_S, IQ2_S and IQ4_XS. The
attack order must be decided by real models and by numeric proof, not by the
report's list as if it automatically covered IQ-quants.

#### Item 5: building the batch on the host

FreeToken offers evidence of a GPU-centric organization with bank views,
prefetch and streams, but the report does not measure a box equivalent to
`host_batch_prepare_ms`. So the safe statement is that our cost was measured and
theirs was not broken down that way, not that FreeToken "builds everything on
the GPU". The local gap is objective: in our profile the host prepares the batch
per token before the CUDA call and takes longer than the kernel. Any gain
attributed to moving that build must keep the dynamic route, the experts' order
and the numeric proof per dtype.

#### History of the 9.3 tok/s number and the duplicated execution

There is no line in the repository, the persisted status or the documentation
that explicitly ties the `9.3 tok/s` number to a deployment SHA. The duplicated
state that can be proven in the code and matches the period before the fork's
hook is `5304228c0cf45dc729524bee21c217dd346be0af`, committed on 2026-08-24 at
18:15:18, with the caveat that the deploy log that would tie that SHA to the 9.3
number was not kept.

In that commit, `infer_server.rs` first did `decode_hidden` or `decode_tokens`
(`lines 2870-2887`), that is, it let llama.cpp run the whole graph. On the
terminal hop, after that return, it did `take_residual` and called
`apply_moe_hidden`, but discarded the result with
`let _ = s.apply_moe_hidden(...)` (`lines 2889-2892`). Right after, the logits
were read from `s.engine.logits_ith(...)` and sampled (`lines 2936-2941`),
without using the external executor's output. For a one-hop terminal request,
the answer came from llama.cpp; the external pass was extra, discarded work. The
commit did not have yet the opt-in `moe_external_executor` callback that today
lets the native block be prevented.

The operator's reading needs a precision correction: in that commit telemetry
did not only report the chosen path. It already serialized `selected_path` and
`executed_engine`, but `record_terminal_engine` marked `executed_engine=moe`
when the external pass ended (`lines 1188-1196`), without recording that
llama.cpp had also run the graph. So the `moe` label was evidence that the
external pass happened, not that it was the only executor nor that its output
fed the sampling. There was no detection of duplication or mixing in that case.

So the strong claim "9.3 was our engine's throughput" is wrong for that
duplicated state. The more precise claim is: if the 9.3 test was done on the
`5304228` state, the terminal answer was not produced by our MoE executor and
that number is not a speed baseline of the own engine. The repository does not
allow claiming, without the deploy log, that `5304228` was exactly the SHA of
the 9.3 test; stating that link as a fact would be inventing evidence.

The later honest profile is comparable per phase, not by a conversation's
aggregated tok/s: the grouped measurement recorded `316.455 ms/token` of prefill
and `186.069 ms/token` of decode, against `1689.3` and `1738.7 ms/token` before
grouping. The instrumented repeat recorded `365.426` and `206.173 ms/token`. So
in those per-token numbers the later honest state is better, not worse. The
aggregated `tok/s` can drop when the prompt is long because it includes the
prefill; it does not invalidate that per-phase comparison.

#### Priority decision

The next measurable gain should start with the VRAM-resident cache, because it
is the only item that already has a direct measured cost of `766 MiB` of weights
per token. In throughput impact across models, the per-dtype CUDA bridge comes
right after: Qwen UD measured seconds per token on `CPUReference` before the IQ
bridge. Within Gemma NVFP4, host preparation is the next target, followed by the
real bandwidth policy and double buffer. Each dtype bridge requires a numeric
proof per format. CUDA Graphs come after stabilizing residency, layout and
policy, since the already grouped launches are a small share of the explicitly
measured time.

### Formal objective: inverting the experts' residency

The current state is a historical consequence, not a product decision. The
cache was created in `7a09d316befc51a2cbaf97775ece9caaab933f28` (2026-08-24,
`feat: adicionar cache global de experts q4_0`) when the executor was CPU-only.
The `ExpertCache` kept `Arc<[u8]>`, and the default probe was
`SystemFreeMemory`, which read `MemAvailable` on Linux, `vm_stat` on macOS and
available physical memory on Windows. The comment itself said a VRAM probe would
be a future integration. In that context, keeping the payload in RAM made sense:
it was the reference decoder's cache, worked without CUDA and did not assume a
discrete GPU.

The CUDA bridge came later, in `c782e557a8738d44afdc8505de7cba75261aa793`
(2026-08-24 at 20:14:30). It got `lease.bytes()` from the cache and allocated
the CUDA buffers inside each run. That commit made no residency change in
`cache.rs`, no device probe and no admission policy. The later profile confirmed
the consequence: LRU hits avoided reading and unpacking on the host, but still
transferred the weight to the GPU. So the operator's explanation is right: there
was integration inertia between the CPU Phase 4 and the CUDA Phase 5, not a
technical limitation forcing the cache to stay in RAM.

The other side of the inversion also has an objective historical cause. In the
CPU phase's loader, `llama_model_default_params` got `n_gpu_layers=-1` during
the memory fit and there was no `moe_external_executor`; llama.cpp's normal load
could put the expert tensors on `CUDA0`. The option to not allocate experts was
added later as F5-E2. So llama.cpp's residency on the card and our cache's
residency in RAM were two independent decisions never closed as a single device
budget.

This objective fixes the inversion as a contract of the MoE path, without
changing the dense path:

1. The own executor's global expert cache becomes resident in the VRAM of the
   device that will run the batch. A hit returns device pointers or handles and
   does no H2D. A miss admits the expert once, waits for the CUDA event to
   complete before publishing the lease and only then reuses it across tokens
   and sessions.
2. F5-E2 stops being only memory hygiene and becomes a prerequisite. When the
   external executor is selected, the llama.cpp loader neither allocates nor
   manages the expert weights in any device buffer. It only keeps the metadata
   and descriptors needed by the hook; the GGUF source and the own cache stay
   responsible for the bytes. Without the opt-in, llama.cpp stays byte for byte
   on the current dense behavior.
3. The boundary is generic. The residency key includes model, device, layer,
   expert, tensor/role, dtype and effective geometry. Architecture, gate/up
   convention and format are data of the hook's descriptor. A dtype, tensor,
   geometry or architecture without a compatible decoder is refused by name at
   the admission limit, naming the item; it never reactivates the native weights
   nor silently assumes CPU or H2D.
4. Admission uses the real VRAM budget, not a constant. For CUDA, each miss
   queries the device's free memory and subtracts weights still in llama.cpp,
   fixed buffers, KV, activations, staging and an operational reserve. After
   F5-E2, the llama.cpp expert weights term must be zero. The limit is
   recomputed when external memory changes and eviction can only remove entries
   without a lease and without a pending CUDA operation.

#### Storage design and flow

The current logical cache will be separated from the physical representation.
The key, LRU, leases and statistics stay common; the entry now holds a storage
backend per device, an allocation handle, resident bytes, dtype, geometry, a
readiness event and last use. The GGUF's mmapped payload can stay as a cold
source or an explicit CPU fallback, but is not confused with the hot CUDA cache.

```text
GGUF mmap / cold source
        |
        | miss + measured VRAM budget
        v
device cache: allocate -> H2D once -> ready event -> lease
        |
        | hit: lease of the same device pointer, no H2D
        v
kernel grouped by dtype -> aggregation -> lease release
```

The descriptor must not publish a handle before the expert's H2D and sidecars
are ready on the right stream. Eviction must wait for the use event, not just
observe `active == 0`; that avoids reusing an allocation while the kernel is
still reading it. When an expert does not fit even after idle eviction, the
error must name `device`, `dtype`, tensor, layer, expert, requested bytes and
available budget.


#### Phases and done criteria

- **R0, contract and budget.** Define the device allocator/probe trait,
  separate cold bytes from resident handles, and test the arithmetic with fake
  probes. Done when the admission decision is reproducible without CUDA and no
  VRAM constant exists in the executor.
- **R1, the loader's F5-E2.** Turn on the external option in the fork, keep the
  metadata in a `no_alloc` context and prevent llama.cpp's expert buffers. Done
  when the MoE path shows no expert bytes on `CUDA0` or `CUDA_Host`, while dense
  keeps loading and running without the option.
- **R2, resident CUDA cache.** Implement residency for the generic descriptor
  and validate NVFP4 first since it is the only CUDA dtype already proven. A
  warm-up sequence followed by hits must show weight H2D equal to zero per use,
  and a miss must show exactly one admission per resident tensor. Done also
  requires the already approved layer-by-layer numeric comparison.
- **R3, formats and architectures.** Wire each additional CUDA bridge per tensor
  and dtype, starting with the remaining real models. Each block needs a proof of
  Rust CPU versus llama.cpp, CPU versus CUDA, a refusal by name for an unserved
  combination and a coherent E2E; there is no promotion based on a homogeneous
  fixture.
- **R4, inversion proof.** On the CUDA test server, record before/after with the
  same build: `CUDA0`, `CUDA_Host`, CPU, RSS/PSS, device cache bytes, hits,
  misses, weight H2D and activation H2D. The criterion is
  `llama.cpp expert bytes on device = 0`, a resident MoE cache greater than
  zero, zero weight H2D on hits, and no second residency. Measure prefill and
  decode per token in the same test, without attributing a gain before closing
  that accounting.

### Goal resumed: CUDA executor and CPU/GPU co-execution

This is Phase 5's new formal goal, decided by the operator after the
measurement on the CUDA test server with Gemma4 NVFP4 on the same GPU. The
current CPU path measures about `54 ms/token` in prefill and `80 ms/token` in
decode; the FreeToken reference measures `3.1 ms/token` and `18.8 ms/token`,
respectively. The observed gap is about `17.4x` in prefill and `4.3x` in decode.
More than 90% of our path's time is outside `llama_decode`, in the CPU reference
decoder. The fact that FreeToken keeps experts in CPU/RAM rules out offload as a
sufficient explanation: the bottleneck is the expert's computation.

There is a second reference, measured on the real Chat on the CUDA test server
without the batch-8 constraint: `llama.cpp` does `292` input tokens in `983 ms`,
about `3.4 ms/token` of prefill, and settles at about `30 ms/token` at the decode
p50. So the `3.1 ms/token` prefill is at the same time the FreeToken target and
practical parity with `llama.cpp`; in decode, `30 ms/token` is an intermediate
parity milestone with the dense engine before the FreeToken target of
`18.8 ms/token`.

The objective is to reimplement FreeToken's computation approach on the Rust
platform, with own CUDA kernels and calls made by the Rust host. Python, Triton
and FreeToken as an external engine are out of scope. FreeToken is only a source
of design and algorithms; no code is copied. The dense path stays on `llama.cpp`
with no change of selection or behavior.

The current instrumented state still reuses the global `ExpertCache` with a
canonical entry in CPU/RAM and temporary staging CUDA buffers. That is exactly
the gap covered by the formal inversion objective above, not the final design.
The router and the admission policy stay in Rust. Each forward must publish
`gpu_expert_calls`, `cpu_expert_calls`, H2D/D2H bytes and bandwidth,
`buffer_index`, overlap time, resident bytes per device and the reason for any
fallback.

### Formal objective: replacing the experts' execution and loading

This objective is a formal extension of Phase 5. The MoE engine must replace
`llama.cpp`'s expert block without keeping a second residency of the same
weights. The order is deliberate: first prove and measure the suppression of the
duplicated execution; only then suppress the loading of the weights. So the
variation of each measurement is attributed to an isolated change.

#### F5-E1: generic replacement of execution

The extension point lives in the fork's common `build_moe_ffn`. When the opt-in
callback is installed, it gets the already resolved routing ids and weights, the
total and active expert counts, architecture, activation, fused or separate
gate/up convention, names, geometries, dtypes and GGML scales. The Rust executor
picks the decoder by the dtype received as data. There is no replacement branch
per architecture or `dtype == NVFP4`.

The callback returns the block's output directly and keeps the native expert
matmuls out of the graph. Attention, normalizations, KV cache, embeddings,
tokenization and sampling stay in `llama.cpp`. A missing callback keeps the
native graph, including for dense models. An architecture, geometry, activation
or dtype the executor cannot serve must produce a refusal by name at the decode
boundary, never run the native block as a silent fallback.

The inventory follows llama.cpp's same priority when a GGUF carries a fused
`ffn_gate_up_exps.weight` together with `ffn_gate_exps.weight` and
`ffn_up_exps.weight`: the fused tensor is the active representation and the
duplicated splits are ignored. That avoids turning copies of the same expert
into competing roles in the descriptor. On the K and heterogeneous paths, each
active tensor also looks for the canonical sidecars by removing `.weight` from
the base name and appending `.scale` or `.input_scale`. The per-expert values are
validated as finite scalars, loaded into the payload/descriptor and applied in
the gate, up and down products. The shared tensor `ffn_*_shexp` stays out of
that inventory.

The tests `the_shared_expert_stays_out_of_the_route_inventory` and
`route_inventory_separates_exps_from_shexp_and_the_sidecars` confirm that only the
four `*_exps.weight` tensors are classified as route: `*_shexp` and
`.scale`/`.input_scale` stay out. The test
`fused_sidecars_use_the_right_component_and_split_fallback` confirms the
association of component 0/1 in the fused tensor's sidecars and the fallback to
the separate gate/up sidecars. So the hypotheses of a counted shared expert and
a sidecar tied to the wrong component do not explain the NaN observed on Ornith.

The heterogeneous CUDA descriptor is a C structure compatible with
`#[repr(C)]`: `data` at offset 0, `bytes` at 8, `dtype` at 16, `scale` at 20,
`input_scale` at 24, `secondary_scale` at 28 and `secondary_input_scale` at 32;
the final size is 40 bytes. Offset and size assertions exist in Rust and in
`cuda_kernels.cu`. That part is not covered by the local build on macOS: any
kernel change needs an `nvcc` build on the CUDA test server.

The common point normalizes the route's ids and weights tensors before
reshaping them. Since those buffers cross from the backend that computes
top-k/gather to the CPU custom op, an apparently contiguous stride does not prove
that the producer's allocation was already materialized for the scheduler's
copy. The graph always creates a `ggml_cont` producer before the reshape,
including for the first token and slot. That closes the read of uninitialized
memory that showed up as `NaN` in `ffn_moe_weights`;
`llama.cpp/tests/test-moe-route-layout.cpp` covers the contiguous and strided
views.

#### Numeric proof before performance

The MoE path's coherent answer does not prove the callback computed the FFN:
telemetry only proves selection and execution. The numeric regression starts at
the first layer and the first stage, and only moves on when the comparison
passes. The ignored test `nvfp4_matches_llama_cpp_from_first_layer_and_stage`
gets a real GGUF through `POM_MOE_NUMERIC_MODEL`, derives the number of experts
from the file's tensors and walks the layers in order. For each identical
deterministic expert and hidden state, it compares `gate`, `up`, activation and
`down` of the Rust CPU decoder with an oracle built by ggml/llama.cpp itself on
the model's same tensors. The first stage that fails is the diagnosis published
before any fix.

The initial criterion is `abs(a-b) <= 1e-4` or relative error `<= 2e-3` for the
quantized formats; F32 uses `1e-5` absolute and relative. Non-finite values or a
divergence above the limit fail with layer, stage, index, absolute and relative
error. The same matrix will be repeated with the CUDA executor against Rust CPU
once the Rust CPU versus llama.cpp matrix is green. That separates
unpacking/scale errors, geometry or routing errors and CUDA kernel errors
without attributing performance to a wrong result.

##### NVFP4 convention used by the proof

Sidecar resolution is generic: for a base tensor `X.weight`, a sidecar's
canonical name is `X.scale` or `X.input_scale`. The loader refuses a missing
canonical tensor, naming it; it does not turn absence into a scale of 1. The six
sidecars per expert are kept in the payload: `.scale` and `.input_scale` for
gate, up and down.

The external computation follows the semantics observed in the fork's native
oracle. `build_lora_mm_id` does the multiplication with the NVFP4 weight and
applies the matching `.scale` after the per-expert result. The `.input_scale`
field is loaded by the model, but is consumed neither by the native
`build_moe_ffn` nor by any other operation of that FFN. That is why the external
executor does not apply it to the hidden state, to gate/up or to the down
activation. That does not discard the metadata: if a future GGML operation
explicitly declares that field as part of the input quantization, the rule will
be to apply it before that operation's multiplication, with an independent
proof; this decision must not be reused without that evidence.

For the E2M1 values, the fork defines `kvalues_fp4` as the doubled values
`[0, 1, 2, 3, 4, 6, 8, 12, 0, -1, -2, -3, -4, -6, -8, -12]`. The UE4M3 scale is
converted to the raw value divided by two. The doubled table and the `0.5`
factor are a single ggml convention, shared by NVFP4 and MXFP4; using the halved
E2M1 table together with UE4M3 divided by two applies a double reduction. Rust
CPU and CUDA now use exactly that convention, protected by a unit test and by
the comparison against ggml.

There is a known limit on the synthetic input `POM_MOE_NUMERIC_INPUT=ones`: on
the CUDA test server's x86 host, the native backend uses eight AVX2
accumulators, FMA and a horizontal reduction, while a scalar implementation
accumulates sub-block by sub-block. That non-associativity produces an error
centered only on the down stage of some synthetic cases, without indicating a
scale, zero-point or routing difference. The executor selects the AVX2 order
when the matching capabilities are available and keeps the scalar reduction on
other architectures. The proof with a deterministic hidden state on the real
Gemma4 GGUF passed on every layer and stage; that real result, and not a
relaxed tolerance, is the correctness criterion.

The real proof's command, outside the default suite because it needs a large
file:

```bash
POM_MOE_NUMERIC_MODEL=/path/model.gguf \
POM_MOE_NUMERIC_ACTIVATION=2 \
cargo test -p node nvfp4_matches_llama_cpp_from_first_layer_and_stage -- --ignored --nocapture
```

`POM_MOE_NUMERIC_ACTIVATION` is GGUF data, not a per-architecture selection in
the executor. The route capture is not used as the oracle: the test first uses
the same hidden state, expert and weights of an isolated case; the later E2E
proof reuses each layer's real hidden state. The layout fix's `ggml_cont` is
deliberately called even when `ggml_is_contiguous` returns true: only the
producer's execution proves the area is ready for the cross-backend copy. The
per layer/token cost still has to be measured separately, since current
telemetry counts neither bytes nor time of that copy.

To prove the full regression before redeploying, run the ignored test below in
isolation with a `qwen35moe` GGUF and real CUDA:

```bash
POM_MOE_NUMERIC_MODEL=/path/qwen35moe.gguf \
POM_MOE_NUMERIC_GPU_LAYERS=99 \
cargo test -p node --features cuda --bin pom \
  inference::llm::pool_tests::qwen35moe_external_route_matches_native_at_layer \
  -- --ignored --nocapture
```

It captures layer 1's output for four tokens on the native path and on the
external callback, requires `moe_forward_us > 0` and only `cuda` executors, and
fails if any residual is not finite. The published `MOE_NUMERIC_PROOF`
measurement must show `max_abs` and `max_relative`; each element must satisfy
`abs <= 5e-2 + 2e-2 * max(abs(native), 1)`. That combination proves the route
was materialized and read without NaN, that the callback really used CUDA and
that the external result follows the native block. The command requires the
CUDA feature and must not be replaced by `cargo test` without `--ignored`.

For the Ornith-1.5-35B-Q4_K_M proof, use the real full GGUF, not a slice, as
the value of `POM_MOE_NUMERIC_MODEL`. The exact path was not in that worktree,
so it stays an explicit placeholder:

```bash
POM_MOE_NUMERIC_MODEL=/absolute/path/Ornith-1.5-35B-Q4_K_M.gguf \
POM_MOE_NUMERIC_GPU_LAYERS=99 \
CUDAARCHS=120 \
CUDACXX=/usr/local/cuda-12/bin/nvcc \
CCACHE=ccache \
RUSTC_WRAPPER=ccache \
CARGO_TARGET_DIR=/data/ftk-cuda-target \
cargo test -p node --features cuda --bin pom \
  inference::llm::pool_tests::qwen35moe_external_route_matches_native_at_layer \
  -- --ignored --nocapture
```

That proof was not run: it was waiting for explicit authorization to spend
CPU/GPU on the CUDA test server.

Done when the telemetry of a MoE request shows the callback executed,
`executed_engine=MoE`, with no divergence, and the isolated measurement compares
prefill and decode against F5.0. The dense graph must stay without a callback and
without regression.

#### F5-E2: opt-in suppression of weight loading

When and only when F5-E1 is selected, the fork's loader gets an explicit option
to not allocate the expert tensors. It keeps in the model object only the
metadata needed to build the hook's descriptor: names, rank and geometries,
dtype, total and active expert counts, plus the scales and whether gate/up is
fused or separate. The weight bytes stay accessible only to the Rust executor's
GGUF source and its LRU cache, not to `llama.cpp` buffers.

The local implementation already propagates
`EngineConfig::moe_external_executor` down to `llama_model_params`, separates
that option in the `ModelCache` identity and creates the descriptor's routed
experts in a GGML `no_alloc` context. They do not enter `ctxs_bufs`, get no
CUDA/CPU buffer and do not go through `load_all_data`. The metadata context
stays alive in `llama_model` so the hook keeps reading name, dtype and geometry.
Classification is by the generic semantic enums `LLM_TENSOR_*EXP`, never by
architecture, model name or dtype. Shared experts (`LLM_TENSOR_*SHEXP`) stay in
`llama.cpp` because they are a separate native computation and are not yet part
of the callback's descriptor; they are neither silently discarded nor counted as
owned by the Rust cache.

The fork's document `llama.cpp/docs/rdma-p2p-moe-hook.md` records the load
point, the metadata context and the default behavior. A missing callback in
opt-in mode refuses at the decode boundary, so it does not reactivate the native
experts through a silent fallback. The local build without CUDA passed; the
mandatory validation with `--features vulkan,cuda` was to be done on the
`fm/ftk-cuda-check` branch on the CUDA test server after the current deployment
freed the card, before promoting this phase to `feat/ftk`.

The opt-in must be generic per architecture and dtype. Without it, or without
the MoE engine selected, the loader keeps the current behavior byte for byte. A
dense model can never turn the option on. The fork will have its own document
with the changed load point, the preserved metadata and the default behavior.

Done only with a measurement on the CUDA test server using the same model and
configuration: VRAM and RAM before and after, with a per-buffer breakdown
(`CUDA0`, `CUDA_Host` and CPU), the process's RSS and PSS, plus prefill and
decode per token. The proof must show that `llama.cpp`'s expert buffers
disappear and that there is no double residency with the executor's cache. No
gain will be attributed to the load suppression without comparing those numbers
in isolation to the numbers obtained after F5-E1.


#### Local implementation of device residency

The loader's F5-E2 is already in the fork published at `03902a5ba` and the Rust
branch already propagates `EngineConfig::moe_external_executor` down to it. The
new implementation separates hot storage from the cold source:
`DeviceExpertCache` only keeps an opaque backend handle and its leases, while
`ExpertCache` stays the RAM LRU for the CPU paths. On the CUDA path, a miss reads
the payload from the GGUF source and drops it after the upload. The device LRU
is created once per NVFP4 `MoeRuntime` and shared by every layer, so there is no
independent LRU per layer.

The contract is deliberately independent of CUDA and NVFP4. `DeviceAllocator`
measures free memory, uploads and returns an allocation with size and handle;
any future backend can implement the same boundary. The current CUDA
implementation uses `cudaMemGetInfo`, `cudaMalloc`, `cudaMemcpy` only on
admission and `cudaFree` on eviction. The budget starts from the free memory
observed after loading the model and is recomputed on each miss; there is no
fixed card capacity. A refusal names the device, the expert key, the requested
bytes and the measured limit. An empty payload, a probe failure, an incomplete
upload or a null handle also refuse explicitly.

#### Admission fix by measured workspace

The budget now also keeps the transient workspace `llama.cpp` itself measured
after building the context. `LlmEngine::cuda_compute_bytes` extracts the
`CUDA*` buffers from `llama_rs_context_memory_json`, only sums the
device-resident `compute` field and ignores `CUDA_Host`; a missing, incomplete
report or one without positive CUDA bytes refuses the CUDA executor with an
explicit message. The reserve is `ceil(1.25 * compute_bytes)` using integer
arithmetic, a relative margin of 25% for graph growth and fragmentation, not a
fixed card, context or model capacity.

`DeviceExpertCache` uses that reserve both in the initial ceiling and in each
miss's new free memory measurement. If the expert named by the
`model/layer/expert/format` key does not fit after the reserve and the eviction
of idle entries, admission refuses and includes device, key, requested bytes and
available limit. There is no allocation attempt to find out whether it fits, nor
a silent fallback to a residency that would eat the graph's workspace. The test
`device_cache_refuses_an_expert_when_the_measured_workspace_used_up_the_budget` covers
the tight case with a fake allocator. That fix was a local implementation;
promotion requires a CUDA build on the check branch and validation in the real
environment, without touching the service during the operator's measurement.

In NVFP4, the batch now sends the small list of resident pointers to the GPU.
The batch's internal H2D contains the hidden state and pointers, not the
weights; `upload_ms` and `upload_bytes` stay separate in the `device_cache_*`
statistics. A key's first access is a miss and does one admission/upload; later
hits do no weight H2D. A handle's lifetime is protected by a lease, and eviction
only removes an entry without a lease. The payload's contiguity, geometry and
numeric validation are still checked before admission, so residency does not
relax the correctness proof.

The unit test `device_cache_hit_reuses_the_handle_without_a_new_upload` uses a fake
allocator and proves one upload for two accesses; the adjacent tests prove an
active lease and a refusal by the measured budget. Those tests do not replace
CUDA validation. That commit still had to be built on the CUDA test server with
`--features vulkan,cuda`; no deploy, model swap or throughput measurement was
done in that step because the card was in use by the operator.

The fork did not change in that step: the generic replacement ABI and the
metadata-only loader are already in `03902a5ba`. If the CUDA build reveals a
kernel ABI incompatibility, it will be a separate commit in the fork, published
before promoting the Rust branch.

#### F5-E3: route batch on the device

After residency, the NVFP4 path still called the FFI once per token and
returned one vector per expert for the Rust aggregation. That kept building the
batch on the host and repeated allocations, hidden copies and intermediate D2H.
The next revision keeps the resolved route as data: the expert ids, token ids,
weights and per-token offsets go into a single FFI per layer. CUDA computes
gate/up, GeGLU and down for every resident `(token, expert)` pair and does the
weighted sum on the device; only the final `[tokens, hidden]` result goes back to
the host. The CPU path stays token by token and is not changed by this phase.

This change claims no tok/s gain without a measurement. It must be validated
with the numeric proof of the same hidden state, weights and route before any
benchmark. The CUDA test `cuda_nvfp4_executor_route_batch_aggregates_tokens_on_the_device`
compares two tokens, four route pairs and the device aggregation against the CPU
decoder. The CUDA build test on the CUDA test server stays mandatory before
promotion; no deploy, model swap or throughput measurement is part of that step
while the card is in use by the operator.

#### F5-E4: regression and publication

Each phase produces a separate commit in POM and in the fork. The execution test
must cover the opt-in callback, a missing callback and the refusal by name. The
load test must cover preserved metadata, absence of the data tensor and an
intact dense path. The dense suite stays mandatory; the performance and
residency proof requires the real model on the CUDA test server and cannot be
replaced by a fixture.

#### F5-E5: CUDA bridges per dtype, before CUDA Graphs

This was the active gap of that phase. The priority is structural and anchored
in the numbers already measured, not in the expectation of a small gain: in
decode, the honest MoE executor was at `5.4 tok/s`, against llama.cpp's
`20.7 tok/s` and FreeToken's `53.3 tok/s`; in prefill, it was at `3.2 tok/s`,
against `55 tok/s` and `325 tok/s`, respectively. The Qwen UD IQ dtypes stayed
on `CPUReference`, about five seconds per token, before the per-dtype CUDA
bridge. The bridge is now proven with a differential fixture; the Qwen UD E2E
measurement is still the next product validation.

After grouped execution and device residency, the most recent Chat measurement
on the same machine recorded `23.3 tok/s` on MoE against `26.7 tok/s` on
llama.cpp. In prefill it was `449.8 ms` for 23 tokens on MoE (`19.56 ms/token`)
against `172.3 ms` (`7.49 ms/token`) on dense, and the decode p50 was
`29.712 ms` against `29.231 ms`. MoE's p95 was `38.363 ms`, against `29.521 ms`
on dense; that characterizes a worse tail, but does not prove a cache miss by
itself. The observed placement breakdown was 30 layers with weights on the GPU
on MoE against 23 on the GPU and 7 mixed on dense. Those numbers are a baseline,
not a statement that the VRAM reserve was already validated.

The concrete order is Q4_K (`dtype=12`) and Q6_K (`dtype=14`), used by Qwen3.6
Q4_K_M, followed by IQ3_S (`21`), IQ2_S (`22`) and IQ4_XS (`23`), used by the
corpus's two real UD GGUFs. The implementation gets dtype, geometry, role and
payload as data; there is no branch per architecture, model name or global
family. Each bridge must compare the CUDA output with the Rust CPU decoder for
the same payload, hidden state and route, starting at the first layer and the
first stage. The proof with a real file moves the format from "implemented" to
"proven"; a synthetic fixture or `cargo test` without CUDA execution does not
grant that promotion.

A dtype without a bridge stays explicitly visible as `CPUReference`, with
`dtype=<id> (<name>)` and a reason, or is refused at the boundary of a CUDA call
when the contract requires CUDA. An uncovered payload is never interpreted as
another format. In that delivery the bridge is declared for Q2_K, Q3_K, Q4_K,
Q6_K, IQ3_XXS, IQ3_S, IQ2_S, IQ4_XS and MXFP4 only after the respective proofs;
the other IQ formats stay protected by the refusal by name.

When a heterogeneous route only combines those nine dtypes, the CUDA decision is
made per tensor, not by the equality of a global `KQuantFormat`. So a
Q2_K/Q3_K/Q4_K/Q6_K combination like Qwen3.6's can use the same heterogeneous
executor, with `k_dot` choosing each tensor's already validated decoder. A dtype
outside that set uses the external CPU decoder when it exists; if it does not,
admission refuses the combination before loading and lists the dtypes that
blocked the bridge. The mixed route's specific differential comparison is in the
test `heterogeneous_cuda_k_quants_executor_matches_cpu_decoder_q2k_q3k_q4k`,
with an absolute tolerance of `1e-4` and a report of the relative difference too;
it was run with real CUDA before declaring the combination proven.

llama.cpp's common callback delivers `selected_experts` and `weights` in the
token-major order produced by `ggml_get_rows` for `[1, experts_used, tokens]`,
while the Rust runtime also groups batches as `[token, expert_slot]`. The
boundary in `llm.rs` validates and keeps that order, with no extra transposition,
before calling the runtime. The finiteness validation stays before the upload
and identifies `ffn_moe_weights`, architecture, layer, token, slot and expert in
the error. That matters for `qwen35moe`, which uses the same generic
`build_moe_ffn` for routed experts and keeps the shared expert on another path;
there is no alternative route contract per architecture.

Done criterion per block: a green `--features vulkan,cuda` build on the check
branch; a differential test run for each dtype; the matching real GGUF starts and
answers with `moe_executor=cuda`, without `engine_mismatch` or a silent fallback;
and a later tok/s measurement shows the real value against the `5.4/3.2`
baseline and the FreeToken target `53.3/325`. While the CUDA test server is in
use, building is only allowed with prior notice in the status and no measurement
or service change.

#### F5-E6: CUDA Graphs of the MoE path

Graphs come after the per-dtype bridges. The graph will be data driven and
indexed by a shape key that includes device, dtype, hidden, intermediate, number
of tokens, route pairs, strides and activation mode. Capture only happens after
the resident cache's handles, workspaces and descriptors are stable; shape
changes create another graph or refuse the capture by name. Replay must not
silently switch to CPUReference.

The done criterion is a numeric proof before/after replay for each key,
followed by a CUDA build and a tok/s measurement on the same load. The gain will
be reported as a difference in tok/s and in launch/allocation cost, without
attributing to the graphs gains that came from the dtype bridge, the VRAM cache
or the batch build. The goal's target stays approaching FreeToken's
`53.3 tok/s` in decode and `325 tok/s` in prefill; a green suite without a number
closes none of those phases.

### Phases of the CUDA goal

1. **F5.0, reproducible reference profile.** Rerun on the CUDA test server the
   same Gemma4, GPU, prompt and configuration that produced the operator's
   numbers, separating router, LRU wait, dequantization, H2D, kernel, D2H and the
   rest of `llama_decode`. This phase's target is reproducing `54 ms/token` of
   prefill and `80 ms/token` of decode within a `+/-10%` band, with the share
   outside `llama_decode` still above 90%. Without that agreement the new number
   is not comparable.

2. **F5.1, minimal CUDA executor for prefill.** Implement in CUDA the experts'
   mat-vec and the gate/up/down sum, with dispatch, leases and slice copies
   controlled by Rust. The first path covers the already proven Gemma4 NVFP4 and
   keeps the CPU decoder only as an explicit fallback. Done when 100% of the
   expert forwards of the reference load's prefill publish `gpu_expert_calls`,
   `cpu_expert_calls=0`, the output passes the numeric comparison with the CPU
   decoder and prefill reaches `<= 5.4 ms/token`, a reduction of at least one
   order of magnitude over the current `54 ms/token`. The final target of
   `3.1 ms/token` is still measured in F5.3; the `5.4` milestone cannot be
   replaced by just not regressing.

3. **F5.2, real bandwidth profile and double buffer.** Measure H2D and D2H on
   the same GPU, for the load's token and expert sizes, persist the effective
   profile and select the CPU/GPU policy by the measured time. Replace the test
   scaffold with two truly alternating CUDA buffers, so while one buffer is
   computed the other can be transferred. Done when the runtime loads the
   measured profile, records the two indexes in use and, over 100 warm iterations
   per load point, the combined time satisfies
   `T_total <= 1.10 * max(T_compute, T_transfer)`. The safety target is not
   regressing the prefill measured in F5.1.

#### Initial F5.2 measurement - 2026-08-25

The measurement was run on the CUDA test server, on an RTX 5060 Ti, with
`POM_MOE_CUDA_PROFILE=1`, `CARGO_TARGET_DIR=/data/ftk-cuda-target` and
`CUDACXX=/usr/local/cuda-12/bin/nvcc`. The service was not touched: the test ran
on a temporary checkout and the expert weights were resident in the CUDA cache
before the 100 iterations of each point. The reproducible command was:

```text
POM_MOE_CUDA_PROFILE=1 CARGO_TARGET_DIR=/data/ftk-cuda-target \
CUDACXX=/usr/local/cuda-12/bin/nvcc \
cargo test -p node --features vulkan,cuda --bin pom \
  measures_the_h2d_d2h_band_of_the_nvfp4_batch -- --ignored --nocapture
```

Observed output:

```text
CUDA_BANDWIDTH case=tiny hidden=64 intermediate=64 tokens=2 active_experts=2 calls=400 h2d_bytes=60000 h2d_ms=1.998 effective_h2d_GB_s=0.030 d2h_bytes=51200 d2h_ms=0.760 effective_d2h_GB_s=0.067 kernel_ms=7.400 wall_ms=14.320
CUDA_BANDWIDTH case=gemma4_token hidden=2816 intermediate=704 tokens=1 active_experts=8 calls=800 h2d_bytes=1140800 h2d_ms=2.262 effective_h2d_GB_s=0.504 d2h_bytes=1126400 d2h_ms=0.818 effective_d2h_GB_s=1.377 kernel_ms=22.398 wall_ms=39.021
CUDA_BANDWIDTH case=gemma4_batch8 hidden=2816 intermediate=704 tokens=8 active_experts=8 calls=6400 h2d_bytes=9120800 h2d_ms=3.543 effective_h2d_GB_s=2.574 d2h_bytes=9011200 d2h_ms=2.209 effective_d2h_GB_s=4.079 kernel_ms=162.502 wall_ms=185.358
```

Those values are effective rates of the small copies made by a synchronous FFI
call, not the PCIe's maximum bandwidth. The weight bytes do not show up in the
call's H2D because the test measures the resident path; the initial upload is
accounted separately by the cache. The current call still allocates and frees
buffers per forward, does two `cudaDeviceSynchronize` and only then returns,
which is why there is no overlap in that measurement. At the two representative
points, `T_total / max(T_compute, T_transfer)` was `39.021 / 22.398 = 1.742` and
`185.358 / 162.502 = 1.141`, respectively. Both fail the `1.10` criterion; F5.2
stayed open and no co-execution gain was declared.

#### Measurement after the two CUDA slots - 2026-08-25

Commit `00bf6f1` replaced the route path with a pipeline of two persistent
slots. Each slot has a stream, device buffers and pinned host buffers; when there
are at least two token groups with routes, the second group is prepared and
transferred while the first computes. Telemetry now separates `kernel_ms`
(summed work), `kernel_span_ms` (critical window across slots) and
`transfer_span_ms` (H2D plus D2H of the critical window), and publishes
`buffer_mask` and `buffer_switches`.

On the same CUDA test server, GPU and command as the previous measurement, the
output was:

```text
CUDA_BANDWIDTH case=tiny hidden=64 intermediate=64 tokens=2 active_experts=2 calls=400 buffer_mask=3 buffer_switches=100 h2d_bytes=60800 h2d_ms=2.885 effective_h2d_GB_s=0.021 d2h_bytes=51200 d2h_ms=0.795 effective_d2h_GB_s=0.064 kernel_ms=8.576 kernel_span_ms=6.053 transfer_span_ms=1.959 wall_ms=16.260
CUDA_BANDWIDTH case=gemma4_token hidden=2816 intermediate=704 tokens=1 active_experts=8 calls=800 buffer_mask=3 buffer_switches=0 h2d_bytes=1140800 h2d_ms=1.840 effective_h2d_GB_s=0.620 d2h_bytes=1126400 d2h_ms=0.472 effective_d2h_GB_s=2.387 kernel_ms=22.553 kernel_span_ms=22.553 transfer_span_ms=2.312 wall_ms=28.768
CUDA_BANDWIDTH case=gemma4_batch8 hidden=2816 intermediate=704 tokens=8 active_experts=8 calls=6400 buffer_mask=3 buffer_switches=100 h2d_bytes=9121600 h2d_ms=4.120 effective_h2d_GB_s=2.214 d2h_bytes=9011200 d2h_ms=1.386 effective_d2h_GB_s=6.502 kernel_ms=333.662 kernel_span_ms=168.081 transfer_span_ms=3.036 wall_ms=180.647
```

The `gemma4_batch8` point satisfies the measured criterion:
`180.647 / max(168.081, 3.036) = 1.075`, below `1.10`, and `buffer_mask=3`
proves both slots were used. The one-token point is still at
`28.768 / 22.553 = 1.276`, and the small point at `16.260 / 6.053 = 2.686`; so
the result does not allow declaring the whole F5.2 done. In the revision that
produced that measurement, the pipeline was integrated into the runtime and
telemetry recorded the slots, but the profile was not persisted/loaded yet and
the CPU/GPU selection did not consume comparable times measured for both
backends. The F5.2b delivery below closes that selection gap and leaves the CUDA
measurement as the remaining gate.


#### F5.2b: NVFP4 co-execution by measured bandwidth

This delivery closes the next operational wiring, without declaring a
performance gain before measuring. `BandwidthProfile` persists in the CUDA
sidecar the CPU and transfer bandwidths measured in isolation and under
contention; when the overlapped bandwidths exist, they take precedence. The
share of misses sent to the GPU is `transfer/(cpu+transfer)`, bounded to
`[0, 1]`, and the policy splits unique experts in a deterministic order. Hits
already resident in the device cache stay on the GPU, so an expert repeated in
the same route is not admitted on both lanes.

The runtime loads the profile once per NVFP4 model and passes it to each block.
In the route forward, the CPU lane uses the global RAM `ExpertCache` and the Rust
NVFP4 decoder; the GPU lane uses the resident cache and the CUDA kernel. The two
functions run in sibling threads and the output is summed per token, with the
width validated before combining. The single path turns the route into a
one-token batch, so no exception without co-execution is opened. A missing
profile keeps every miss on the GPU, preserving the previous behavior; an
invalid profile refuses the load by name.

Diagnostics now expose `cpu_expert_calls`, `gpu_expert_calls` and
`coexecution_gpu_fraction`, besides the existing CUDA measurements. The unit
tests cover the bandwidth policy, duplicates, a lane error, the double buffer
reservation, the lanes' sum and the sidecar's persistence. The CUDA build and
this revision's focused tests were run on the CUDA test server; no effective
speed or overlap is claimed in this text beyond the synthetic measurements
recorded below.

On the CUDA test server, the build of revision `ffe9669` was run with
`CUDAARCHS=120`, `CUDACXX=/usr/local/cuda-12/bin/nvcc`, `CCACHE=ccache`,
`RUSTC_WRAPPER=ccache` and `CARGO_TARGET_DIR=/data/ftk-cuda-target`. The release
build ended in `9m45s`, with `BUILD_EXIT=0`; ccache recorded `524/532` cacheable
calls (`98.50%`) and zero hits because the target was cold. The focused CUDA
tests passed: `7` co-execution tests, `1` sidecar persistence test and the
synthetic H2D/D2H benchmark.

The co-execution test measured CPU and CUDA uploads simultaneously on eight
synthetic experts of the Gemma4 geometry. The output was:

```text
COEXEC_PROFILE cpu_ms=925.491 gpu_ms=117.632 upload_ms=3.765 payload_bytes=3345440 gpu_fraction=0.9959
```

The sidecar produced by the test contained `cpu_wall_ms=925.491047`,
`cuda_wall_ms=117.632127`, `cpu_overlap_bytes_per_sec=28918183.58` and
`transfer_overlap_bytes_per_sec=7107826903.09`, resulting in a GPU fraction of
`0.9959`. That is a synthetic measurement of one geometry, not an E2E
measurement of the real model. The CUDA test server only had the Gemma4 `.pom`
manifest at the queried path, without the slices' GGUFs, so the ignored proofs
`cuda_nvfp4_executor_batch_preserves_the_real_gguf_geometry` and
`nvfp4_matches_llama_cpp_from_first_layer_and_stage` did not run for lack of
the required file. No performance gain is attributed to the co-executed path on
the basis of that test.

4. **F5.3, prefill on target.** Optimize expert grouping, operation fusion and
   LRU/staging reuse guided by the F5.2 profile. Done only when the prefill
   average, with the same definition as the reference, is `<= 3.1 ms/token` on
   the CUDA test server, with the numeric error within F5.1's tolerance,
   `engine_mismatch=false`, no CPU fallback and decode still `<= 80 ms/token`
   during the measurement.

5. **F5.4, decode parity with llama.cpp.** Apply the same lane to one-token
   decode, first without requiring the FreeToken target. Done when the decode
   p50 reaches `<= 30 ms/token` on the same load and GPU, tying the `llama.cpp`
   measured on the real Chat, while the prefill stays `<= 3.1 ms/token`.
   Telemetry must show `executed_engine=MoE`, without mixing or divergence; a
   fallback must fail by name or show up with its reason, never be counted as
   GPU.

6. **F5.5, FreeToken target and full co-execution.** Overlap routing, transfer
   and computation across requests, keeping the LRU's leases. Done when decode
   reaches `<= 18.8 ms/token` on the same load and GPU, prefill stays
   `<= 3.1 ms/token`, and each hop's telemetry shows `executed_engine=MoE`,
   without mixing or divergence.

7. **F5.6, final proof and regression.** Repeat points F5.0 to F5.5 in at least
   three independent runs on the CUDA test server, attach the median and spread
   of the times, and run the full dense suite. The goal only closes if the
   `3.1 ms/token` and `18.8 ms/token` limits are reached on the reference load,
   with no functional or performance regression observed on the dense path. If a
   phase does not reach its number, record the real value and the evidence,
   without declaring it green from a unit suite.

This resumption replaces the earlier state of "Phase 5 not active yet". Before
any kernel, the design and those targets had to be published in the goal's
status. F5.1 already has its first code delivery: `cuda.rs` does the Rust
dispatch and `cuda_kernels.cu` runs NVFP4 gate/up/down in CUDA, compiled by
`build.rs` only with the `cuda` feature. `POM_MOE_NVFP4_BACKEND=cpu` is the
explicit fallback for comparison; a CUDA kernel failure does not silently fall
to the CPU. The CUDA build was validated on the CUDA test server, but the
kernel's numeric comparison and the prefill measurement were still waiting for a
window without GPU contention. Until that proof, no performance improvement is
claimed.

## Goal: MoE executor for every GGUF dtype

This is a formal goal added to the `feat/ftk` path: the own runtime must run MoE
experts in every active GGUF dtype supported by `llama.cpp`'s `ggml_type`, not
only Q4_0. The source of the enumeration is `llama.cpp/ggml/include/ggml.h`,
especially `GGML_TYPE_COUNT`; IDs removed from GGUF files are not treated as
valid formats. The scope keeps the fork: dense models stay on `llama.cpp`, and no
weight or quantization block crosses the network because of slicing.

The safety contract is common to every phase: the GGUF header is validated
before any decoding, dispatch uses the tensor's ID and geometry, and an
unimplemented combination refuses with `dtype=<id> (<name>)`. An unknown dtype
is also refused with its raw ID. Interpreting one format's bytes as another's is
forbidden. When the goal is done, that refusal will be the rare path for a truly
new dtype or an invalid tensor, not the normal exit for known models.

### Reorganized delivery milestone - 2026-08-25

This goal's operational criterion was reorganized to deliver value on the four
real MoE models available on the CUDA test server before broadening the
abstract coverage of every dtype. The milestone is: Gemma4 NVFP4, Qwen3.6
Q4_K_M with Q4_K/Q6_K, Qwen UD-IQ3_S with heterogeneous IQ3_S/IQ2_S/IQ4_XS and
Ornith UD-IQ3_XXS. Each one must start and answer on the MoE path, with
`moe_executor` visible per hop, without a silent `CPUReference` and without
regression against llama.cpp on the same build. The acceptance number is tok/s
measured on the CUDA test server; a green suite or a metadata load alone does
not close the milestone.

The current order becomes:

1. **Per-dtype safeguard and VRAM reserve.** The measured reserve of `53decb4`
   and per-layer ownership arrive together. A layer whose expert tensors have a
   CUDA executor or an available `CPUReference` can be taken out of llama.cpp.
   Dtypes without a device cache use `CPUReference` and publish the concrete
   dtypes; a combination without an external decoder is refused before loading.
   Only a real runtime limitation keeps the layer native, with a visible
   reason, never leaving the card without an owner.
2. **Milestone coverage.** NVFP4, Q2_K/Q3_K/Q4_K/Q6_K and then
   IQ3_S/IQ2_S/IQ4_XS per tensor, including the device cache and the
   heterogeneous combination used by the two UD models. The decoder is chosen
   per tensor and dtype; architecture, names, geometries and conventions come in
   as metadata. Each block requires a numeric proof against the oracle, a real
   GGUF when available and an E2E proof on the four models before being called
   done.
3. **After the milestone.** Prefill, CUDA graphs and tail reduction, prioritized
   by the measured profile. The remaining dtypes stay covered by the safeguard
   and only enter the device executor after the milestone, without changing the
   native path.

This reorganization replaces the previous queue as the delivery order, but does
not remove the full inventory nor the refusal by name: the other dtypes stay
listed and explicitly protected, and come back to implementation once the four
milestone models have honest numbers.

### State of heterogeneous IQ coverage - 2026-08-25

The next phase's device bridge is implemented locally for the dtypes IQ3_S
(21), IQ2_S (22) and IQ4_XS (23), both in homogeneous files and in
heterogeneous GGUFs. In the heterogeneous case, the cache keeps one resident
entry per `(model, layer, expert, role, dtype)`; the kernel gets separate
descriptors for fused `gate_up` or separate `gate`/`up` and `down`, and picks the
codebook by each tensor's dtype. That avoids turning the UD into a homogeneous
payload on the host and avoids weight transfers on cache hits.

The plan only externalizes a heterogeneous combination when every expert dtype
belongs to the supported set. A combination like `[12, 21, 23]` stays entirely
with llama.cpp and publishes the refusal by name; there is no partial
externalization that could remove a tensor without an executor. The proof on the
CUDA test server ran the individual path for IQ3_S, IQ2_S and IQ4_XS against the
CPU decoder and also the heterogeneous path with gate IQ3_S, up IQ2_S and down
IQ4_XS. In every case `gate_up`, activation and `down` had no first divergence at
the `1e-4` tolerance. The E2E proof on the two UD models' real GGUFs and the
product measurement were still pending.

### State of scalar CUDA coverage - 2026-08-25

F32 (0), F16 (1) and BF16 (30) now share a CUDA bridge parameterized by
`PlainFormat`. The payload used by `PlainSource` is admitted only once into
`DeviceExpertCache`, including the `gate_up` and `down` length header; the kernel
validates those lengths, decodes the given dtype and runs gate/up, SiLU, down and
aggregation in the route batch. There is no branch per architecture or model
name, and a dtype outside `{0, 1, 30}` is refused before launch.

The differential CUDA test covers the three formats with the same payloads and
hidden state as the CPU `PlainExpert`, with a tolerance of `1e-5` for F32 and
`2e-3` for F16 and BF16. The build and that test still had to pass on the
validation worktree, in `/data/ftk-cuda-target`; there is also no E2E proof with
a real MoE GGUF of those three dtypes. Without CUDA, or if admission by the
measured budget refuses the payload, ownership stays with llama.cpp and
telemetry reports the reason.

### IQ differential diagnosis - 2026-08-25

Commit `d631442` made the IQ comparison diagnosable per dtype and stage: the CPU
test reports the first non-finite value, and the CUDA proof records the first
diverging index in `gate`, `up`, activation or `down`, with the actual value,
the expected one, absolute error and relative error. The isolated CPU fixture
passed:

```text
CARGO_TARGET_DIR="$PWD/target/ftk-cuda-check" cargo test -p node --bin pom each_prioritized_iq_has_a_defined_block_and_dequantization -- --nocapture
1 passed, 891 filtered out
```

The first remote attempt passed `--ignored`, but those tests are not ignored and
the output was `0 passed, 907 filtered out`; that result was not treated as a
success. Validation was repeated on the CUDA test server, with
`CARGO_TARGET_DIR=/data/ftk-cuda-target` and
`CUDACXX=/usr/local/cuda-12/bin/nvcc`, and the native CUDA build completed.

In commit `e1a7ab5`, the right test reached the safeguard and returned
`BackendUnavailable("dtype GGML 22 (iq2_s) ainda não tem ponte CUDA IQ-quant")`
("has no IQ-quant CUDA bridge yet"). To investigate without changing the
production dispatch, `86226c7` added a test-only constructor. With it, on the
same CUDA target, the individual proof returned `1 passed` and the per-stage
diagnosis published:

```text
IQ iq3_s estágios: gate_up=None, activation=None, down=None
IQ iq2_s estágios: gate_up=None, activation=None, down=None
IQ iq4_xs estágios: gate_up=None, activation=None, down=None
```

The heterogeneous proof `heterogeneous_cuda_iq_executor_preserves_gate_up_down_per_dtype`
also returned `1 passed`. The historical divergence was not reproduced and there
was no kernel change; the only production change of that phase was re-enabling
the three dtypes after that evidence. The test constructor was removed in the
same delivery, so the proof goes through the normal dispatch.

In the final commit `89156d9`, the normal dispatch repeated the individual proof
and returned `1 passed`; the per-stage diagnosis and the heterogeneous test also
returned `1 passed`. The full suite with
`cargo test -p node --features vulkan,cuda --bin pom` ended with `891 passed`,
`6 failed` and `10 ignored`, out of `907` tests. The six failures are:

- `gguf::tests::canonical_qwen35_nextn_metadata_and_tensor_names_prove_mtp`,
  `gguf::tests::mtp_is_proven_by_real_nextn_tensors_even_without_metadata` and
  `gguf::tests::read_layout_sees_nextn_predict_layers`: `GGUF truncado antes da
  seção de dados` ("GGUF truncated before the data section");
- `direct_lab::the_cost_per_token_does_not_depend_on_where_the_entry_point_is`:
  `N=2 entrada=0: 7,06 pernas por token, esperado 2` ("7.06 legs per token,
  expected 2");
- `transfer::offer_latency_tests::an_offer_leaves_without_reading_the_whole_file`:
  `oferta demorou 27,110586968s; ela não pode depender do tamanho do arquivo`
  ("the offer took 27.1 s; it must not depend on the file size");
- `webui::tests::merge_parts_joins_two_slices_into_a_whole`: metadata
  `llama.attention.head_count_kv_by_layer` with length 16, expected 1 or 32.

Those failures do not involve the changed IQ dispatch. The release build was run
on the same checkout and ended with code 0:

```text
CARGO_TARGET_DIR=/data/ftk-cuda-target CUDACXX=/usr/local/cuda-12/bin/nvcc cargo build --release --features vulkan,cuda
Finished `release` profile [optimized] target(s) in 25m 38s
```

In the same round, the local dense suite was run with
`CARGO_TARGET_DIR="$PWD/target/ftk-cuda-check" cargo test -p node --bin pom`:
`879 passed`, `4 failed`, `9 ignored`, out of `892` tests. The observed failures
were:

- `gguf::tests::canonical_qwen35_nextn_metadata_and_tensor_names_prove_mtp`:
  `GGUF truncado antes da seção de dados` at `gguf.rs:1202:52`;
- `gguf::tests::mtp_is_proven_by_real_nextn_tensors_even_without_metadata`:
  `GGUF truncado antes da seção de dados` at `gguf.rs:1174:52`;
- `gguf::tests::read_layout_sees_nextn_predict_layers`:
  `GGUF truncado antes da seção de dados` at `gguf.rs:1325:38`;
- `webui::tests::merge_parts_joins_two_slices_into_a_whole`:
  `array metadata 'llama.attention.head_count_kv_by_layer' tem comprimento 16; esperado 1 ou 32 (contagem original de blocos)`
  at `webui.rs:5114:61`.

Those failures stayed outside the IQ diagnosis files. They are no proof of the
CUDA bridge and do not change the safeguard.


### Phases and done criteria

1. **Inventory and conformance.** Map every active ID of the enumeration, name
   them in the loader, survey the expert dtypes in a real corpus and add header
   fixtures for supported and refused formats. Done when each ID reaches an
   explicit decision, each refusal names the dtype and the parser reads no
   payload without validating bounds, rank, geometry and offset.

2. **NVFP4, dtype 40.** First implement the reference CPU decoder for the NVFP4
   layout used by Gemma4, including E4M3 scales, blocks, fused or separate
   gate/up, and down. Done when synthetic fixtures cover each scale and sign,
   the result is compared operation by operation with an independent reference
   and a real NVFP4 GGUF completes the opening and the expert forward without
   falling into the Q4_0 loader.

3. **Generic layer and Q4_0 compatibility.** Extract `ExpertDecoder` and a
   per-dtype dispatch, keeping the cache key, memory admission and name formats
   outside the decoder. Migrate Q4_0 and NVFP4 to that layer, with identical
   contract tests per format. Done when the dense path does not import the MoE
   runtime, Q4_0 keeps passing and no branch can pick a decoder by file name.

4. **Coverage by impact, with no family cut.** After the already published
   blocks NVFP4, K-quants `Q4_K` (12) and `Q6_K` (14), and IQ-quants `IQ3_S`
   (21), `IQ2_S` (22) and `IQ4_XS` (23), the remaining queue follows this order:
   first the scalars `F32` (0), `F16` (1) and `BF16` (30), then the classic
   blocks `Q4_1` (3), `Q5_0` (6), `Q5_1` (7) and `Q8_0` (8), and then `MXFP4`
   (39), `TQ1_0` (34), `TQ2_0` (35), `Q1_0` (41) and `Q2_0` (42). The next queue
   covers the other types, abandoning none: numeric `F64`, `I8`, `I16`, `I32`
   and `I64`; K-quants `Q2_K`, `Q3_K`, `Q5_K` and `Q8_K`; and IQ-quants
   `IQ1_M`, `IQ1_S`, `IQ2_XS`, `IQ2_XXS`, `IQ3_XXS` and `IQ4_NL`. The order of
   the first two blocks was set by the real models available on the CUDA test
   server. No Q4_0 MoE exists there, so Q4_0 stays covered by local tests,
   without being a physical validation queue. Each subgroup is done with
   decoder, payload fixture, geometry rejections and a differential test before
   the next one.

5. **Real proof and closing.** Run a real MoE model for each dtype present in
   the corpus, measure loading, cache, forward and tok/s, and compare with the
   `llama.cpp` reference or an independent numeric reference. Done when every
   active dtype of the enumeration has a format test and at least one execution
   proof; only removed or unknown IDs, or invalid GGUFs, may end in a refusal by
   name. The full dense suite stays mandatory in each phase.

### Current GGUF inventory

The active dtypes are: `0 F32`, `1 F16`, `2 Q4_0`, `3 Q4_1`, `6 Q5_0`,
`7 Q5_1`, `8 Q8_0`, `9 Q8_1`, `10 Q2_K`, `11 Q3_K`, `12 Q4_K`, `13 Q5_K`,
`14 Q6_K`, `15 Q8_K`, `16 IQ2_XXS`, `17 IQ2_XS`, `18 IQ3_XXS`, `19 IQ1_S`,
`20 IQ4_NL`, `21 IQ3_S`, `22 IQ2_S`, `23 IQ4_XS`, `24 I8`, `25 I16`, `26 I32`,
`27 I64`, `28 F64`, `29 IQ1_M`, `30 BF16`, `34 TQ1_0`, `35 TQ2_0`,
`39 MXFP4`, `40 NVFP4`, `41 Q1_0` and `42 Q2_0`.

IDs `4 Q4_2`, `5 Q4_3`, `31 Q4_0_4_4`, `32 Q4_0_4_8`, `33 Q4_0_8_8`,
`36 IQ4_NL_4_4`, `37 IQ4_NL_4_8` and `38 IQ4_NL_8_8` stay in the historical
range of the enumeration, but `llama.cpp` marks them as removed from GGUF files.
The loader must reject them as a removed dtype, with the ID and the name in the
message, instead of treating them as one of the active formats.

### Final coverage table of the active dtypes

This table is the honest closing of this round, checked against
`GGML_TYPE_COUNT` and the enumeration in `llama.cpp/ggml/include/ggml.h`. Each
row is individual so as not to turn a fixture into a model proof. The only
physical proof recorded is Gemma4 NVFP4; there is no local GGUF file to repeat
an E2E proof of the CUDA test server's K-quants or IQ-quants in this worktree,
and no remote access was made in this validation.

| ID | dtype | state |
|---:|---|---|
| 0 | F32 | implemented and NOT PROVEN |
| 1 | F16 | implemented and NOT PROVEN |
| 2 | Q4_0 | implemented and NOT PROVEN |
| 3 | Q4_1 | implemented and NOT PROVEN |
| 6 | Q5_0 | implemented and NOT PROVEN |
| 7 | Q5_1 | implemented and NOT PROVEN |
| 8 | Q8_0 | implemented and NOT PROVEN |
| 9 | Q8_1 | not covered |
| 10 | Q2_K | implemented and NOT PROVEN |
| 11 | Q3_K | implemented and NOT PROVEN |
| 12 | Q4_K | implemented and NOT PROVEN |
| 13 | Q5_K | implemented and NOT PROVEN |
| 14 | Q6_K | implemented; local format proof and CPU `gate_up`; real CUDA/E2E pending |
| 15 | Q8_K | implemented and NOT PROVEN |
| 16 | IQ2_XXS | implemented and NOT PROVEN |
| 17 | IQ2_XS | implemented and NOT PROVEN |
| 18 | IQ3_XXS | implemented and NOT PROVEN |
| 19 | IQ1_S | implemented and NOT PROVEN |
| 20 | IQ4_NL | implemented and NOT PROVEN |
| 21 | IQ3_S | implemented and PROVEN by differential CUDA comparison |
| 22 | IQ2_S | implemented and PROVEN by differential CUDA comparison |
| 23 | IQ4_XS | implemented and PROVEN by differential CUDA comparison |
| 24 | I8 | not covered |
| 25 | I16 | not covered |
| 26 | I32 | not covered |
| 27 | I64 | not covered |
| 28 | F64 | not covered |
| 29 | IQ1_M | implemented and NOT PROVEN |
| 30 | BF16 | implemented and NOT PROVEN |
| 34 | TQ1_0 | implemented and NOT PROVEN |
| 35 | TQ2_0 | implemented and NOT PROVEN |
| 39 | MXFP4 | implemented and NOT PROVEN |
| 40 | NVFP4 | implemented and PROVEN with a real file |
| 41 | Q1_0 | implemented and NOT PROVEN |
| 42 | Q2_0 | implemented and NOT PROVEN |

The dispatcher keeps refusing both an active dtype without a decoder and an ID
outside the enumeration before reading the payload.
`Q4LoadError::UnsupportedTensorType` renders `dtype=<id> (<name>)`; for an
unknown ID, the name is `desconhecido` ("unknown"). The tests
`an_unknown_dtype_reports_id_and_name` and
`an_active_dtype_without_a_decoder_reports_id_and_name` cover, respectively, the cases 999
and Q8_1. So the gap is observable and named, never a silent interpretation as
another format.

The table leaves six active dtypes without their own decoder: Q8_1, I8, I16,
I32, I64 and F64. They stay explicit future work; the closing does not declare
them implemented. The other 29 dtypes have a decoder and local tests, but only
ID 40 has an E2E proof with a real GGUF file.

### Experts with heterogeneous dtypes

The old inventory required a single dtype per model and refused the UD GGUFs
that mix IQ3_S, IQ2_S and IQ4_XS. That premise was removed for phase F5-E1.
`detect_expert_format` now returns `Heterogeneous` when the expert tensors have
more than one dtype; `format_heterogeneous.rs` builds an inventory per layer and
role (`gate_up`, `gate`, `up`, `down`) and `MoeRuntime` picks each tensor's
decoder individually. The route, the activation and the normalization still
come from the descriptor of llama.cpp's common hook. There is no branch per
architecture, quantization family or model name.

The heterogeneous path accepts, per tensor, the dtypes with existing decoders:
F32, F16, Q4_0, Q4_1, Q5_0, Q5_1, Q8_0, Q2_K, Q3_K, Q4_K, Q5_K, Q6_K, Q8_K,
IQ2_XXS, IQ2_XS, IQ3_XXS, IQ1_S, IQ4_NL, IQ3_S, IQ2_S, IQ4_XS, IQ1_M, BF16,
TQ1_0, TQ2_0, MXFP4, Q1_0 and Q2_0. NVFP4 stays proven on the homogeneous path;
if it shows up mixed with another dtype, the combination is refused by name per
tensor until its scales are included in the heterogeneous inventory. Q8_1, I8,
I16, I32, I64 and F64 still have no decoder. Any uncovered dtype produces the
tensor name, the ID and the GGML name, without interpreting the bytes as another
format.

In the CUDA bridge, the validated heterogeneous subset is Q4_K (12), Q6_K (14),
IQ3_XXS (18), IQ3_S (21), IQ2_S (22), IQ4_XS (23) and MXFP4 (39). The kernels
receive one descriptor per expert and role, validate the matrix size according
to the dtype and dispatch to the matching K, IQ or MXFP4 decoder. That keeps the
previous IQ path and lets gate, up and down use different dtypes; a missing
bridge with an external decoder in any tensor refuses the load and reports the
blocking dtype.

The ignored test `real_qwen_ud_iq3_s_inventories_dtypes_per_tensor` requires
`POM_REAL_MOE_GGUF` and the real file `Qwen3.6-35B-A3B-UD-IQ3_S.gguf`; it
validates 40 layers, 256 experts and the dtypes `{21, 22, 23}`. The observed
geometry is `[2048, 512, 256]` in gate/up and `[512, 2048, 256]` in down. A
local opening without that file is not considered proof. The E2E proof must run
the same code revision on the host that holds the GGUF and show an answer,
`executed_engine=moe`, no divergence and `input_tokens > 0`.

### Resident cache warmup - F5-E3a

The device cache starts empty by design: loading the model creates the
`DeviceExpertCache`, but the first admission only happened when a route
selected an expert. That left VRAM low after loading and made the first
requests pay for reading the GGUF and uploading the weights.

When there is a CUDA bridge for the selected source, `MoeRuntime` can start an
asynchronous warmup thread when the model loads. It walks the experts by layer
and index, asks the loader itself for each tensor's canonical payload and calls
the same admission as the hot path. Dispatch is by `MoeSource` and by the tensor
metadata; there is no branch per architecture, model or quantization version.
In a heterogeneous GGUF, each role and dtype gets the same key and descriptor
the forward uses.

Warmup is the default, to restore pre-warmed VRAM. The operator can turn it off
explicitly with `POM_MOE_WARMUP=0`, `false`, `off` or `no` to measure a cold
route; `POM_MOE_WARMUP_MAX_MS` still controls the deadline, with 30,000 ms as
the default. The order of the experts in the file is not the order of the real
route, so the budget limits the fill and the first route cancels the rest
before the forward.

The limit is still the measured VRAM budget, including the compute reserve
already reported by `llama.cpp`. When admission returns
`NoBudget`/`BusyAndNoEviction`, warmup stops with `budget-exhausted`; the
candidates not yet visited stay explicitly out of the warmup and remain subject
to the on-demand LRU. It does not fail the load because the whole model does
not fit.

The deadline can be tuned with `POM_MOE_WARMUP_MAX_MS`. The thread checks the
deadline and an `AtomicBool` before each candidate; the inference callback only
signals the stop and does not wait for a CUDA admission in progress, without
removing admitted entries. The `join` in `MoeRuntime::cancel_device_warmup` is
restricted to an explicit warmup swap, outside the hot path. Start and end logs
record model, status, admitted bytes and stop reason. The
`moe_executor_diagnostics` telemetry publishes `warmup_status`, visited
candidates, admitted experts, resident bytes, duration and error. So a startup
is not blocked indefinitely, and a stop by budget, deadline or error does not
become a silent cold cache.

The cache tests cover warmup up to the budget, cancellation before the first
payload and a zero deadline. The CUDA validation of the integration and the E2E
comparison of Qwen UD after warmup still have to be run in the isolated
directory `/data/ftk-cuda-target`; no gain number is claimed before that
measurement.

## Phase 0: local baseline

### Reproducible method

For a real conversation, each answer must keep the `trace` object returned by
`/api/generate` or by `/v1/chat/completions` with usage `stream_options`. For
each turn, record:

```text
prefill_fraction = prefill_ms / (prefill_ms + decode_wall_ms)
input_tokens, output_tokens, prefill_ms, total_ms
sum(hop.prefill.processing_ms), sum(hop.decode.processing_ms)
```

The total time is measured on the client around the request. `RequestTrace`
already separates `prefill` and `decode` per hop and must not be rebuilt from an
aggregated total. The conversation must have at least four turns, keeping the
previous context in each request.

For throughput, use the same prompt and `max_new_tokens` in every run, run 1, 2
and 4 simultaneous requests, and repeat with `max_sessions` 1, 2 and 4. The
`kv_unified` A/B must keep every other parameter equal. For each cell record
output tokens, wall duration, tok/s per request, aggregated tok/s, errors and
the effective configuration reported by `/health`.

### Environment found in this run

| Measurement | Local result | Reason for the gap |
|---|---|---|
| Real multi-turn conversation | not run, `0` requests | there is no GGUF model in the worktree, `ollama list` lists no models and local ports 8181/8080 have no POM server |
| Prefill/decode | `n/a` | depends on real generation and on the `RequestTrace` produced by a loaded model |
| Tok/s with 1, 2 and 4 requests | `n/a`, `0` cells | there is no local runtime nor model to load |
| A/B `kv_unified=true` | not run | the parameter was not exposed in the runtime yet and the only POM instance observed belonged to another worktree and held connections to the remote cluster, which is out of scope |

No access to the remote cluster, SSH or deploy was made. So there is no
performance number to make up. By the operator's decision, Phase 2 went ahead
even without that baseline; the later measurement must use the method above.

Generation measurements stayed out of reach of this worktree without a GGUF and
a local server. The native infrastructure, however, was validated with the
temporary symlink documented in `build-and-run.md`; the results are below.

### Validation run

In the worktree with `<worktree>/llama.cpp` pointing to a local checkout of the
fork:

- `cargo build -p node`: code 0, 0 errors and no warning.
- `cargo test -p node`: 761 tests passed, 5 ignored.
- `cargo fmt --check`: code 0.
- `cargo clippy -p node`: code 0, 0 errors, 45 remaining warnings, all outside
  the MoE engine and the gateway refactored in this goal; they were not
  silenced.
- `apps/frontend`: `npm run build` passed, with Vite's warning about a chunk
  larger than 500 kB; `npm test` passed with 44 files and 278 tests.

The remaining warnings were kept in the run log; there was no Hugging Face
provider error in this validation and no file of that provider was changed. The
symlink was removed at the end and is not a versioned artifact.

### New prefill profile after the `max_tokens` fix

On 2026-08-25, after the GPU was freed, an isolated instance was run with the
binary built in `/data/ftk-cuda-target`, code from fix `72cc5cc` (promoted to
the published history as `e3d1751`), fork `llama.cpp=4bb64b070`,
`--features vulkan,cuda`, `n_gpu_layers=99`, `n_ctx=8192`, `n_batch=512`,
`POM_MOE_CUDA_PROFILE=1` and `--no-thinking`. The service on ports 8080/8092 was
neither restarted nor changed. The instance used ports 18082/18094,
`no-discovery` and the real Gemma4 NVFP4 GGUF. The request
`Escreva hello world 10 vezes.` ("Write hello world 10 times.") asked for
`max_tokens=30` and returned exactly 30 tokens, with ten coherent lines and no
template markers.

The real trace was: `input_tokens=22`, `output_tokens=30`,
`executed_engine=moe`, `moe_executor=cuda`, `engine_mismatch=false`,
`mixed_execution=false`, prefill `processing_ms=1351`, decode
`processing_ms=923` in 29 calls. That gives `61.409 ms/token` (`16.284 tok/s`)
in prefill and `31.828 ms/token` (`31.419 tok/s`) in decode. The decode p50 was
`28.886 ms` and the p95 `44.083 ms`. The instrumented prefill breakdown was:

| metric | raw value |
|---|---:|
| backend / `node_decode_ms` | `1351 ms` / `1350.694 ms` |
| `node_moe_forward_ms` / `runtime_forward_ms` | `1132.604 ms` / `1131.824 ms` |
| `cuda_call_wall_ms` | `158.385 ms` |
| `host_expert_load_ms` | `405.762 ms` |
| `host_batch_prepare_ms` | `0 ms` |
| `cuda_h2d_ms` / bytes | `1.390 ms` / `7,524,240` |
| `cuda_kernel_ms` | `145.731 ms` |
| `cuda_d2h_ms` / bytes | `1.511 ms` / `7,434,240` |
| expert calls | `5280` |

The 22-token prefill made one batched call of
`MoeRuntime::forward_batch_with_route`, not 22 gateway passes. The 5280 expert
calls are `22 tokens × 30 layers × 8 active experts` inside that pass; so the
`calls` metric counts expert records, not prefill passes. The current profile
does not show `host_batch_prepare_ms` as a bottleneck, unlike the historical
profile from before grouping; the new breakdown is the one that counts for the
next investigation.

The same run confirmed the route limit: the request for 30 ended at 30, not 31.
The fork did not change in this fix: `max_tokens` is a contract of the Rust
gateway, so there is no new API field to sync in `llama.cpp`.

### Rerun

With a local GGUF and an instance started from this worktree, repeat the run
without a remote cluster:

```bash
POM_HOME=/tmp/pom-ftk-local ./target/release/pom --gateway-port 8080 --no-discovery
```

Configure the node at `http://127.0.0.1:8080/` and deploy the local GGUF on a
single node through the interface, with `n_ctx` 8192 and `max_new_tokens` 128:
the binary accepts neither a model nor inference parameters on the command
line. Then send the conversation and the concurrency cells to the local gateway
and attach the number tables below this baseline. The runner must record
commit, system, CPU/GPU, memory, build feature, KV type, `n_ctx`,
`max_sessions`, `n_batch`, `max_new_tokens`, model and temperature.


## Safety invariants

- Missing metadata does not silently become zero. A KV budget can only be
  computed when the required geometry is present.
- A rebuild happens as a new generation: the previous server stops accepting
  connections, drains in-flight requests and only then frees the old context.
  A compatible reconfiguration keeps the weights in `ModelCache`; a slice or
  loading change clears that cache. A rebuild failure must not leave the new
  generation announced as ready.
- Cancellation is cooperative and idempotent. Closing the stream interrupts the
  in-flight gateway call and prevents the next chunk or hop; a frame already
  accepted by the peer may finish. Cleanup frees the session on every peer
  reached.
- The new engine is never selected by a name heuristic or by a `llama.cpp`
  failure; the GGUF architecture must declare MoE and the configuration can
  force the dense path while the new path is under validation.
- The expert LRU cache is local to the machine. It never crosses slicing and
  does not turn weights into network traffic.

### State of the MoE path - 2026-09-01

The split layout of the K-quant experts follows
`[hidden, intermediate, experts]`; the loader now keeps that order, including
when `hidden != intermediate`. The callback validates name, dtype, rank and
dimensions against the source and reports both geometries when they diverge.
The expert source stays outside llama.cpp's weight buffer: the measurement on
the CUDA test server found 2,531.86 MiB in that buffer for a model with 256
experts, while the routed payloads stay in the own cache.

The heterogeneous path batches the routes and covers IQ3_XXS (18) plus MXFP4
(39), the `deepseek4` combination, in the CUDA bridge and in the reference CPU
decoder. Route indices are read as I32 in token-major order, including values
above 127. A combination without an external decoder is refused before the
model loads, with the dtypes that blocked it; there is no silent native
fallback. There was no CUDA proof on the development machine yet, because it
has no CUDA compiler; the differential proof and the E2E on the real GGUFs had
to be run on the CUDA test server.

### Phase 5.7: resident cache and multi-GPU CUDA execution - 2026-09-15

The operator's diagnosis on the CUDA test server compared `moe/q4_0` and
`dense/llama.cpp` on the same node `5b035803`, with two RTX 5060 Ti. MoE
measured `765 ms` p50 and `952 ms` p95 per decode call, while dense measured
`52 ms` and `93 ms`. The count `36 GPU / 0 CPU / 0 mixed` did not prove that the
external executor used both cards: it only described the layers llama.cpp
allocated.

The root cause was confirmed in POM's own code, not in the fork
`vendor/llama-cpp-sys-2/llama.cpp`: `cuda_kernels.cu` hardcoded
`kMoeCudaDevice = 0`, and every allocation, upload, stream, event and release of
the executor used that device. Besides, `DeviceExpertCache` kept a single memory
account and a single allocator. The dense path, by contrast, passes the device
list and the `FitPlan`'s `tensor_split` to `llama_model_params` before
`llama_model_load_from_file`; the fork gets that configuration through the
native path, but the MoE executor did not.

The fix keeps the same topology seen by the dense context:

- `cuda_compute_bytes_by_device` groups the compute buffers by `CUDA<n>` and
  ignores `CUDA_Host`;
- `DeviceExpertCache::new_sharded` creates a budget, LRU and allocator per
  card. The stable hash of model/layer/expert keeps every role of the same
  expert on the same GPU, and the leases carry the ordinal to the CUDA bridge;
- the resident pipelines and every FFI entry get the explicit ordinal. The CUDA
  RAII selects the card for each call and restores the previous device;
- a route is grouped by GPU, each group runs in its own pipeline in parallel and
  the full vectors are summed on the host. So experts selected on different
  cards do not go back to CUDA0 nor share a single pipeline mutex.

The second run without improvement also had a state cause in the external path.
The aggregated budget was applied to a single card, so the LRU evicted resident
experts when the working set did not fit in CUDA0's capacity, even with free
VRAM on CUDA1. Meanwhile, the asynchronous warmup kept admitting experts of
future layers and could compete with the first route, causing misses, eviction
and calls that did not finish in the expected time. The budget is now per card
and the first real route calls `cancel_device_warmup`, keeping the entries
already admitted and letting the request's working set control the cache. Later
hits return the same handle and the same ordinal without a new upload.

The parser and cache tests cover the split by `CUDA0`/`CUDA1`, admission into
both shards and reuse of a key's device. In that worktree, `cargo test -p
llama-engine` passed with `306` tests and `11` ignored. There was no NVIDIA GPU
nor local `nvcc`, so the physical two-card proof, the decode measurement and the
final confirmation of the 15x drop had to be repeated by the operator on the
CUDA test server with the CUDA profile on. The native fork was not changed in
this fix.

### Phase 5.8: heterogeneous hot path aligned with FreeToken - 2026-09-15

The latest telemetry from the CUDA test server showed that the node's queue and
setup stayed below 1 ms, while the heterogeneous backend spent `978-1495 ms` in
prefill and `83-117 ms` per decode call. So the scheduler timeouts were not the
cause of that trace. The path also loaded bytes from the mmap for each role
before checking the resident cache; a hit threw that copy away. That
contradicted FreeToken's decision to keep the expert working set resident on
the device and explains why a second request did not get materially cheaper.

The production path now uses three explicit boundaries:

- `HeterogeneousTensorSpec` validates role, dtype, geometry and scales without
  copying the payload. `HeterogeneousMoeBlock` checks the `DeviceExpertCache`
  first and only calls `load_expert_tensor` on a real miss;
- `HeterogeneousRoutePipeline` keeps descriptors, hidden, ids, weights,
  offsets, activations, intermediate results and output allocated per device.
  The pipeline is shared by every layer of the runtime and indexed by
  `(CUDA<n>, hidden, intermediate)`, avoiding a large reservation per layer;
- experts stay partitioned by the stable hash of model, layer and expert. Each
  group per ordinal runs in its own pipeline inside
  `sum_parallel_device_outputs`, and groups on `CUDA0` and `CUDA1` can progress
  simultaneously. llama.cpp's layer count is not used as proof that both cards
  are used: the field confirmation must look at the resident bytes and the
  per-device counters.

The pipeline only copies the route metadata and the hidden state on each call.
The output's D2H `cudaMemcpy` is still the barrier needed to return the result
to the callback, but the extra `cudaDeviceSynchronize` was removed;
`sync_count=0` in that path's profile. `alloc_ms` only shows up when the
capacity grows, not on each token. The update is POM's own CUDA code in
`crates/llama-engine/src/moe/cuda_kernels.cu`; the llama.cpp fork stays
unchanged and only provides the callback and the metadata.

Warmup cancellation is also checked after a candidate is obtained. So a read
that finished after a real request arrived cannot admit bytes of future layers
and evict the active working set. The path still lets a single admission
already in progress finish, but does not start another one after cancellation.
The test `a_warmup_cancelled_after_getting_a_candidate_admits_no_payload` pins
that boundary.

This phase removes redundant copies, allocations and synchronizations, but does
not turn expert partitioning into tensor parallelism. If every expert chosen by
a token falls in the same shard, the other card stays idle on that route; the
dense llama.cpp path can use both for each matrix through `tensor_split`. The
measurement on the CUDA test server must separate that routing effect from the
kernel cost before claiming throughput higher than llama.cpp or FreeToken.

### Phase 5.9: effective IQ3_XXS path and cooperative kernel - 2026-09-15

The telemetry of the model `Mistral-Small-4-119B-2603-UD-IQ3_XXS-9c2b030b`
showed GGML dtypes `18`, `22` and `23` together. That means the model uses
`HeterogeneousCudaExecutor`, not the uniform `IqQuantCudaExecutor`. The previous
phase's heterogeneous optimization still left the dot product scalar:
`HeterogeneousRoutePipeline` assigned one row to a 128-thread block, and each
thread repeated the full loop over the columns. Besides, each thread of the IQ
kernel computed the full dot product of a row by itself, repeating the same work
inside the block. That was the local cost observed as `978-1495 ms` in prefill
and `83-117 ms` per decode, not a queue wait: the node telemetry recorded queue
and setup below 1 ms.

The uniform IQ path keeps sharing the same persistent workspace per
`(device, geometry)` used by the K-quants. The runtime creates a pipeline for
each `CUDA<n>` shard of the resident cache, groups the routes by ordinal and
runs the groups in parallel; no device is picked as a single `main_gpu` by the
executor. The generic IQs assign one row to a 128-thread block and reduce the
dot product fragments in shared memory. For the measured IQ3_XXS, each row uses
a single 32-thread warp: each lane processes the eight values of a
`(group, il)` slice and the sum uses shuffle. Gate and up are computed in the
same warp, reusing the hidden state and halving that stage's blocks. That cuts
the row's coordination work from four warps to one and removes the shared memory
tree, keeping the sequence gate/up, SiLU, down and aggregation. The synchronous
D2H is still the barrier needed to return the result to the callback, but the
extra `cudaDeviceSynchronize` was removed and the profile publishes
`sync_count=0`. IQ3_XXS is now eligible for the CUDA bridge and validates the
payload geometry before launch.

In IQ3_XXS, the 32 lanes cover the superblock's 256 elements in groups of
eight. `aux32`, the two codebook indices and the signs are read once per slice,
instead of rebuilding that metadata for each pair of columns. The other IQs keep
the generic cooperative decoder until they have the same differential layout
proof.

The cache does not copy the cold source on hits: admission is still by hash of
model, layer and expert in independent shards. So a second call reuses the same
handle on CUDA0 or CUDA1. Warmup is cancelled before each `llama_decode` that
has an MoE callback, and again on the first route as a defense; that keeps a
concurrent admission from evicting the working set or holding the shard while
the graph is already running. If the per-device counters still show a single
card in use, the next diagnosis is the effective distribution of the selected
experts, not a timeout.

With `POM_MOE_CUDA_PROFILE=1`, `moe_executor_diagnostics.cuda_device_calls` now
records per ordinal the number of calls, experts processed and kernel/wall time.
That field must be compared with `device_cache_resident_bytes_by_device`: bytes
on a card with no calls only show resident capacity, while calls on `CUDA0` and
`CUDA1` confirm effective participation in the route.

This fix is POM's own CUDA code in `cuda_kernels.cu`; it does not change the
llama.cpp fork. The fork keeps providing the external callback and the memory
metadata. The comparison against `dense/llama.cpp` and against FreeToken must be
repeated on the CUDA test server with the new CUDA build: the development
worktree has neither `nvcc` nor a GPU, so there is no local throughput claim.

### Phase 5.11: q8 MMVQ in the heterogeneous executor too - 2026-09-15

The telemetry JSON closed the cause of the five-second gap. In the 45-token
prefill, `node_moe_forward_ms=4706.821` inside `node_backend_call_ms=4994.464`;
`node_queue_ms=0.038`, `node_lock_wait_ms=0`, `node_scheduler_wait_ms=0.012`,
network `0 ms` and the D2H copy `0.394 ms`. So there was no five-second timeout
in the pipeline. The `node_moe_forward_ms` field is the synchronous external
callback that llama.cpp waits for before returning the prefill.

The same JSON shows the cost that dominated that callback: from the end of
prefill to the end of decode there were another `2406` misses/admissions,
`2399` evictions and `10.5 GB` of uploads to the devices. The resident cache
held about `8.25 GB` on CUDA0 and `8.17 GB` on CUDA1, and warmup had already
admitted `3766` experts before reaching the budget. That is working set
thrashing, not a scheduler queue. Since the leases are released at the end of
each layer and decode visits new routes, a new request can evict the experts
used in prefill; that is why repeating the prompt does not guarantee a hit.
`warmup_status` was `budget-exhausted`, not `timed-out`, and warmup is
asynchronous, so its `4726 ms` are not a blocking wait in decode.

The heterogeneous pipeline now quantizes hidden state and activation into
`block_q8_1` using persistent buffers, calls the fork's canonical
`vec_dot_*_q8_1` for Q4/K/IQ and uses one 32-lane warp per row. That removes the
four redundant warps of the scalar block and uses dp4a also when gate/up/down
have different dtypes. The launch goes from four to six kernels (two
quantizations, gate/up, activation, down and aggregation), without
`cudaDeviceSynchronize`; the D2H copy is still the completion barrier.

To confirm the rest on the CUDA test server, just start the MoE executor. The
diagnostics are automatic and must show `cuda_device_calls` for CUDA0 and CUDA1
and separate `kernel_ms` from `cuda_call_wall_ms`; the cumulative cache counters
must be compared by delta between prefill and decode. The q8 optimization cuts
the callback's compute, but cannot fit in 16 GB an expert working set larger
than VRAM: if misses stay high, the next adjustment is per-layer
affinity/prefetch, not a larger timeout.

### Phase 5.12: parallel heterogeneous admissions per GPU - 2026-09-15

The heterogeneous hot path now makes two passes over the route. The first
checks the cache and groups each miss by the same affinity hash that picks the
shard. One thread per `CUDA<n>` reads the mmap slices and uploads them serially
on that GPU; threads of different shards work in parallel. The second pass
builds the descriptors only after every admission, keeping the leases alive
during the forward to prevent premature eviction.

Each upload uses the mmap slice directly. The CUDA path no longer creates an
intermediate `Vec<u8>` for each tensor, removing a copy of tens of GB when
preparing a cold route.

The specs of the experts of the same layer are also built in a batch, in a
single walk of the inventory. That avoids repeating the lookup of entries and
geometry for each selected expert during prefill.

That aligns admission with the grouping that already existed in the kernel: the
cache no longer picks a single global queue for tens of GB of H2D. The change is
only in the Rust crate and uses the existing CUDA allocator, which selects the
ordinal explicitly on each upload; the llama.cpp fork was not changed. The wall
gain and the effective occupancy of each card still have to be measured on the
CUDA test server. With `POM_MOE_CUDA_PROFILE=1`, compare
`device_cache_upload_ms` by delta with `cuda_device_calls` on `CUDA0` and
`CUDA1`, always with `POM_MOE_WARMUP` unset or disabled to measure the cold
route without speculative pollution.

Besides, the callback now uses the non-blocking variant of
`cancel_device_warmup`: it signals the `AtomicBool` and starts the forward
immediately. The current admission stays protected by the shard mutex and by
leases, and the thread checks cancellation before each next candidate. The
variant that does `join` is only used when replacing a warmup, avoiding turning
a weight copy into TTFT without allowing two simultaneous cache sweeps.

### Phase 5.16: non-blocking warmup cancellation - 2026-09-16

FreeToken's design treats warmup as background work that prepares the working
set without blocking the scheduler. The POM runtime had the same thread, but
`begin_moe_callback` called `cancel_device_warmup`, which signaled and did a
`join`. If a weight admission was in progress, the first callback stalled until
it finished, even though queue, context mutex, transport and scheduler were
free.

The callback and the entry of `forward_batch_with_route` now call
`cancel_device_warmup_for_forward`, which only publishes the cancellation flag.
The admission already started finishes with the shard's normal protection;
active leases keep its handles from being evicted, and the CUDA stream keeps the
order of operations. The swap API still uses `cancel_device_warmup` with `join`,
so a new warmup never competes with the previous one.

This change only acts on the TTFT wait during the warmup-to-forward transition;
it does not raise the budget nor pretend the whole model fits in VRAM. The
mandatory validation on the CUDA test server must compare three rounds of the
same prompt with `nvidia-smi` showing no competing processes, checking
`ttft_ms`, `node_moe_forward_ms`, `generation_tokens_per_sec`, hits/misses and
the text output against the reference `0c088521`. The gain is not claimed before
that measurement.

### Isolated comparison on one machine - 2026-09-15

A real run on node `5b035803`, with two RTX 5060 Ti and no pipeline hop,
confirmed that transport is not the bottleneck. MoE used 46 prefill tokens and
generated 188 tokens: `5667.7 ms` of prefill (`8.1 tok/s`) and `35009.3 ms` of
decode in 95 calls (`5.3 tok/s`), with p50/p95 of `347.2/617.1 ms` per call.
Network was `0-16 ms`, queue and setup stayed below `2 ms`, and `99%` of the time
was in local `llama_decode`.

The `dense/llama.cpp` control on the same node generated 178 tokens at
`18.5 tok/s`, with `749.7 ms` of prefill (`29.2 tok/s`) for 22 tokens and
p50/p95 of `125.4/129.8 ms` per decode call. The comparison does not have an
identical prompt (`46` against `22` tokens), so it is order-of-magnitude
evidence, not a final paired measurement. Even so, MoE was about `3.6x` slower
per token in decode and `3.6x` slower in normalized prefill, despite announcing
`36 GPU / 0 CPU / 0 mixed`; that counter describes llama.cpp layers and does not
prove use of CUDA0 and CUDA1 by the external executor.

The run did not have `POM_MOE_CUDA_PROFILE=1` yet, so it cannot tell which
ordinal received the routes nor separate kernel, H2D, D2H and cache misses. For
the next run on the CUDA test server, the profile must be on from startup and
the telemetry must include `cuda_device_calls` and
`device_cache_resident_bytes_by_device`. The absence of transport and scheduler
wait already rules out timeouts as the main hypothesis; the remaining hypothesis
is the cost of the per-layer callback, of host/device copies and of the custom
IQ3_XXS decoder, to be compared with the optimized CUDA calls of the dense path.


### Phase 5.10: IQ3_XXS path aligned with CUDA MMVQ - 2026-09-15

The previous cooperative decoder still did the codebook arithmetic in float.
That diverged from the fork's MMVQ path, which first quantizes the activation to
`block_q8_1` and uses `vec_dot_iq3_xxs_q8_1` with `dp4a`. The IQ3_XXS pipeline
now keeps persistent q8 buffers for hidden state and activation, quantizes each
row in one warp and uses the canonical decoder of the fork's active checkout.
Gate and up use the same warp and down reuses the q8 activation. `build.rs`
includes `ggml/src/ggml-cuda/vecdotq.cuh` from the same `LLAMA_SOURCE` used by
the binding; no fork file was changed.

The lane mapping follows the MMVQ formula for IQ3_XXS: eight q8 chunks per
superblock, four superblocks covered in parallel per warp, and each lane
advances four superblocks at a time. That avoids leaving 24 lanes idle in the
dot product. The geometry checks and the persistent capacity still prevent
overflow or reallocation on the stable path.

Two later measurements on the same two-RTX-5060-Ti machine confirmed the
bottleneck profile before this optimization. For 16 prompt tokens and 15 output
tokens, prefill took `5323 ms` and decode had a p50/p95 of `254.5/382.0 ms`. For
45 prompt tokens and 91 output tokens, prefill took `4998 ms` and decode had a
p50/p95 of `277.1/470.7 ms`. Network, queue and engine mutex wait stayed around
`0-4 ms`, and setup dropped from `152 ms` on the first load to under `1 ms` on
the next. The almost fixed prefill near five seconds, even with prompts of
different sizes, is compute work of the 36 callbacks, not a timeout or a
transport wait. The q8/dp4a path attacks exactly that work, but the throughput
and numeric correctness confirmation had to be done on the CUDA test server with
`POM_MOE_CUDA_PROFILE=1`.

### Phase 5.13: CUDA batch for admissions and repeated routes - 2026-09-15

The latest JSON from the CUDA test server does not show a hidden five-second
timeout. On the same node, `node_lock_wait_ms` was `0`, queue and scheduler
added up to a few milliseconds and transport added up to `54 ms`. The work is in
`node_moe_forward_ms` (`6445.8 ms` in prefill) and in the decode calls, not in a
pipeline `wait`. The delay observed on the first token follows the local
prefill of `6656.5 ms`.

The cache diagnosis identifies the operational cause: in 320 decode calls there
were `94522` misses/admissions, `90762` evictions and about `412 GB` of
cumulative uploads. The shards were balanced on `CUDA0` and `CUDA1`
(`8242/8165 MB` resident), so both cards were used for residence, but the
working set exceeded the budget and was reloaded. `runtime_forward_ms` and
`device_cache_upload_ms` are cumulative counters of callbacks and devices; they
are not a single five-second wait.

The hot path now groups misses by shard and makes a single budget check per GPU.
`DeviceAllocator` gained `upload_many`; the CUDA allocator queues every
allocation and copy on the default stream and synchronizes only once per batch.
Evictions use `cudaFreeAsync`, keeping the stream order and avoiding
`cudaFree`'s global synchronization on each tensor. The copy's synchronization
is still needed because the bytes come from an `mmap` slice that can be released
as soon as admission returns. The llama.cpp fork was not changed.

In the same route, the executor only deduplicates pairs with the same expert
descriptor and the same token. The kernel computes the FFN once and the
aggregation applies each weight again through the route map, keeping the
router's semantics. That cuts work when top-k repeats an expert for a token
without changing the sum of distinct routes.

Telemetry `d5eeeaa` was still produced before `fef71ae`, which fixed the
shared scale of `block_q8_1` in the quantization kernel. The nonsensical output
of that run is not a valid measurement of the model's quality; the CUDA test
server must repeat the same prompt with the SHA that contains that fix and this
batch optimization. The development environment has neither `nvcc` nor a GPU to
measure the final prefill/decode reduction locally.

### Phase 5.14: default warmup and measured heterogeneous co-execution - 2026-09-15

The CUDA test server's logs confirmed a configuration regression: the process
that produced the telemetry had `POM_MOE_WARMUP` unset and, because of that,
warmup was `disabled`. The behavior now goes back to pre-warming by default.
`POM_MOE_WARMUP=0`, `false`, `off` or `no` stays available for explicitly cold
runs. Warmup stays limited by each GPU's independent budget; on a 5060 Ti it can
end with `budget-exhausted` before loading every expert, because the whole model
does not fit in VRAM. To make better use of that budget, the warmup enumeration
is expert-major: it alternates the layers for each expert before moving to the
next expert. So when VRAM cannot hold the whole model, every layer gets a
resident working set instead of only the model's prefix.

The roughly five-second gap was not a timeout: in the paired trace,
`node_lock_wait_ms`, queue, scheduler and transport stayed close to zero, while
`node_moe_forward_ms` took practically the whole prefill. The heterogeneous
executor now accepts the same measured `BandwidthProfile` as NVFP4. Experts with
every tensor resident stay on the GPU; misses are split per expert between CPU
and GPU routes, run in parallel and summed with the same output width. The
residence check creates no lease and does not touch the LRU, so the planner does
not fake hits or block eviction.

Diagnostics are automatic when the MoE CUDA executor is active;
`POM_MOE_CUDA_PROFILE` is not needed and does not change execution. The sidecar
`<model>.cuda-profile.json` with the `bandwidth` field is still required to
enable the measured split between the CPU and GPU lanes. Without that file, the
runtime keeps the CUDA-only policy and does not make up a rate for the scalar
Rust CPU. On the CUDA test server, validation must repeat the same prompt with
the default warmup, persist the measured sidecar for this model and compare
prefill, decode, text correctness and upload deltas.

### Phase 5.15: pinned host staging on the heterogeneous output - 2026-09-16

The CUDA test server's telemetry after the q8 pipeline isolated the cost that
still dominated the callback. In prefill, the CUDA kernel added up to about
`177 ms`, but D2H transfers added up to `6317 ms`; in decode, about `680 ms` of
kernel against `12801 ms` of D2H. The correct result still had to be written to
the callback's host tensor, so the external contract did not allow simply
leaving the output in VRAM. The observed cost came from the synchronous
`cudaMemcpy` straight into the pageable memory that llama.cpp hands to the
callback.

Each `HeterogeneousRoutePipeline` now reserves a persistent `cudaMallocHost`
with the same capacity as the device output. The D2H copy goes to that pinned
staging and, after the completion barrier, a short `memcpy` hands the result to
the original host buffer. The staging is freed together with the pipeline's
persistent buffers and only grows when the batch geometry changes. That removes
the CUDA runtime's repeated implicit staging without changing the callback's
semantics, the route aggregation or llama.cpp's graph capture.

The gain has to be confirmed on the CUDA test server by comparing
`cuda_d2h_ms`, `cuda_call_wall_ms`, `node_moe_forward_ms` and the text
correctness with the same prompt. The goal of beating the dense path by 1.5x is
not proven yet.

### Phase 5.17: direct heterogeneous q8 dispatch by dtype - 2026-09-16

The heterogeneous q8 kernel received a CUDA function pointer chosen by the dtype
and called that pointer inside the block loop of each row. That kept the result,
but prevented the compiler from inlining the fork's canonical `vec_dot_*_q8_1`
primitives. For the Mistral-Small route, which mixes dtypes 18, 22 and 23, the
dispatch now selects a helper compiled per dtype; each helper calls the matching
primitive directly. The `block_q8_1` layout, the quantization, the warp
reduction and the number of launches did not change.

A source test keeps the function pointer alias from coming back in the q8 path.
In deploy `ea58bfab`, the CUDA build on the CUDA test server passed in `4m52s`;
three rounds with `nvidia-smi` showing no competing process measured `10.6127`,
`11.8136` and `10.9890 tok/s`, median `10.9890 tok/s`. That is `1.073x` the
reference `b4df3c22` (`10.2416 tok/s`) and `1.125x` the earlier reference
`0c088521` (`9.770 tok/s`). The correctness round returned the expected numbered
list, with no corrupted text. The result is an observed improvement, but still
below the goal of `1.5x` over dense.

### Phase 5.18: working set and CUDA budget telemetry - 2026-09-16

The `DeviceExpertCache` policy keeps active leases protected: `Drop` only lowers
`active`, and an entry is only removed when the LRU finds an idle entry
(`active == 0`). So the thousands of evictions observed in a run do not show
eviction of experts in use. They can be idle residence churned by a working set
larger than the effective limit, but before there was no per-layer measurement
to separate the hypotheses.

The heterogeneous executor's diagnostics now accumulate, per layer, batches,
tokens, route occurrences, distinct experts in the batch and the sum/max of
distinct experts per token. They also publish the current effective capacity
per GPU, computed with the same formula as `MemoryAccount` and a free memory
read from the CUDA allocator. The capacity is a diagnostic snapshot and does not
change the admission policy.

This instrumentation must be used before another kernel or dispatch tweak:
compare `cache_working_set_by_layer` with
`device_cache_capacity_bytes_by_device`, plus misses, evictions and uploads. If
the capacity is right and the working set repeatedly exceeds the limit, the
problem is legitimate pressure from the expert set; if there is capacity
available despite the churn, investigate the memory accounting and the
admission granularity. `device_cache_budget_by_device` exposes the full account
per shard: `baseline_free_bytes`, `current_free_bytes`, `compute_bytes`,
`reserve_bytes`, the intermediate limits and the resident entries.
`device_cache_entries_by_device` lets you convert capacity into real entries,
without confusing the generic `cache_entries` field of the host cache. The
instrumentation's throughput was measured on the CUDA test server at
`e8709f09`: three rounds with no competing process had `11.018`, `10.440` and
`10.351 tok/s`; the effective capacity ranged from `15.337` to `17.803 GB` and
residence ended up pressed against that limit. Phase 5.19 used that breakdown to
confirm the recoverable margin of the compute buffers and guide the approved
exception for the main MoE context.

### Phase 5.19: physical micro-batch exception for the MoE context - 2026-09-16

The raw probes on the CUDA test server showed that `MemoryAccount` does not
underestimate VRAM: after the context buffers, each RTX 5060 Ti had about
`0.5 GB` free, while the compute buffer took about `3.1 GB`. The resident cache
ended up pressed against `15.3-17.8 GB` in aggregate, below the `19.62 GB` of
`4499` warmed candidates. The set does not fit with `n_ubatch=512` and there is
no evidence of another GPU load.

The exception keeps `n_batch=512` as the public batch, but limits `n_ubatch` to
`128` only in the main context when `moe_external_executor` is active.
llama.cpp's graph then reserves smaller physical workspaces, giving VRAM back to
the expert cache without changing the dense context. In three rounds on the CUDA
test server, with only the `pom` process visible in `nvidia-smi` on both cards,
the decode of the same prompt measured `16.001`, `15.968` and `13.631 tok/s`,
median `15.968 tok/s`. The previous reference of the same test measured
`10.560 tok/s`, a gain of `1.512x` over the earlier state of the MoE executor.

`FIT_MAX_UBATCH=512` stays the deliberate operational preference for the dense
path and for the MTP context: `n_ubatch` affects prompt processing and the MTP
context's buffer. The operator approved a restricted exception because the main
context with `moe_external_executor` has to give VRAM back to the resident
cache. That context uses 128, while dense and MTP stay at 512.

The isolation investigation found two boundaries. The dense path stays at 512
because it uses the common policy, while a separate explicit policy makes only
the main context that uses the MoE executor use 128. MTP stays at 512 by calling
its own policy when creating the `MTP` context. However, it is not possible to
reduce only the cache's consumption from the Rust callback:
`llama_context_params` has a single `n_ubatch`, the scheduler uses that value to
split the whole graph, and the callback only receives the already split
micro-batch. So the MoE-only alternative also reduces the micro-batch of
attention, KV and the other operations of the target context, besides freeing
VRAM for experts. The trade-off of also reducing the attention and KV
micro-batch of the MoE context was accepted in this exception. The
implementation keeps that split in `effective_n_ubatch` and
`apply_mtp_context_batch_params`; there is no new kernel or dispatch tweak in
this phase.

The diagnostics confirm the cause: aggregate capacity rose to
`20.925-23.500 GB`, against `15.337-17.803 GB` before, and residence ended up
pressed against the limit in every round. The snapshots showed about
`12.82/12.68 GB` free at baseline per card, compute of `1.02/1.00 GB`, reserve
of `0.26/0.25 GB` and only `0.27 GB` free after residence. There is no evidence
of free VRAM omitted by `MemoryAccount`.

The observed working set had exactly four route occurrences per token in each of
the 36 layers. In the 832-token round, the sum of distinct experts per batch
ranged from `2228` to `2810` across layers. The cache telemetry still
accumulated `29,846` misses and `24,876` evictions, so the working set pressure
was not removed, only reduced. Since active leases are never LRU victims, those
counters represent churn of idle entries that do not fit in the effective
capacity.

The partitioning across GPUs is already expert-parallel: `CUDA0` and `CUDA1`
have independent LRU, budget and allocator, and each key's hash picks a single
shard. In the best-capacity round, there were `2719` and `2660` entries, with
`11.859` and `11.603 GB` resident on the two cards. There is no second copy of
the same cache to be replaced by expert parallelism. The goal of beating dense
by `1.5x` was not reached yet; this result is an MoE capacity improvement and
must stay as the reference before any new change.

### NCCL investigation in tensor parallelism - 2026-09-16

The CUDA test server confirmed two RTX 5060 Ti and the NCCL install through the
system loader:

```text
libnccl.so.2 => /lib/x86_64-linux-gnu/libnccl.so.2
libnccl.so   => /lib/x86_64-linux-gnu/libnccl.so
```

The initial diagnosis was reproduced in the build:
`vendor/llama-cpp-sys-2/build.rs` passed the `GGML_*` variables on to CMake, but
then set `GGML_CUDA_NCCL=OFF` unconditionally. After allowing the opt-in, CMake
recorded `GGML_CUDA_NCCL=ON`, `NCCL_INCLUDE_DIR=/usr/include` and
`NCCL_LIBRARY=/usr/lib/x86_64-linux-gnu/libnccl.so`. The first link revealed the
second boundary: the static archives produced by CMake did not propagate
`NCCL::NCCL` to the Rust binary's final link. The build script now adds the
resolved directory and `dylib=nccl` only when the opt-in is on, keeping the
default build without a new dependency.

The resulting isolated binary was
`112aeef362fbc44d0b66fbf799822ed609a2dd6b3a007f853a137bab4b426a45`. The link
proof was:

```text
libnccl.so.2 => /lib/x86_64-linux-gnu/libnccl.so.2
```

An E2E smoke test was run outside the production service, with the same GGUF
`Qwen3.6-35B-A3B-UD-IQ3_XXS.gguf`, both GPUs, the short prompt
`Oi, teste. Responda em uma frase curta.` ("Hi, test. Answer in one short
sentence.") and `max_tokens=32`. With an isolated process without a TP
identity, the `dense/llama.cpp` path measured `29.674`, `60.606` and
`53.030 tok/s` in three calls, median `53.030 tok/s`. That round confirms that
the executable loads the model on both cards, but it is not an NCCL
measurement: `tp` was not configured and the observed split is by layers, not
the tensor parallelism rank group. The variation includes the process warmup and
must not be compared directly with the deployment's historical control.

The attempt to select `POM_INFERENCE_MODE=moe` in the same direct startup is not
a valid measurement: the command line path hardcodes `Dense` and the request
keeps recording `dense/llama.cpp`. When preparing an isolated MoE journal, the
executor reached both GPUs, but the call ended in
`kernel CUDA MoE falhou (código 400)` ("MoE CUDA kernel failed (code 400)"),
with no tokens and no usable tok/s. That does not attribute the failure to
NCCL. The own MoE executor does not use llama.cpp's collective, and the
post-build MoE benchmark still depends on a valid MoE deployment.

As a comparable reference already recorded on the CUDA test server, the best
confirmed MoE was `15.968 tok/s` at SHA `f4b7eb4`, while the dense control was
`24.97 tok/s`. Those numbers are pre-NCCL. The post-NCCL comparison on the
production deployment was not run in this round, because swapping the served
binary and restarting the processes on that server requires explicit
operational authorization. So there is no evidence of a real throughput
improvement yet. The available technical conclusion is only: the host finds
NCCL, the new binary is linked with `libnccl.so.2`, and the own MoE path does
not benefit directly from that collective.

## Defects noted

- Throughput and prefill/decode split measurements are still not possible in a
  local environment without getting a model and starting an own runtime.
- The filtered CUDA battery still has a historical NVFP4 numeric failure, not
  introduced by warmup. At `ef36059` and at `c292403`, the isolated test
  `cuda_nvfp4_executor_route_batch_aggregates_tokens_on_the_device` reproduced
  `13.464294 != 164.57135`. The IQ proof of `86226c7` did not reproduce the old
  IQ differences; the three dtypes passed the differential proof and must not
  be protected by a wider tolerance.

