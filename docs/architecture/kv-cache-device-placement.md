# KV cache on a device separate from the weights

## Result

With the current vendored llama.cpp version, and without changing the fork
checkout, the KV cache cannot be placed on a specific GPU other than the GPU
that holds the weights of the same model. This task does not add
`context_gpu`, `kv_cache_gpu` or a matching control in the deployment screen,
because those options would have no real effect.

The operator's case, with the weights on the RTX 3060 and the KV on the Radeon
RX 7800 XT, needs a decision before any native implementation. The status of
this task is `needs-decision [key=kv-cache-device-placement]`.

## Evidence in the public contract

The header `vendor/llama-cpp-sys-2/llama.cpp/include/llama.h` offers:

- `LLAMA_SPLIT_MODE_NONE`, `LAYER`, `ROW` and `TENSOR` (`:198-203`). The
  `LAYER` and `ROW` modes state explicitly that they split layers and KV across
  GPUs.
- `llama_model_params.devices`, `split_mode`, `main_gpu` and `tensor_split`
  (`:307-322`). These parameters choose the devices and the split of the
  weights. `main_gpu` only picks the model GPU in `NONE` mode.
- `llama_model_tensor_buft_override` (`:302-305`), one rule per name pattern
  for model tensors.
- `llama_context_params.type_k`, `type_v`, `offload_kqv` and `kv_unified`
  (`:421-470`). There is no `kv_device`, `kv_buft` or list of cache devices
  there.

`ggml_backend_sched` also offers no way out of the public `llama_context`
contract to replace the KV allocation. The scheduler is created by the context
after the memory has already been built, in
`vendor/llama-cpp-sys-2/llama.cpp/src/llama-context.cpp:584-607`.

## Effective path in llama.cpp

1. `llama-model.cpp:1350-1384` computes the split points from `n_gpu_layers`
   and `tensor_split` and stores a `dev_layer` for each layer.
2. `llama-kv-cache.cpp:210-223` walks the cache layers. With `offload=true` it
   calls `model.dev_layer(il)` and uses `ggml_backend_dev_buffer_type(dev)`.
   With `offload=false` it keeps the CPU buffer.
3. `llama-kv-cache.cpp:274-285` allocates the buffers grouped by that buffer
   type. There is no second device choice for the KV.
4. `llama-model-loader.cpp:1237-1254` only consults `tensor_buft_overrides`
   while creating the GGUF weight tensors. The `cache_k_l*` and `cache_v_l*`
   tensors do not exist yet at that stage.

This also explains why `split_mode=row` does not solve the request: it can use
a split buffer for the weights, but cache creation stays tied to the layer
placement. `offload_kqv=false` only solves the VRAM overflow by moving the KV
to RAM.

## Current POM boundary

The Rust wrapper keeps the same separation:

- `crates/llama-engine/src/llm/model.rs:429-447` passes `devices`,
  `tensor_split` and `tensor_buft_overrides` when loading the model.
- `crates/llama-engine/src/llm/config.rs:1012-1022` builds the context
  parameters with context size, sessions, unified cache, `offload_kqv` and the
  K/V types.
- `crates/llama-engine/src/mtp.rs:168-187` repeats the same policy for the MTP
  context.

POM has no safe point to reach an internal `llama_context` structure and swap
buffers after construction. Doing so through pointer casts, manual GGML
allocation or a partial KV copy would be a hack with no synchronization
contract, and it was not implemented.

## Alternatives

### Available now, without the fork

- **KV in RAM:** set `offload_kqv=false`. Keeps the weights where they were
  placed, but leaves the Radeon idle for this purpose and raises the cache
  access cost.
- **Joint split:** use `split_mode=layer`, `row` or `tensor` with both GPUs.
  Uses the total VRAM, but distributes weights, compute and KV by the native
  rules; it does not meet the requirement of weights only on the RTX and KV
  only on the Radeon.
- **Lower the KV pressure:** reduce context/sessions or use smaller K/V types.
  POM already supports this, but it does not create a separate placement.

### Required for the exact request

A future task allowed to change the fork must introduce an explicit contract
for KV placement, for example a `ggml_backend_buffer_type_t` or a device list
in `llama_context_params`. The implementation would have to cover every
native memory family, including `llama_kv_cache`, the recurrent cache and
hybrid memory, keep both backends alive in the scheduler, guarantee the copies
between the weights device and the KV device, and publish the effective
placement for diagnostics. Only then could POM add a persisted field,
index/device validation and the control in the deployment screen.

## Decision requested

Pick one of:

1. temporarily accept KV in RAM or the existing joint split;
2. authorize a separate future task to create the API and the implementation
   of KV on an independent device in the llama.cpp fork;
3. keep the current behavior until that decision.
