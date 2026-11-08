# Plan: `pom-plugin-vllm`

Status: draft, not started. Follows the `pom-inference-engine` skill
(`.claude/skills/pom-inference-engine/`). The vLLM facts below come from
general knowledge of the project, not from a checked release: verify every
flag against the pinned version in Phase 0.

## The deciding fact: vLLM does not fit POM's model format

POM's catalog, slicing and deploy screen work only with GGUF files, and the
deploy screen offers an engine by the model's GGUF `general.architecture`.
vLLM's native format is a Hugging Face model folder (safetensors). Its GGUF
support is experimental: one file only, a limited set of architectures, and
slower than its native path. The catalog has no notion of a model folder
(`safetensors` appears only in `crates/node/src/models/huggingface.rs`).

Decide before any work:

- **A. GGUF only (recommended first step).** Fill `ARCHITECTURES` with the
  GGUF architectures vLLM loads (llama, qwen2/qwen3, mistral, gemma...). No
  POM change. Cost: vLLM below its best, multi-part GGUF refused.
- **B. Model folders as a new kind of model in POM.** Catalog entries, the
  Hugging Face download, an engine offer keyed on `config.json`
  `architectures`. The real goal for vLLM, but a POM core change with its own
  spec (catalog, deploy, folder transfer in `file-transfer.md`). Second phase.

## What vLLM already does vs what needs work

| Level | vLLM native? | Where the work is |
|---|---|---|
| L0 runs under the plugin | Yes: `vllm serve` on `127.0.0.1`, `/health` answers only after load, stops on SIGTERM | Plugin: the runtime is a Python environment, not a binary |
| L1 generation contract | Yes: streaming, `/v1/completions` for raw text, `--reasoning-parser` gives `reasoning_content`, full sampling, a dropped connection cancels. Tool parsing is off by default, so calls stay in the text | Rust bridge |
| L2 concurrency and prompt cache | Yes: continuous batching (`max_num_seqs`), automatic prefix caching, `cached_tokens` with `--enable-prompt-tokens-details`, `--kv-cache-dtype fp8` | Settings mapping |
| L3 speculative decoding | Yes: `--speculative-config` (ngram, EAGLE, MTP) | Settings. No per-request `draft_n`; only Prometheus counters |
| L4 sessions, prefill/decode split | Partly: KV connectors (LMCache disk store, NIXL/P2P disaggregated prefill) | Fork branch or connector glue. Large |
| L5 decisions (`probe`) | Through the bridge: `logprobs`/`top_logprobs`, `max_tokens: 1`, `allowed_token_ids` | Bridge only, probably no fork change |
| L5 vision | Only for model folders, not GGUF plus `mmproj` | Only with option B |
| L6 POM layer slice per machine | No. vLLM pipeline parallelism is Ray/NCCL, not POM slices and the hidden-state link | Large fork feature. Out of scope |

The fork can start identical to upstream: L0 to L3 and the L5 probe are
plugin work only.

## Phases

### Phase 0: intake and decisions (about 1 day)

- `engine-intake.sh https://github.com/vllm-project/vllm`: fork, `pom`
  branch pinned to a release tag.
- Decide A vs B and platforms: CUDA first, ROCm later, no Metal (vLLM on
  macOS is CPU only, so the plugin does not publish the service there).
- Fill the L0 to L6 assessment table from the vLLM docs of the pinned version.

### Phase 1: L0, the runtime (hardest part of the build)

- `scripts/build-runtime.sh`:
  - standalone Python (python-build-standalone), so the node image needs no
    system Python;
  - a `uv` environment with the pinned vLLM wheel (or the fork with
    `VLLM_USE_PRECOMPILED=1` while it changes only Python code);
  - packaged into `build/runtime/` with `bin/server` (a shell wrapper running
    `python -m vllm.entrypoints.openai.api_server`) and `RUNTIME.json`;
  - expect 5 to 10 GB (torch and CUDA libraries inside the wheels). The
    static-CUDA rule does not apply; the driver is the only system dependency;
  - published as a pinned release asset, as Strata does, never committed.
- `src/launch.rs`:
  - model: `--model <path>`, `--served-model-name`;
  - window and sessions: `--max-model-len n_ctx`, `--max-num-seqs max_sessions`;
  - KV cache: `--kv-cache-dtype` from `kv`;
  - GPUs: `CUDA_VISIBLE_DEVICES` from `gpus`, `--tensor-parallel-size` = GPUs chosen;
  - API key through an environment variable, never the command line;
  - `LOAD_TIMEOUT_SECS` raised: first start compiles CUDA graphs and torch.compile.
- `src/settings.rs`: `gpu_memory_utilization` (vLLM reserves 90% of the GPU
  by default), `enforce_eager`, `reasoning_parser` (select), `speculative`
  (select, for L3).
- Check with `verify-engine-plugin.py` and the mock server, then on the CUDA
  machine.

### Phase 2: L1 and L2 bridge (`src/bridge.rs`)

- `/pom/generate`: `chat_template: true` calls `/v1/chat/completions` with a
  single user turn, `thinking` as `chat_template_kwargs.enable_thinking`;
  `chat_template: false` calls `/v1/completions`.
- Stream conversion: `delta.reasoning_content` -> `r`, `content` -> `t`,
  `usage` plus measured times -> `done` with llama.cpp-style `timings`.
- Fit `max_tokens`: vLLM rejects prompt + `max_tokens` over the window. Count
  the prompt with `/tokenize`, clamp, report `finish: length`, not an error.
- Unsupported requests (images under option A, `session`, `prefill_only`)
  answer `{"error": "this engine does not ..."}`.
- Tests: bridge against the mock OpenAI server; live chat, raw completion,
  two parallel streams, `cached_tokens` on a second turn.

### Phase 3: L3 and the L5 probe

- Speculative decoding through settings. Leave `draft_n` out of `done`, or
  approximate it from Prometheus counters before and after; document which.
- `probe.labels`: `max_tokens: 1`, `logprobs`, `allowed_token_ids` limited to
  each label's first token, probabilities normalised. `prefix_bytes` is
  covered by automatic prefix caching.
- Check `/v1/systemone` answers on this engine.

### Phase 4 (optional): L4 sessions

- Assess the LMCache disk store as the save/restore files behind
  `/pom/session*`. A fork `feature/` branch only where the connector cannot
  name or export one request's KV. Neutral branches, per
  `branches-and-upstream.md`.

### Out of scope

- L6 (POM slices): a vLLM deployment stays on one machine, using its GPUs
  through tensor parallelism. `STAGES = false`.
- Option B gets its own spec in this repository.

## Risks

- Runtime size and build time: release pipeline and node disk.
- GPU memory reservation: vLLM claims most of the GPU at load, so the deploy
  screen's VRAM preview is wrong for this engine.
- Version churn: flags change between releases; pin one vLLM version per
  plugin release.
- GGUF quality under option A may force option B early.

## Open decisions

1. Option A or B for the model format.
2. Whether L4 sessions are in scope.
