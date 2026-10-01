# Handoff report: NaN in the heterogeneous MoE of Ornith-1.5-35B

Handoff date: 2026-08-27

## 1. Symptom and reproduction

The failing model is Ornith-1.5-35B, identified as the `qwen35moe`
architecture, in a heterogeneous Q4_K + Q6_K GGUF. The observed slice serves
layers 0-19 and captures `l_out-19`. The run's inventory had
`expert_count=256`, `expert_used_count=8` and
`expert_feed_forward_length=512`. The GGUF metadata reports recurrent layers
in `qwen35moe.attention.recurrent_layers = [1,1,1,0,...]`, with a full
attention interval of 4. Layer 1, where the failure shows up, is recurrent.

With the MoE engine path selected in the deployment and the process started
with `POM_MOE_ROUTE_DIAGNOSTICS=1`, inference fails with (log message as
emitted, in Portuguese):

```text
hook MoE do llama.cpp falhou: tensor ffn_moe_weights da arquitetura qwen35moe,
camada 1, token 0, slot 0, expert 0 tem valor não finito (NaN)
```

("llama.cpp MoE hook failed: tensor ffn_moe_weights of architecture
qwen35moe, layer 1, token 0, slot 0, expert 0 has a non-finite value (NaN)".)

The error was observed repeatedly at the first position: layer 1, token 0,
slot 0, expert 0. The message can only be produced by `normalize_moe_route`, in
`crates/node/src/inference/llm.rs:628-670`, specifically by the finiteness
check of `weights` at 658-663. That function is only called by the MoE
callback.

There is an important caveat about the supposed determinism: later evidence in
`/tmp/moe-evidencia.txt` showed a run that answered correctly after a reload,
but that instance did not log any warm-up of the resident MoE cache and had no
external executor. So that success does not prove the MoE; it only proves the
native path worked in that instance.

In the CUDA test server evidence, the running binary was built from `686f839`, was the
same inode deployed at 08:33, the process had `POM_MOE_ROUTE_DIAGNOSTICS=1`,
`RUST_LOG=info`, had no child process, and the callback really ran, because
the `normalize_moe_route` message appeared. Even so, the files
`/tmp/pom-node-1.log` and `/tmp/pom-node-2.log` had zero occurrences
of `POM_MOE_ROUTE_DIAGNOSTICS`.

## 2. Bridge fixes already attempted

The four runtime attempts were made without the CUDA differential proof
passing. What they share is that the NaN persisted at the first position, not
proof that each hypothesis below is false.

1. `899f503`, `fix: permitir K-quants heterogeneos no executor CUDA` (allow
   heterogeneous K-quants in the CUDA executor). Turned on the per-tensor
   decision and let the external executor receive heterogeneous K-quants. The
   hypothesis was that picking the executor by a global dtype was leaving the
   Q4_K/Q6_K combination off the correct path. The NaN remained. This rules
   the change out as a sufficient fix, but does not rule out payload, scale or
   inventory errors after dispatch.

2. `f3cb1bb`, `fix: corrigir rota e reativar CUDA MoE heterogeneo` (fix the
   route and re-enable heterogeneous CUDA MoE). Changed the transposition of
   the llama.cpp route to the layout POM expects and re-enabled heterogeneous
   CUDA publishing. The hypothesis was that expert-major versus token-major
   ordering was corrupting ids and weights. The NaN remained byte for byte at
   the same first element. This makes the transposition alone insufficient;
   it does not yet prove that all geometry and memory are correct.

3. `74de697`, `fix: materializar rota externa do MoE antes do callback`
   (materialize the external MoE route before the callback). Materialized the
   external route with `ggml_cont` before the reshape/callback. The hypothesis
   was that checking `ggml_is_contiguous` alone did not guarantee the producer
   had finished filling the area for the cross-backend copy. The NaN remained.
   Materialization alone did not solve it, and the origin of the area read has
   not been observed yet.

4. `8fdd064`, `fix: carregar sidecars no executor MoE heterogeneo` (load
   sidecars in the heterogeneous MoE executor). Changed the K and
   heterogeneous loaders to look for `.scale` and `.input_scale`, carried the
   scales in the payload/descriptor and applied them in the expert stages. It
   also let the inventory accept `ffn_gate_up_exps.weight` alongside the
   separate tensors, preferring the fused one instead of counting the
   duplicates as another route. The hypothesis was exactly the difference
   between the working Gemma NVFP4 and the failing heterogeneous Ornith:
   missing scales and an incompatible inventory. The NaN remained in the
   tested run. Without a diagnostic line from the callback, this does not
   prove that the sidecar was read, applied or tied to the right tensor.

`f02a150` was not a fifth numeric attempt. It was the ABI build fix after the
sidecars grew the descriptor. In `crates/node/src/inference/moe/cuda.rs:271-292`
Rust declares `data`, `bytes`, `dtype`, `scale`, `input_scale`,
`secondary_scale` and `secondary_input_scale`, at offsets 0, 8, 16, 20, 24, 28
and 32 with size 40. `cuda_kernels.cu:49-81` repeats the same order and keeps
the offset and size assertions. The CUDA build of `8fdd064` failed at 32
bytes; `f02a150` fixed that, but there is still no numeric proof for Ornith.

`686f839` handles DeepSeek4 head/residual aliases (`l_last-N` versus
`l_out-N`) and is not a fix of the MoE payload. It explains the binary seen on
the CUDA test server, but must not be counted as validation of the external executor.

## 3. Instrumentation iterations

1. `b403859`, `feat: instrumentar origem da rota MoE` (instrument the MoE
   route origin), added detailed collection of ids, weights, counts, bytes,
   dtypes and samples of token zero. The computation happened after pointer
   and geometry validation. On the four paths that failed first, the callback
   returned before collection. There was no evidence of raw input.

2. `046a42b`, `fix: tornar diagnostico da rota MoE observavel` (make the MoE
   route diagnostic observable), added `log_moe_callback_entry` and removed the
   filter that required a specific layer for the first sample. It also kept
   the rule of sampling the first call and non-finite routes. Still, the
   detailed sample depended on getting past the validations; the entry did not
   identify a return that aborted before it.

3. `b182fa5`, `fix: registrar saídas do callback MoE` (log MoE callback
   exits), put `callback_entry` at the start of the callback and added
   `callback_exit` for each `return false`, with reasons such as
   `null_callback_argument`, `invalid_expert_count_used`,
   `hidden_geometry_overflow`, `route_geometry_overflow`,
   `route_normalization_failed`, metadata failures and `forward_failed`. The
   entry now logs only raw pointers and counts, without dereferencing the
   descriptor. The unit test pinning that format is at `llm.rs:735-761` and
   passed.

The remaining problem is no longer a missing path in the callback. The CUDA test server
evidence showed zero lines even with the later `normalize_moe_route` message,
and confirmed the callback was entered. So the step to take on now is proving
why the `tracing::warn!` event does not reach the observed destination. There
is no executable proof of that chain yet; no other numeric fix should be
started before it.

## 4. Current state on the shared branch

At inspection time, `origin/feat/implantar` was at `62b5f8e`, on top of
`686f839` and `b182fa5`. The relevant code is:

- `llm.rs:120-128`: `POM_MOE_ROUTE_DIAGNOSTICS` is read through a `OnceLock`
  and only the values `1`, `true`, `yes` and `on` enable the diagnostic.
- `llm.rs:191-207`: `callback_entry` emits the raw arguments. The call comes
  right after reading the flag in `llm.rs:272-284`, before the null check,
  without reading the descriptor.
- `llm.rs:286-304`: the exit macro logs the same six pointers and the
  `n_tokens`/`hidden_width` counts before returning `false`.
- `llm.rs:306-499`: every error return of the callback is tied to a stable
  reason. A successful forward still has no exit event.
- `llm.rs:628-670`: `normalize_moe_route` validates the size, transposes from
  expert-major using `source = slot * n_tokens + token` and refuses non-finite
  weights. This function was not changed in this handoff.
- `format_heterogeneous.rs:255-315`: if there is a fused tensor, it determines
  the geometry and the separate tensors are not treated as a second route; the
  loader accepts both coexisting.
- `format_heterogeneous.rs:471-493`: the payload carries the primary,
  secondary and input scales for gate/up/down.
- `cuda.rs:271-292` and `cuda_kernels.cu:49-81`: the Rust/CUDA descriptor has
  the seven fields in the same order with field-by-field assertions.
- `block.rs:1100-1149`: the block loads the payload, allocates/reclaims the
  resident expert and fills the descriptor with the four scales.

The only change made by this worker in this step was `b182fa5`, plus the
matching documentation and this report. No CUDA kernel, loader, descriptor or
normalization was changed in this handoff.

## 5. Solid contrasting evidence

- Gemma4-26b-NVFP4 works on the MoE engine on the CUDA test server. Ornith fails. The Gemma
  case uses NVFP4, while Ornith uses heterogeneous Q4_K + Q6_K. This is a
  factual format difference, but it does not identify the failure by itself.
- The NVFP4 path explicitly reads sidecars in `format_nvfp4.rs:530-535` and
  applies the input scale in the decoder in `format_nvfp4.rs:569-587`. Before
  `8fdd064`, the K/heterogeneous paths did not load those sidecars into the
  payload and the descriptor had no `input_scale`.
- The Ornith GGUF contains fused tensors `blk.N.ffn_gate_up_exps.weight` and
  also separate `ffn_gate_exps`/`ffn_up_exps` tensors, plus `.scale` and
  `.input_scale` companions. The coexistence was confirmed in the load log,
  not just inferred from code.
- The GGUF declares `[1,1,1,0,...]` for recurrent layers and full interval 4;
  layer 1 of the error is recurrent. The load log confirmed this filtering in
  the KV cache.
- Loading logged `resolve_fused_ops` resolutions for DeepSeek V4 HC pre, comb
  and post. This confirms fusions are enabled in the fork, but does not prove
  that the callback's specific operation got a different layout.
- The logging evidence is solid: `RUST_LOG=info` should accept `warn`, the
  normalization message proves the callback was entered, there was no child,
  and the files examined did not contain the lines. The destination examined
  may, however, not be the subscriber's effective destination.

## 6. Open hypotheses and falsification

### 1. The observed destination does not receive the subscriber

This is the strongest hypothesis for Fact 1. `main.rs:341-346` installs a
`tracing_subscriber::fmt` with `EnvFilter` and `.with_target(false)`, while
`runtime.rs:1169-1195` writes the server file through `append_log`, which only
receives explicit supervisor messages. No code connects `tracing` events to
that file. The default writer of `fmt` must be checked in the deployed
process. Also, `respawn.rs:40-44` creates the new process with stdout and
stderr set to `Stdio::null()`, which can erase output after a restart.

Falsification: run a minimal proof in the same binary that installs the real
subscriber, fire a `warn!` with target `moe.route_diagnostic`, and check
stderr, stdout, the `infer_server_<port>.log` file and the descriptors
`/proc/<pid>/fd/1` and `/proc/<pid>/fd/2` separately. If the event shows up in
captured stderr/stdout but not in the file, the cause is the log destination.

### 2. The flag is in the environment but does not open the gate

`llm.rs:120-128` uses a `OnceLock` and compares the accepted values exactly.
The evidence said the variable was present, but did not record its value. An
earlier initialization with a missing value, or a value other than
`1/true/yes/on`, would make the callback proceed without emitting. In a normal
process that inherits the variable from the start, the first call should read
the right value, so this hypothesis is weaker than the destination one.

Falsification: log once, outside the callback or in a dedicated proof, the
recognized value (`enabled=true/false`) and the initialization order, and
call the callback with the variable set before the first access. The test
must check that the event appears with `POM_MOE_ROUTE_DIAGNOSTICS=1`.

### 3. An additional target filter

`EnvFilter` is built with `RUST_LOG=info`; by level, `warn` should pass.
`.with_target(false)` only omits the target name from formatting and should
not drop the event. Still, another subscriber, a module directive or a custom
writer could filter `moe.route_diagnostic`.

Falsification: print a default-target `warn!` and a `warn!` with the MoE
target right at the same point, and capture the effective `EnvFilter`. If only
the MoE target disappears, try an explicit `moe.route_diagnostic=warn`
directive.

### 4. stdout/stderr redirection during restart or llama.cpp load

`llama.cpp` can write through its own log callbacks, but there is no proof in
the Rust code that it replaces the subscriber. The possibility of a
duplicated/null descriptor remains open, especially because of
`respawn_self`, which nulls stdout/stderr.

Falsification: compare the `/proc/<pid>/fd/1` and `/proc/<pid>/fd/2` links
before and after loading, run the `warn!` test after loading the model and
capture both descriptors directly. If the fd changes or points to `/dev/null`,
the problem is the redirection, not the callback.

### 5. Shared expert counted as routed

The shared expert `ffn_*_shexp` must stay out of the routed inventory and be
run by the native `build_ffn`. If it is counted among the 256 routed experts,
index 0 can point to an unfilled region, consistent with a NaN at the first
position.

Falsification: with logging working, compare per layer the descriptor's name
list, `expert_count`, and the pointers and bytes of each payload, requiring
that no `shexp` is among `ffn_*_exps`. The test GGUF must show an effective
count of 256 without the shared one.

### 6. Fused layout and sidecars tied to the wrong component

Fused and separate tensors coexisting, plus `.scale`/`.input_scale` on each
tensor, can leave gate/up using offsets or scales from the wrong components.
The current loader prefers the fused one in `format_heterogeneous.rs:255-270`
and builds scales in 471-493, but there is still no numeric proof for Ornith.

Falsification: the callback must log names, dtypes, dimensions, bytes and
scales for each role; compare each component against the GGUF geometry and
against an independent read of the sidecars. The CUDA differential test must
pass with no NaN and within the published tolerance.

### 7. Q4_K/Q6_K physical stride or ABI still diverging on the CUDA machine

The Rust/CUDA code declares 144 bytes per row for dtype 12 and 210 for dtype
14 in `cuda.rs:1066-1071`, but the ABI assertion only covers the descriptor,
not the semantics of each payload. A stride divergence could still hit the
first block.

Falsification: build with `nvcc`, print dtype, bytes, columns and stride per
payload, and run the per-stage differential tests against the CPU decoder. The
proof cannot be replaced by `cargo test` on macOS.

## 7. Pitfalls and what not to repeat

- The green local regression never covered `cuda_kernels.cu`; macOS builds the
  Rust crate, not the CUDA kernel. The 32 versus 40 bytes error only showed up
  on the CUDA test server. Do not treat a local `cargo test` as a CUDA build.
- The success after reload was not a MoE success: the instance had no
  warm-up/admission of the external executor. Do not use that case as proof.
- `POM_MOE_ROUTE_DIAGNOSTICS` being in `/proc/<pid>/environ` does not prove
  the `OnceLock` recognized the value nor that the subscriber wrote to the
  file examined.
- The `normalize_moe_route` line proves the callback was entered, but does not
  prove the first `tracing::warn!` call reached the sink. Fact 1 needs a proof
  of event transport.
- `with_target(false)` does not mean events with a custom target are filtered;
  it only means the target name is not printed. Do not change it on an
  assumption without capturing the event.
- `append_log` is a supervisor lifecycle file, not an automatic collector of
  `tracing` logs. Grepping that file alone can give a false negative.
- The `resolve_fused_ops: ... enabled` messages alone do not show that a fused
  node was run for the MoE FFN. The nodes or the callback's effective input
  must be observed.
- Do not redo the numeric bridge, revert or push another attempt without
  running and passing the Ornith CUDA differential test:

```bash
POM_MOE_NUMERIC_MODEL=<gguf> POM_MOE_NUMERIC_GPU_LAYERS=99 \
cargo test -p node --features cuda --bin pom \
  inference::llm::pool_tests::qwen35moe_external_route_matches_native_at_layer \
  -- --ignored --nocapture
```

The next worker must first build the executable proof of Fact 1, making the
event appear in the right sink. Only then should the raw values be used to
decide between shared expert, sidecar/layout and stride.
