# What it took: the engine forks the POM runs today

The POM runs two engines besides its own pipeline code: the `llama.cpp` fork
(in process, through `pom-plugin-llama.cpp`) and Strata (a plugin,
`pom-plugin-strata`, driving the Strata fork). This file separates every
change those forks carry against their official `main` into **features**
(capabilities the official engine lacks), **characteristics** (properties of
build, packaging and behaviour the POM depends on) and **fixes** (defects
found while running in the POM). The last section turns them into what any new
engine needs; `feature-levels.md` makes that a checklist.

Refresh this file after a new engine lands or a fork moves:
`git log --format='%h %s' <upstream>/<default>..origin/<pom branch>` in
each fork, then `git diff --stat` on the same range.

## 1. llama.cpp fork (`pom-tensor-parallel` vs upstream `master`)

32 commits on top of upstream `b49650adb` (2026-09-17), about 80 files and
5000 lines. The engine is linked in process: `pom-plugin-llama.cpp` builds
`llama-cpp-sys-2` against the commit `llama-cpp.lock` pins and its C++ shims
(`llama_mtp_shim.cpp`, `wrapper_common.cpp`) call internal methods. The fork
keeps its own report, `docs/rdma-p2p-divergence-from-upstream.md`.

### Features

| Feature | Commits | What it adds |
|---|---|---|
| Tensor parallelism without moving weights | `ba39cec84` `6e5d86726` `a2d91edd2` | A reduce op after the row-parallel GEMMs (attention output, FFN down) driven by `llama_set_tp_reduce_callback`; the all-reduce lives in the POM node. Identity when no callback is set. |
| External MoE executor hook | `ff59e560a` `03902a5ba` `4bb64b070` `b7b163cdc` `32d461112` `e822a565c` `16c517a43` | `llama_set_moe_ffn_callback` with a generic `llama_moe_ffn_descriptor` (layer, experts, dtypes, shapes); per-layer metadata-only expert loading (`moe_external_executor_layers`); route tensors materialized for the callback; a route copy trace. The POM's own MoE executor runs the experts. |
| GPU cache for host-resident experts | `6b99580c2` `d5926811e` | Port of upstream PRs #29887 and #29030 (`--moe-cache-mib`, lazy-row reads), extended to one cache per GPU. |
| External MTP draft | `1b9c07a81` `7af965b84` | `qwen4exp` exports `h_nextn` for an outside draft head and loads a nextn-only draft file (shared MTP module) with its own `DECODER_MTP` graph. |
| External tensor data (branch `pom-external-data`) | `bd4b9586a` | The loader reads `general.external_data.files`, `.tensor_files` and `.tensor_offsets` from a GGUF header and maps each listed tensor from another file at an absolute offset. It lets a header-only "virtual slice" serve a layer range of the whole model without copying weights (POM's `write_virtual_slice`, on unless `POM_VIRTUAL_SLICES=0`). Kept on its own branch, built by `pom-plugin-llama.cpp`'s branch of the same name, until it is proven; `map_tensor_source` in that plugin branch does the same for the MoE readers (one source file only). |
| Parallel constrained decisions | `5ddafd2d3` `ef5b9bfa4` `5b1f9f9c5` | `/v1/decision` and `llama-parallel-decision`: every allowed value of a finite schema scored in branch sequences forked from one cached context (the POM's typed decisions). Contributed from another fork and merged. |

### Characteristics

| Characteristic | Commits | Why the POM needs it |
|---|---|---|
| Hidden-state input of explicit width | `05efc320b` | A pipeline stage feeds the previous stage's hidden state as batch input; the width may be wider than `n_embd` (hyper-connection models) and must not be truncated. |
| Every symbol exported from the Windows DLL | `239fba375` | The POM's C++ bridge calls internal `llama_model`/`llama_context` methods; Linux exports them, Windows did not. |
| Release packages the POM links | `bdcb6188d` `f7daca92c` `600d31512` `933a6e10e` | CUDA 13.4 plus Vulkan as loadable backends, MSVC import libraries, no web UI publish, so the POM links a release instead of compiling the fork. |
| Pinned by exact commit | `pom-plugin-llama.cpp/llama-cpp.lock` | Repository, branch and commit; CI and the Docker build fetch the same source. |
| Fork documentation | `TP_DESIGN.md`, `docs/rdma-p2p-*.md`, `AGENTS.md` | Why each divergence exists, regenerated after every upstream merge. |

### Fixes

| Fix | Commits | Found by |
|---|---|---|
| Memory fit snapshot taken at the right time | `b60af3833` | Slice planning overran memory in production. |
| Kimi-K3 recurrent rollback | `d9dd86175` | The POM removes whole sequences (never partial) at the end of a turn. |
| Wide residual not re-broadcast on a continuation hop; batched reshape | `682462658` `c83f0d242` | A stage that continues a pipeline receives the already wide residual. |
| CUDA VMM pool gives an idle reservation back on OOM | `8da9d6f12` | Large `n_ctx * max_sessions` reservations on 16 GiB cards. |
| IQ1_S/IQ2_S/IQ3_S grid bytes through `__byte_perm` | `8dd48c3dd` | nvcc 13.2 miscompiled the byte read for sm_120: garbage output on Blackwell. |
| MoE cache robustness | `43e7b097a` `be6572856` `416ffa94d` | A mismatched cache view refused instead of aborting; nothing to cache is not an error; a ggml-alloc view never opens an empty buffer range (Vulkan). |

## 2. Strata fork (`pom-stages` vs the fork's `main`)

40 commits on top of `296b59d2` (2026-10-14); 32 files, about 4000 lines,
mostly `src/program/generate.cpp`, `src/program/stage_link.hpp`,
`src/core/conversation_*` and `serve/server.py`. The official repository has
moved more than a thousand commits since: rebasing is part of the work.

### Features

| Feature | Commits | What it adds |
|---|---|---|
| A layer split across processes | `dc94b4b9` `dc614bba` `e3e1f1ed` `512bd574` | A POM slice (renumbered `blk.N`, `pom.start_layer`/`end_layer` metadata) loads under its layers' numbers in the whole model. Earlier stages run `--stage-serve`, the last `--stage-prev`; a framed protocol (`H Z R S P W C Q`) carries prompt pieces and verify windows; the prompt cache continues on both sides; micro-pipelining; several GPUs and middle stages. |
| Conversation sessions on disk | `d7043288` `21e5eb58` `c3427a16` `37f2dedd` `46d20fc0` `bbd67a6a` `485d9b8d` `e109fb65` | Whole and delta session files (`STRSDLT`): `SAVE`, `RESTORE`, `SAVE_DELTA`, `RESTORE_DELTA`, also into a batch slot. |
| A named turn kept in a batch slot | `c8da77b0` `6127fd6a` `a7dadafa` `e5fda582` `9a8e7acd` `52dfe72f` `59ed5c27` `62b886e7` `383d9932` `ae9f5d26` | `keep_slot`, a turn hook that saves inside the slot window, the finish handed over by the engine (`BDONE`), the turn boundary at the end of the last user message, and the tokens an image holds. |
| Sessions of a layer split | `06bf7a12` `64f5e7e6` `88e7fb6a` `46a19b0f` `543ac574` `edb654ed` `57ee4ba9` | Session file v2 with stage parts; stages joined on a one-GPU machine and a whole session cut into stages. |
| Asynchronous session save | `35316188` | `SAVING` answered at once, the file written on a thread (`--session-async-save`). |
| Decision probe | `aa1e8ea5` | `PROBE` labels the next generation's first token by its label distribution (`/v1/systemone`). |
| Tool calls in the near form | `f5ecb611` | `<parameter_calls>` / `<parameter name=...>` recovered under `tool_call_recovery`. |

### Characteristics

| Characteristic | Commits | Why the POM needs it |
|---|---|---|
| An earlier stage needs no `output.weight` | `9d7cc2a0` | A slice carries only its layers. |
| Template fidelity when thinking is off | `162b18d6` | History keeps the empty thinking block, so the prefix cache matches. |
| Follows the official reclaim API | `92e9c345` | Keeps the fork rebasable on the official branch. |

### Fixes

| Fix | Commits |
|---|---|
| Sliced per-layer geometry read at the model's offset | `e06cde9d` |
| The prompt read walks its cut points in ascending order | `a1972ab8` |
| Temporary diagnostics, added and reverted | `46b76616` `b3dc30d3` `821f378e` (a lesson: never leave them on a feature branch) |

## 3. pom-plugin-strata (67 commits on `main`)

The plugin never edits Strata's sources. Its library supervises; Strata's
engine, server and a POM bridge live in a runtime directory it builds or
downloads.

### Features

| Feature | Commits (first) |
|---|---|
| Whole-model inference service: `host.services` kind `inference`, `engine.load/unload/status/upstream` | `5d493ff` |
| Engine output in the POM process log; settings published to the deploy inspector | `454b18e` `39732a3` |
| Two stages on two machines (`engine.link`, stage frames, hidden-state precision, pieces in flight) | `61bce42` `2ca38bc` `0ee0f73` |
| Tool calls and structured reasoning through `/pom/generate` | `aac5c79` `afc2227` |
| One screen: status, log, and Strata's own app as a tab (`ui.upstream`) | `79ecee7` `201da04` `c760710` |
| AMD cards (HIP runtime), several GPUs per stage, middle stages, release workflow | `0ee0f73` |
| Conversation sessions: save, delta, pieces of 64 MiB, sync between machines, limits | `13e50ed` `0cd9bcd` `1cb3921` `05c7256` `c9e3b68` `94d71d1` |
| Decision probe for `/v1/systemone`, pinned question prefix | `beff677` `086766c` |
| Vision through the deployment's projector (`mmproj_path`, `images`) | `8a7b811` `4287149` |
| Release to GitHub and the license server | `bb3987c` |
| Prefill/decode split (`role`, `prefill_only`, `decode_holds`, `session_source`) | `a32eba5` ... `aa7956c` |

### Characteristics

- The runtime is never vendored nor embedded: `strata.lock` pins the fork and
  branch, `scripts/build-runtime.sh` records the commit in `RUNTIME.json`; a
  release pins URL, size and SHA-256 and the plugin verifies the download.
- The runtime links no CUDA shared library (static cudart/cublas through
  `cmake/pom-static-cuda.cmake`), built in the POM's own CUDA image; HIP
  carries its ROCm libraries in `lib/`.
- The bridge (`runtime/pom_strata_server.py`) wraps the engine's server from
  outside its tree, so the fork keeps only engine features.
- One manager thread owns every child, in its own process group with the
  parent-death signal; `query` reads a snapshot.
- Secrets: the bridge key leaves the library only through `engine.upstream`;
  `src/proxy.rs` drops browser credentials on the way to the engine's app.
- The screen signals `ready()` when the first status settles.

### Fixes

| Fix | Commits |
|---|---|
| The POM's `max_tokens` fits the room the templated prompt leaves | `76a766e` |
| A split model's last stage on one GPU; an earlier stage shows status only | `d0a8894` `122ec3b` |
| The batch state is the engine's, not the service's | `bc0858f` |
| Sessions kept and synced only from 8000 prompt tokens, 5 GiB limit | `93fe0e6` `67e23d0` |
| The whole-model arena without transparent huge pages | `b626269` |
| The KV cache format is a deployment setting, the same on every machine | `d15fbdf` |
| Prefill turns: no thinking budget, two tokens, session kept after the generator ends | `ce57ba0` `356ebf6` `765eb1b` |

## 4. What a new engine needs, derived

| Need | From | Lives in |
|---|---|---|
| Load one model file at a path, on chosen GPUs, with a context size and N parallel sequences | all | engine (usually already there) |
| A loopback server with streaming generation, reasoning kept apart from the answer | plugin `/pom/generate`, Strata `afc2227` | engine server + plugin bridge |
| The model's own chat template applied to one rendered user turn; raw completion when `chat_template` is false | inference.md 3.5 | engine |
| Stop sequences, `max_tokens`, the sampling fields, an honest finish reason | plugin `76a766e` | engine + bridge |
| Token counts and llama.cpp-style timings (with MTP draft counts) | inference.md 7.1 | engine + bridge |
| Prompt prefix reuse across turns (`cached_tokens`) | POM invariant: prefix cache is product functionality | engine |
| Conversation sessions: save/restore, delta, by slot | Strata sessions commits | engine fork (feature branch) + bridge routes |
| Typed decisions: label distribution of the first token | llama.cpp `/v1/decision`, Strata `PROBE` | engine fork + bridge `probe` |
| Images through a projector | plugin `8a7b811` | engine + bridge `images` |
| Run a layer slice of a POM-sliced GGUF, exchange hidden states | llama.cpp `05efc320b`, Strata `dc94b4b9` | engine fork + plugin `engine.link` |
| Build with no shared library the node image lacks, pinned commit | both | plugin `scripts/build-runtime.sh`, `engine.lock` |
| Robust on the operator's GPUs (compiler bugs, OOM retry) | llama.cpp fixes | engine fork (fix branches) |
