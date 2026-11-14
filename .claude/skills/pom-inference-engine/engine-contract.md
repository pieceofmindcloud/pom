# The POM side of an inference engine plugin

The authority is the POM repository: `docs/subsystems/inference.md` section
7.1 and `crates/node/src/inference/{plugin_engine,plugin_session,plugin_stage_link}.rs`.
This file is the map; when it and the code differ, the code wins and this
file is corrected. The POM knows no engine by name: everything below is keyed
by the plugin code.

## Service (`host.services`)

```json
{"id": "<code>", "kind": "inference", "name": "<Display>", "status": "ready|starting|installing|waiting|error",
 "version": "<runtime version>", "detail": "<= 190 chars, no secret>",
 "architectures": ["<GGUF general.architecture>", "..."],
 "settings": [{"key", "type": "number|integer|boolean|select", "label": {"en","pt-BR"},
               "description": {"en","pt-BR"}, "min"?, "max"?, "options"?, "placement"?: "pom"}],
 "stages"?: true, "vision"?: true,
 "pom_settings"?: ["prefill_max_inflight_chunks", "hidden_state_dtype", "inference_role", "prefill_min_tokens"]}
```

The deploy screen offers the service only for a model whose architecture is
listed and only on machines that publish it. `settings` are the engine's, drawn
in the inspector and sent back in `engine.load.settings`; `pom_settings` names
POM settings that apply to this engine's deployments.

## Operations (local node only, through the plugin host)

| Operation | Request | Answer |
|---|---|---|
| `engine.load` | `params`: `model_path` (abs), `model_label`, `n_ctx`, `max_sessions`, `kv`, `gpus` (NVIDIA indices), `gpu_devices` (`{index, vendor, ordinal, name, vram_total_mb}`), `settings`, optional `link`, `mmproj_path`, `session_dir`, `role`, `prefill_min_tokens`, `range` (`{start_layer, end_layer}`: a stage cut from the whole model, see below) | `{"status": "accepted"}` at once; the load runs on the plugin's thread. A refusal is an `Err` with the reason. |
| `engine.status` | - | `{state: idle|loading|ready|stopping|error, detail, n_ctx, ...}`. Read every 2 s. `ready` makes `/health` say `model_loaded`; `error`, or `idle` after serving, ends the generation and the supervisor restarts it within its budget. |
| `engine.upstream` | - | `{"status": "ready", "port", "token"}` only while ready; `starting`; or `{"status": "error", "error"}` (an earlier stage answers error). |
| `engine.link` | - | Stages only: `{"status": "ready", "port", "token"}` of an earlier stage's loopback link; the node hands it only to the next stage's machine. |
| `engine.unload` | - | `{"status": "accepted"}`; stop the engine and free the GPU and RAM. |

While the plugin serves, the node announces `engine/plugin-v1:<code>` (and
`engine/plugin-role-v1:<role>` for a role) instead of the hidden-state
capabilities, so no gateway plans it as a llama.cpp hop.

## Generation (`POST /pom/generate` on `engine.upstream`)

`Authorization: Bearer <token>`. Body:

```json
{"prompt": "<rendered by the gateway>", "chat_template": true, "thinking": true, "max_tokens": 512, "stop": ["..."],
 "temperature", "top_p", "top_k", "min_p", "presence_penalty", "frequency_penalty", "seed",
 "session"?: "<conversation key>", "images"?: ["data:image/...;base64,..."],
 "probe"?: {"labels": ["Yes", "No"], "prefix_bytes"?: 123},
 "prefill_only"?: true, "decode_holds"?: 0, "session_source"?: "<node id>"}
```

- `chat_template: true`: apply the model's chat template to `prompt` as one
  user turn and open the assistant turn; `false`: raw completion, as is.
- `thinking`: open the reasoning block or not.
- `max_tokens`: fit it to the room the templated prompt leaves.

Answer: NDJSON, one object per line, in order:

| Line | Meaning |
|---|---|
| `{"t": "..."}` | answer text; tool calls stay here in the model's own form (the gateway parses them) |
| `{"r": "..."}` | reasoning text; never `<think>` markup inside `t` |
| `{"p": [read, total]}` | prompt progress (optional) |
| `{"done": {...}}` | `finish` (`stop`/`length`), `prompt_tokens`, `completion_tokens`, `reasoning_tokens`, `cached_tokens`, `timings` (llama.cpp style: `prompt_n`, `prompt_ms`, `predicted_n`, `predicted_ms`, optional `draft_n`, `draft_n_accepted`), `stop_sequence`; optional `probe`, `session`, `prefill` |
| `{"error": "..."}` | the request failed; nothing after it |

Between machines the gateway posts the same body to the target's
`/api/internal/plugins/<code>/engine/generate` (signed); the plugin sees only
loopback.

## Typed decisions (`probe`)

`probe.labels`: answer the probability of each label for the first generated
token in `done.probe` (as a llama.cpp peer reports it). `prefix_bytes`: the
part of the prompt repeated across questions; pin it in the prompt cache.

## Conversation sessions (all under `session_dir`, plugin bridge routes)

| Route | Use |
|---|---|
| `POST /pom/session {action: save|restore|save_delta|restore_delta, filename}` | `{tokens, bytes, ms}`; 501 when sessions are off (the POM then syncs nothing) |
| `GET /pom/session/offer?key=` | `{tokens, bytes, ram_tokens, layers, pending, config?}`: what a peer can fetch |
| `GET /pom/session/pending?key=` | `{pending}`: a deferred save not yet written |
| `GET /pom/session/file?key=&kind=full|full_ids&offset=&len=` | the file in pieces |
| `PUT /pom/session/chunk?key=&kind=&offset=&total=` | a piece arriving; published when whole |

The `done` line says `session: {key, saved: {ids, delta}}` or
`session: {key, saved: null, deferred: true}`. A session restores only where
the layers (`layers_compatible`) and the engine settings fingerprint
(`configs_compatible`) match.

## Prefill/decode split (`role`)

A `prefill` machine gets turns with `prefill_only: true`, `max_tokens: 1` and
`decode_holds`; it reads the prompt into a slot, keeps the session
(`done.prefill.tokens`) or answers `done.prefill.skipped` when the new part is
shorter than `prefill_min_tokens`. The decode head gets `session_source`,
fetches and restores the session, and reads only the rest. Requires sessions.

## Stages of a split model (`stages: true`)

- The model file is a POM slice: `blk.N` renumbered from 0, GGUF keys
  `pom.start_layer`, `pom.end_layer`, `pom.original_block_count` (the prefix is
  the brand slug). Load it under the whole model's layer numbers.
- First stage: `link: {}` - make a secret, serve the link on loopback,
  return it from `engine.link`. Later stage: `link: {token, prev_port,
  max_inflight}` - connect to `127.0.0.1:prev_port`, present the token.
- Frames both ways: little-endian `u32` length then bytes. Each payload starts
  with a kind byte: `1` raw bytes; `2` a tensor (`u32` meta length, meta, then
  `f32` values), which the node carries in the deployment's
  `hidden_state_dtype` and hands back as `f32`.
- Only the last stage takes requests. Up to `max_inflight` frames may be in
  flight; answers come back in order.
- Reference protocol: Strata `src/program/stage_link.hpp`.
- Optional, a stage from the whole model: a service that lists an architecture
  in `stage_ranges` gets `model_path` = the whole model (its first file) and
  `range: {start_layer, end_layer}` in `engine.load`, with the same `link`
  rules (first stage when `start_layer` is 0, last when `end_layer` is the
  last layer). It loads only those layers, the embedding on the first stage
  and the head on the last; nothing is written to disk. Reference: Strata
  `--stage-layers`, `pom-plugin-strata` `apply_range`.

## Health and placement the node derives

`/health` of a plugin engine: `model_loaded` only once `ready`, the layer
range, `n_ctx_per_session` (the whole window), `selected_path:
plugin/<code>`. Images are sent only when the service declares `vision` and
the deployment has a projector.
