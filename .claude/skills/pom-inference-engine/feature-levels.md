# Feature levels of a POM inference engine

Work level by level; each level is usable on its own and the next builds on
it. For every item: check whether the official engine already does it
(`engine-intake.sh` output, the engine's docs and tests). When it does, the
plugin bridge only maps it. When it does not, it is an engine feature: a
branch of its own in the engine fork (`branches-and-upstream.md`), merged
into `pom`, and `engine.lock` moved to the new `pom` commit.

"Bridge" is `src/bridge.rs` (Rust, in the plugin) or, when the engine's API is
too far from OpenAI's, a bridge script shipped in the runtime and started
instead of the engine's server (Strata's `runtime/pom_strata_server.py`
wraps the engine's own handler). Never patch the engine tree for POM glue.

## L0 - The engine runs under the plugin

| Item | Where | Check |
|---|---|---|
| Builds from the pinned `pom` commit into `build/runtime` (`bin/server`, `lib/`, `RUNTIME.json`) | `scripts/build-runtime.sh`, `engine.lock` | runs twice from a clean `build/` |
| No shared library the node image lacks (CUDA statically linked, ROCm/Metal libraries carried in `lib/`) | build script; on Linux `ldd build/runtime/bin/server` | only system libraries and the driver |
| A server bound to `127.0.0.1`, on the port the plugin picks, loading the model path it is given | `src/launch.rs` `command` | `verify-engine-plugin.py` reaches `ready` |
| A health route that answers 200 only once the model is loaded | `launch.health_path` | load of a large model stays `loading` until done |
| Chosen GPUs honoured (`gpus`, `gpu_devices`: `CUDA_VISIBLE_DEVICES`, `HIP_VISIBLE_DEVICES`, Metal default device) | `src/launch.rs` | the engine log names the device |
| Stops on SIGTERM within 30 s and frees memory | engine | `engine.unload` returns to `idle`; no process left |
| `ARCHITECTURES` filled with the GGUF `general.architecture` values it runs | `src/launch.rs` | the deploy screen offers the service |

## L1 - The generation contract

| Item | Where | Check |
|---|---|---|
| Streaming generation with the model's chat template for one user turn | engine + bridge | `chat_template: true` answers as a chat |
| Raw completion (`chat_template: false`) | engine `/v1/completions` | the prompt reaches the model unchanged |
| Thinking switch (`thinking`) and reasoning streamed apart from the answer | engine (`reasoning_content`) + bridge `r` lines | no `<think>` in `t` |
| `stop`, `max_tokens` fitted to the room left, `finish` `stop`/`length` | engine + bridge | a long `max_tokens` on a full window ends with `length`, not an error |
| Sampling fields (`temperature`, `top_p`, `top_k`, `min_p`, penalties, `seed`) | engine | `seed` + `temperature` reproduce an answer |
| Token counts, `reasoning_tokens`, llama.cpp-style `timings` | engine usage / timings + bridge | the POM request log shows prompt and decode speed |
| Tool calls left in the text in the model's own form | engine (do not strip them) | a tool call reaches the POM's parser |
| Client disconnect cancels the generation | engine | closing the stream frees the slot |

## L2 - Concurrency and the prompt cache

| Item | Where | Check |
|---|---|---|
| `max_sessions` parallel sequences | engine (`--parallel` or equal) | two requests at once both stream |
| Prefix reuse across turns, reported as `cached_tokens` | engine | the second turn of a conversation reports a large `cached_tokens` |
| KV cache type from `kv` or an engine setting | `src/launch.rs`, `src/settings.rs` | the log shows the type |
| Context fit: refuse or lower `n_ctx` that does not fit, with the reason | engine + `engine.status.detail` | an oversized context is a readable error |

## L3 - Speculative decoding

| Item | Where | Check |
|---|---|---|
| Draft or MTP decoding, when the engine has it | engine | faster decode on the same prompt |
| `timings.draft_n` and `draft_n_accepted` | engine + bridge | the POM trace shows the acceptance |

## L4 - Conversation sessions

| Item | Where | Check |
|---|---|---|
| Save and restore a sequence's state to a file (whole and delta) | engine feature branch | restore then continue gives the same answer as without |
| A named turn kept in a slot, saved at its end (deferred allowed) | engine + bridge `session` | `done.session` |
| `/pom/session*` routes (offer, pending, file, chunk, POST) | bridge | a second machine restores the first one's session |
| Settings fingerprint (`config`) and layer identity (`layers`) in the offer | bridge | a session of another KV type is refused |
| Prefill/decode role (`role`, `prefill_only`, `decode_holds`, `session_source`), `pom_settings` gains `inference_role`, `prefill_min_tokens` | engine + bridge + `launch::POM_SETTINGS` | the trace shows the prefill machine first |

## L5 - Decisions and vision

| Item | Where | Check |
|---|---|---|
| `probe.labels`: label probabilities of the first token in `done.probe` | engine feature branch + bridge | `/v1/systemone` answers on this engine |
| `probe.prefix_bytes` pinned in the prompt cache | engine | the second question reads only its own part |
| Images from `data:` URLs through `mmproj_path` | engine + bridge `images`, `launch::VISION` | an image question answers about the image |

## L6 - A layer slice per machine

| Item | Where | Check |
|---|---|---|
| Load a POM slice (`pom.start_layer`, `pom.end_layer`, `pom.original_block_count`) under the whole model's layer numbers; an earlier stage without the output head | engine feature branch | the slice loads; geometry matches the whole model |
| Accept a hidden state of explicit width as input and hand out the last layer's residual | engine feature branch | a two-stage answer equals the whole-model answer (greedy) |
| Stage link: earlier stage serves frames on loopback (`engine.link`), later stage connects to `prev_port` with the token, kind-byte envelope, tensors as `f32` | engine feature branch + plugin `engine.link`, `launch::STAGES`, `pom_settings` `prefill_max_inflight_chunks`, `hidden_state_dtype` | two machines serve one model |
| Prompt pieces in flight, answers in order; prompt cache continued on every stage | engine | the prompt reads on both machines at once |
| Optional: serve a range of the whole model with no slice on disk (llama.cpp: external tensor data, a header-only virtual slice; the node announces `model/range-load-v1`) | engine feature branch | the range answers as the real slice does; nothing written but the header |

A plugin engine may also take a range of the whole model: it lists the
architecture in `stage_ranges` and gets `range` in `engine.load`
(`engine-contract.md`). Strata does it for Flash-Next (`--stage-layers`). The
other range operations of the layer range deployment spec (`engine.cuts`,
`engine.estimate`) are not implemented: the planner uses the GGUF's cuts.

## Platforms

| Backend | Notes |
|---|---|
| NVIDIA (CUDA) | static cudart/cuBLAS, build in the POM's CUDA image, `CUDA_ARCHITECTURES` for development, the release's wide list |
| AMD (HIP) | ROCm libraries in `lib/`, `HIP_VISIBLE_DEVICES` from `gpu_devices` (vendor `amd`, `ordinal`), container needs `/dev/kfd`, `/dev/dri` |
| Apple silicon (Metal) | the POM node runs natively on macOS; no parent-death signal (the plugin's PID file covers a killed host); `gpus` is empty, the engine uses the default device; package `macos-aarch64` |
| CPU only | allowed; say so in the service `detail` |
