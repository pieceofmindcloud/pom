# Subsystem: deploying to the network

This subsystem puts into service a set of slices that already exists,
distributing them across the cluster's machines. It lives in
`crates/node/src/backend/deploy/` (planning and sequencing) and uses
`crates/node/src/backend/webui/` (applying the slice on a node,
`apply`/`launch`/`release`), `crates/node/src/models/` (catalog,
`plan_by_size`/`extract_shard`, `splitter::merge`) and
`crates/node/src/backend/transfer.rs` (moving the bytes). The whole logic is
"the operator's choice is part of the request": the node suggests, the operator
decides, and the deploy rescans the cluster instead of trusting the preview.

The durable record of each deployment lives in `pom.db`, in `POM_DB_DIR` when
set or in the project root by default. The validated JSON stays complete so the
operator's intent can be listed and recovered; undeploying marks the record
inactive and keeps a tombstone. The legacy `deployments/` folder is neither
deleted nor written by the runtime. The optional copy is only done by the
throwaway script `scripts/migrate_pom_db.py`.

## 1. Overview: the plan, the merge and the order

Two routes do the heavy work: `POST /api/ui/deploy/plan` (reads the topology,
called after a structural edit) and `POST /api/ui/deploy` (executes; rescans
the cluster). The request carries `group` (the original model, by the
technical key `ShardSet.group` derived from the `.pom`'s `pom_id` - or the
model's **name** in the catalog, with `kind: "whole"`), `kind` (`"shards"`, the
default, `"whole"` - see section 1.1 - or `"tensor_parallel"` - see section
1.2), `params` (the `RuntimeParams` of the whole pipeline), `selection` (the
operator's explicit choice - `None` = the node decides) and `merge` (joining
adjacent slices on one machine). `selection` travels in the request because
`deploy` re-surveys: the choice is validated against the new scan and refused
if it no longer holds - the screen's copy can be a minute old, and a machine
that left the cluster cannot receive a slice.

Before copying anything, the deploy releases the machines that already serve
the same model, proven by identity (`prepare_existing_model`), and waits for
them to become idle. `keep_existing: true` limits that release to the machines
of the plan: the others keep their deployment, which stays in service beside
the new one. A machine of the plan is always taken over, whatever it serves,
because a node holds one model at a time. The deploy screen asks the operator
about both cases before sending the request.

The request also accepts `deployment_name` (an operational name separate from
`group`), `strategy: "simple" | "complete"` and `preferred_node_ids`. An empty
name uses the model name and names longer than 128 characters are refused.
`group` stays the technical identity used by the catalog, gossip and routing.
The simple strategy uses the smallest sufficient set of peers; the complete one
prefers the finest existing cuts and spreads the load across more machines. No
slice strategy creates new GGUF boundaries: that is still the job of the Slice
screen. Whole-model mode is the controlled exception: when the operator asks
for a prefix for another peer, the backend queries the real GGUF boundaries
and materializes the ranges on the peer that owns the file.

On the deploy screen, switching from slices to `whole` clears the map and does
not pick a machine automatically. The operator adds the desired machine from
the bottom strip; the UI keeps the one-machine rule for that mode. Changes to
`RuntimeParams`, to the per-machine GPU/KV overrides in the inspector and to the
operational name do not change the topology: the screen neither restarts the
`plan` nor asks for a new capacity preview on every control. The plan key only
considers ranges and machines; the overrides stay in the local rows and go into
the deploy. The old projection stays hidden until `POST /api/ui/deploy`, which
runs the full preflight with the current values before starting.

Three rules come from the architecture, not from taste:

- **Pipeline order = layer order.** Whoever holds layer 0 receives the whole
  prompt of every request. Choosing the order is the same act as choosing who
  gets which range - there is no second button.
- **A node can serve several distinct ranges.** Each valid range becomes a
  pipeline stage, even when the same machine gets separate ranges. With `merge`
  on, only adjacent ranges on the same node become a merged unit
  (`splitter::merge`, Rust): the machine joins the slices into one file and
  serves the union. The merged step carries `parts`, one per slice. The same
  exact range on different nodes is a replica of the same stage and forms a
  valid parallel route.
- **Partial overlap and a global gap are refusals.** Two ranges with different
  cuts over the same layer would make the answer ambiguous; exact copies are
  ignored in the logical coverage and stay available to the router. A valid
  assignment must cover each required layer exactly once.

## 1.1 Whole model: the degenerate case, not a second path

`kind: "whole"` (`DeployKind`) deploys **an unsliced GGUF on one or more
machines**. Copies of the full range are replicas of the same stage and valid
parallel routes for balancing by RTT/load. There is no second flow: `survey`
reads the model from each peer's `/api/ui/models` `models` (instead of
`shard_sets`) and turns it, in `parse_whole`, into a `HeldShard` of range
`0..total_layers-1`. From there on it is all the same code - `candidates`,
`model_size`, the coverage arithmetic, the disk/memory guard, the copy phase
and `launch_targets`. Coverage closes by construction, "a node serves one or
more ranges" is trivially satisfied, and each target goes out as a plain
`LaunchTarget` with `skip_split: true`, which makes the `apply` on the target
recognize itself as `single_peer` (no hidden-state extraction). When the
operator selects more than one target, each copy becomes an independent
`Step`; a literal repetition of the same range on the same target is ignored
as an idempotent action.

What actually changes:

- **Where the inventory is read from.** `parse_whole` refuses a slice
  (`shard_range` filled) and refuses a model without a layer count in the GGUF
  header - `total_layers` is the number every peer is configured with, and
  `NodeConfig::validate` would refuse the zero on the target, after `launch`
  had already stopped inference there.
- **The noun in refusals.** `DeployKind::unit` swaps "the slice L0-31" for
  "the model <name>": whoever chose a whole model sees no slice on screen.
- **The model's identity.** `group` is the catalog `name`, and it travels as
  `model_label` to the peer - two different names for the same file are two
  models, here and in the pipeline planner.

Known limit: if the target machine does not have the file yet, the copy uses
the **pull** direction, which `transfer::safe_model_path` confines to the
sender's slice folder. A whole model almost always lives outside it (LM Studio
library, Ollama store), so the step's `note` advises sending the file through
the Transfers screen first.

Planning a whole model does not wait for the full catalog. If the fast
inventory already showed the file but the answer has no `total_layers` yet,
the survey asks that peer for `POST /api/ui/model-metadata` and reads only the
header of the selected path. The rest of the folder can stay at
`metadata_complete: false`; that does not turn unrelated neighbors into a
prerequisite for the deployment.

### 1.1.1 Choosing layers from the whole model

`POST /api/ui/deploy/plan` can receive `whole_slice` with
`{source_node_id, target_node_id, end_layer}`. The source must announce the
selected whole model. `model-layer-options` reads the GGUF on that source and
returns the accepted prefix ends: any whole-layer end in a dense layout, or
only the architectural boundaries of `arch_boundaries` in a hybrid layout. The
frontend uses these points as the slider positions; it never turns the slider
into a fixed step.

On execution, `deploy` creates a virtual range for the prefix and another for
the rest, calls `slice-ranges` on the source and waits for the slicer to seal a
manifest with `sha256` before starting `transfer/pull`. If a range already
exists on the target, it stays in the list of available layers and has no
slider. A failure or missing GGUF read is a refusal by name, with the reason
visible in the inspector.

The preflight also checks the output contract the loader requires. For the
`deepseek4` architecture, `llama.cpp` requires `token_embd.weight`,
`output_norm.weight`, `output.weight`, `output_hc_fn.weight`,
`output_hc_base.weight` and `output_hc_scale.weight` in a loadable instance;
since it knows neither `pom.start_layer` nor `pom.end_layer`, the source copies
the real head weights into each non-final range before starting the
deployment. The preflight only refuses when the source does not allow that
copy, listing the missing tensor or the concrete cause. Layer counts the loader
compares with the local index are also rebased to the range;
`nextn_predict_layers`, which counts from the end, is zeroed outside the final
range.

## 1.2 Tensor parallel: a group of ranks

The TP path prefers the round already sliced into `shard_sets[].tp_parts`.
Each part covers the whole model and carries `tp_rank` and `tp_size`; the UI
groups by width and shows `1of2`, `2of2`, or `1of4` to `4of4`. These parts are
not pipeline stages and cannot become a single `L0-31` range.

`kind: "tensor_parallel"` (`DeployKind::TensorParallel`) deploys a model on a
group of machines through tensor parallelism: a **primary** (owner of the GGUF
and the pipeline hop) + K **workers** (each extracts its own tensor shard from
its own local copy). The request carries `tp` (`TpLaunch`): `{size,
primary_node_id, worker_node_ids}` - the operator picks the **primary** and the
**workers** by node_id among the machines on the map (the screen offers
selects, not a URL field), and the node derives `rank0_url` from the inference
port announced by the primary itself (`Machine.inference_url`) and keeps the
other address candidates. When there are no `tp_parts`, the scan reads the
whole model from the primary, and `launch_targets` expands the step into **one
target per rank** (the primary as rank 0 and each worker as rank 1..size-1),
all pointing at the same `rank0_url` and its candidate list for the
collective. When there are `tp_parts`, `survey` resolves each target by range,
rank and group size. If the rank is already on the target, there is no copy;
otherwise the planner finds the same part on another peer and transfers it by
`sha256`. When the explicit selection also lists a worker as if it were a
pipeline stage, the planner removes that row before building `steps`: a worker
is only a rank of the collective and each machine enters `launch_targets` once.
If the libp2p tunnel cannot dial, the rank tries each known HTTPS address of
the target, which covers peers behind NAT and containers on the same host.

Before accepting, the plan validates in two layers: `tp_group_checks` (size,
primary/workers in the cluster, available ranks, no repetition and inference
URLs) runs in the `plan` itself; when it uses a whole model,
`tp_shardability_check` asks the primary (route `POST /api/ui/tp/check`)
whether its GGUF can be split - MoE/SSM/weight-tied or dimensions that do not
divide refuse the plan **on screen**, with a reason naming the tensor, instead
of letting the job fail in the apply.

`POST /api/ui/tp/check` is an operator route like the rest of `/api/ui/*`: the
call must carry the connection key (`bearer_auth`). Going without it yielded
`{"error":"missing Authorization header"}`, which is valid JSON without `ok`,
and the code announced that as "does not have the shard check (old binary?)" -
sending the operator to compare versions with both machines on the same commit
(08/17). Today the answer is read by `tp_check_verdict`, which repeats the
peer's `error` when there is one and only talks about an old binary when the
body explains nothing.

The `apply` side tells the two formats apart. For `tp_parts`, the `.pom` is
fetched and merged before the bytes, each target points at the already
existing file and `skip_split: true` prevents a new extraction. Without
pre-sliced parts, `ApplyRequest.tp` (the `TpIdentity` from `network.rs`) makes
**each rank** extract its own shard through `plan::extract_tp_shard`, including
rank 0. `TpIdentity` also carries `members` - all ranks in order, with
`node_id` and inference URL -, because **rank 0 must put the workers in the
same decode**: TP only works in lockstep, and the gateway only talks to rank 0.
A member without an announced inference port is a refusal in the plan
(`tp_members`), not a hang at the first barrier. The node also announces the
role through `Network::set_tp` and registers the collective
(`register_tp_reduce`). See `docs/subsystems/slicing.md` (tensor shard
section) and `docs/subsystems/p2p-network.md`.

Each member also carries the libp2p `PeerId`, the announced UDP candidates and
the server's tunnel capability. So rank 0's `broadcast_to` and the collective's
reduce can rehydrate the swarm even when the process running the rank did not
get the same announcement, and retry the call through the swarm when the
machines are on networks without a direct HTTPS route.

`AddNodeRequest` also carries `kind`: adding a second machine to a whole model
already live is a parallel route, the same as adding a machine to a slice.

## 1.3 Output capacity derived from the runtime

The `max_new_tokens` field inside `params` is the size of one backend leg.
There is no second absolute ceiling in `pom.json`: the request's total budget
comes from the client's `max_tokens` and the space left in the active
runtime's `n_ctx` window. The gateway stitches legs together invisibly until
the client stops, reaches its budget or the context cannot hold more output.

Exact operational steps before testing a long deployment:

1. On the deploy screen, set `n_ctx` and `max_new_tokens` in `params` to the
   chosen values, for example `n_ctx: 100000` and `max_new_tokens: 32000`.
2. Apply the deployment and wait for the `/health` of **each peer** to confirm
   `model_loaded` and `runtime_capacity`. A peer that is still restarting does
   not enter the published capacity. The gateway's `/health` keeps answering
   during the cold catalog enrichment; the optional Ollama probe has a short
   deadline and does not turn that work into a block on HTTP readiness.
3. Query the authenticated `/v1/models` and confirm `context_length: 100000`
   and `top_provider.max_completion_tokens: 100000` when that is the effective
   context. Example without exposing the key:

   ```bash
   curl -fsS -H "x-api-key: ${POM_API_KEY}" \
     http://<gateway-host>:8080/v1/models | jq '.data[] | {id, context_length, max_completion_tokens: .top_provider.max_completion_tokens}'
   ```

The gateway announces the smallest `n_ctx` confirmed among the peers of the
model's planned pipeline and keeps `max_new_tokens` only as the leg size. An
idle peer chosen by the pipeline is woken by `reload_model` before the prefill.
All planned idle peers receive the event in parallel, and the barrier waits on
`/health` for up to 15 minutes so a cold load can finish; if that deadline
passes, the request fails with an explicit error. Live peers of alternative
routes do not block the capacity. The client's local limit, such as
`maxTokens: 5000` in Pi, stays independent of this capacity. A legacy
`pom.json` containing the removed field is refused on read, instead of being
accepted with a hidden default.

## 1.4 Deployment thermal protection

Each node writes `thermal` to `pom.db`, with a default `limit_c` of 85 °C,
`hysteresis_c` of 8 °C and `cooldown_secs` of 30 seconds. The monitor uses the
same `SystemLoad.gpus` list the screen shows. A known reading above the limit
unloads the model, removes the layer announcement and publishes the `thermal`
state; the deployment configuration is kept for resuming.

Resuming only happens when every GPU has a known reading at most
`limit_c - hysteresis_c` (77 °C by default) and stays there during the
cooldown. A missing reading, a driver failure or a GPU without a sensor does
not trigger an undeploy and also does not prove it is safe to turn back on.
`POST /api/ui/stop-inference` clears the pending recovery, so a manual undeploy
is not undone by the monitor. Each transition logs the observed temperature,
the limits and the time in the server log.

## 1.5 Unloading when the peer goes idle

`RuntimeParams.unload_if_idle` and `idle_minutes` are persisted per
deployment. When enabled, each peer records locally the last request in
`backend/reqlog.rs`. The existing monitor, outside the hot path and without
creating an inference timer or `sleep`, evaluates that stamp and calls the
local unload. The deployment stays active: the live peer announces `idle`, with
its range preserved, instead of `serving`. The deployment identity stays in the
model catalog while all matching peers are `idle`, even without
`runtime_capacity`; that lets a request by name be planned and wake the peers
before the first hop. A restarting peer (`runtime_capacity` missing without
`idle`) stays out of the catalog until it confirms `/health`.

On the first request after `idle`, the gateway reloads the local runtime and
resets the activity. It sends the authenticated `reload_model` event in
parallel to each planned idle peer; the event is idempotent for a second
request that wakes the same machine, and each recipient loads its own slice.
So the first request may wait for the load, but the hops are not warmed up
sequentially nor wait on each other. The event answers at once (`loading` or
`ready`) and the gateway repeats it every second until every peer is `ready`
(15 min ceiling): one call held open for the whole load depended on a
connection surviving minutes of silence, and when it broke the request failed
while the peer went on loading. A waking peer keeps announcing `idle` until its
model answers, so a second request that arrives during the load waits for it
instead of dropping the peer from the plan. Once the pipeline is awake the
gateway waits for the peers' announcement (up to 5 s) and runs admission and
planning again, because an idle peer announces no runtime capabilities and a
plan made while it slept would run degraded. The wait shows up in the trace as
the `pipeline_wake` gateway span. The supervisor waits for the idle
generation to shut down completely before starting the new one, and the
model's FFI release runs off the async executor in `spawn_blocking`; the
gateway and `/health` stay responsive during that interval. The status goes
back to `serving` when `/health` confirms the model is loaded. `idle` is per
peer, not per deployment: an idle machine does not change the others' state.
The shutdown signal of the HTTP/3 endpoint is one-shot and persists even if the
QUIC task has not done its first poll yet. After draining, the server waits at
most 2 s for the QUIC endpoint and aborts the task if needed; the supervisor
has an extra 5 s ceiling. If cleaning up the previous generation does not
finish within that deadline, the reload fails without starting another
generation and keeps the handle for a later attempt, avoiding overlapping
native resources. The immediate range announcement is best-effort with a 1 s
ceiling, without delaying the local start when DNS or UDP do not answer.

## 1.6 A plugin's inference service

`RuntimeParams.inference_service` names the plugin whose inference service
runs the model instead of llama.cpp (the Strata plugin publishes `"strata"`).
The deploy screen offers it as "Inference service" next to llama.cpp when any
machine publishes a service of kind `inference`; choosing it fixes the kind to
`whole` - or keeps "Slices" when the service publishes `stages` - lists only
the whole models whose GGUF `architecture` the service declares, and keeps
only the machines that run it in the peer strip. Its
inspector shows the settings the engine reads (context, concurrent requests,
idle unloading) and the engine's own settings, as the service publishes them
(`components/EngineSettings.tsx`); those go in `RuntimeParams.engine_settings`.
The POM settings the service declares in `pom_settings` (in-flight prefill
chunks, hidden-state precision) are shown in a separate "POM settings" card
when its slices cross machines. The per-machine GPU layers and KV cache on the
GPU are llama.cpp's and are not offered.

`POST /api/ui/deploy` (and the preview, when the screen sends the service in
`params`) refuses such a deployment unless it is a whole model on exactly one
machine, or layer slices on distinct machines for a service that publishes
`stages`, where every machine's plugin publishes that `inference` service in a
state other than `error` or `waiting` (`plugin_engine::deploy_block`, which
asks each machine for its services). The slices of a split model link through
the nodes (`inference.md`, section 7.1, "Split across machines"). Everything after that is the whole-model path of section 1.1:
the same survey, copy, `launch`, `apply`, journal, versions and undeploy. The
difference is on the target, where the runtime asks the plugin to load the
model instead of starting llama.cpp (`inference.md`, section 7.1); undeploying
or removing the machine stops the runtime, which asks the plugin to unload.

## 2. The survey (`survey`)

`survey` gathers, for each machine in the cluster, `GET /api/ui/models`
(catalog, timeout `PEER_TIMEOUT * 4`) and `GET /api/ui/node` (identity/
hostname, timeout `PEER_TIMEOUT`). It builds `Vec<Machine>`:

- `Machine { node_id, hostname, mem_free_mb, gpu_free_mb, ram_free_mb,
  disk_free_mb, rtt_ms, load, held: Vec<HeldShard> }`.
- `HeldShard` is a slice of the set on that machine, with `total_layers` as an
  `Option` - `None` is the normal state of an old slice (no `pom.*` metadata),
  and it stays an `Option` up to `model_size` because "unknown" and "zero" are
  different facts. A TP part also carries `tp_rank` and `tp_size`; parts with
  the same range are only deduplicated when rank and width also match.
- `mem_free_mb` is free VRAM on a machine with a discrete GPU, available RAM on
  any other; `0` = the machine did not answer, and nothing is inferred from it.
- `disk_free_mb` is the space for the file, which is what a copy needs and
  what `transfer.rs` checks before writing a byte.
- `gpu_free_mb` and `ram_free_mb` are exposed separately so the screen can
  suggest GPU first and RAM as fallback capacity. `mem_free_mb` stays the
  effective capacity used by the planner's checks: discrete VRAM when
  available, otherwise RAM.
- The machine's `rtt_ms` is the measurement seen by the gateway to guide the
  automatic choice; `0` identifies the node itself and `null` means there is
  no measurement yet. The edges of `route_options` use a separate, directional
  matrix, aggregated from the `/api/ui/network/rtt` reports, so a third peer's
  latency is not shown as if it were the real hop. `load` is the number of
  in-flight requests observed by `LoadTracker`.
- `label()` - hostname, or the first 12 characters of the `node_id` when the
  hostname is empty or repeated.

`holds(start, end)` finds a plain slice and `holds_tp(start, end, rank, size)`
finds the exact TP rank. `overlapping_layers` finds overlap between a range
and another's collection.

## 3. The plan (`plan`)

`plan_with_options(group, machines, selection, params, merge, kind, tp,
strategy, deployment_name, preferred_node_ids)` builds the `Plan`. Without
`selection` (the default path), the strategy picks `auto_chain` or
`auto_chain_complete`, and `auto_assign` considers combined GPU/RAM capacity,
load, RTT and preferred peers; a peer without the local slice can also receive
the copy; with `selection`, each `SelectedSlice { start_layer, end_layer,
target_node_id, n_gpu_layers, offload_kqv }` is validated by position:

- **An empty `target_node_id` is a refusal, not an invitation** for the node
  to choose.
- **Machine with `n_gpu_layers`/`offload_kqv`**: only what is a property of the
  hardware travels per machine. `offload_kqv` is only the GPU-versus-RAM
  boolean, and when it stays on the GPU the cache follows the layer that holds
  the weights; there is no separate GPU index. The KV cache is sized by the
  context window, and a box with 16 GB of VRAM and one without a GPU in the
  same pipeline cannot both be right about where it fits. Everything else
  (`RuntimeParams`) belongs to the whole pipeline - context, batch, chunk and
  hidden-state precision decide the prompt budget the gateway charges, and
  sampling only runs on the last peer. The precision is `f16` by default;
  `f32` is the lossless option, with four bytes per value, while `f8` and
  `i4g64`/`i4g128` are lossy formats.
- **Precision capability**: `f32` and `f16` are base formats, with no extra
  negotiation. The f8/i4 formats can only be deployed when every selected peer
  announces the matching capability in `inference_frame_capabilities`. The
  planner blocks the deploy with the incompatible peer's reason; validation on
  the inference path keeps the same protection against diverging
  configurations after the deploy.
- **Automatic memory fit**: `RuntimeParams.fit_memory` is on by default in
  `shards` and `whole`. The node queries the backend's free and total memory
  per device and uses `RuntimeParams.fit_memory_margin_mb` to set each one's
  safety reserve, 256 MiB by default. The free amount already excludes existing
  processes, so the margin is not a second charge for the usage. llama.cpp's
  `common_fit_params` runs before loading using the total capacity `n_ctx *
  max_sessions`, and may reduce the GPU layers to avoid OOM. An `n_gpu_layers`
  other than `99` is explicit and turns this layer fit off. In
  `tensor_parallel`, the planner normalizes `fit_memory` to off because the
  native fit does not support `split-mode tensor`.
- **MoE experts in RAM and the GPU expert cache** (POM settings, Beta group):
  `RuntimeParams.moe_experts_in_ram` keeps every routed expert in host memory,
  computed by the CPU (llama.cpp's `--cpu-moe`, as a `tensor_buft_overrides`
  entry); it turns the layer fit off, because `common_fit_params` refuses
  overrides set before it. `RuntimeParams.moe_cache_mib` (`0` = off) becomes
  `llama_context_params::moe_cache_size`: recently used experts held in RAM are
  served from VRAM and only misses cross PCIe, for batches of up to 32 tokens
  (generation; prefill keeps the regular offload). With the fit on, each node
  first fits without the cache and keeps it only when that plan leaves routed
  experts in host memory (`fit_plan`): a node whose experts all fit in VRAM
  would otherwise lose expert layers and the MTP draft's room to the cache. When
  the cache stays, the fit counts it and keeps fewer whole expert layers on the
  GPU. When a context fails to fit with the cache, the engine retries with half
  the cache, and turns it off once half is under 512 MiB
  (`moe_cache_retry_mib`). Without routed experts in host memory the cache has
  nothing to hold and changes nothing. The
  size is per GPU: on a node with two GPUs (an RTX over CUDA and a Radeon over
  Vulkan, say) both cards keep their share of the layers in VRAM and each one
  caches the experts of its own layers (one cache per device in the fork).
  While the cache is on, the gateway's adaptive prefill chunk never goes below
  64 tokens (`MOE_CACHE_MIN_CHUNK`): a 32-token chunk is a generation batch for
  the cache and thrashed it. `RuntimeParams.adaptive_prefill_chunks` (POM
  settings, Beta group, on by default) turns the adaptive chunk off, so every prompt
  goes in chunks of exactly `prefill_chunk_tokens`. Measured on a whole
  Qwen3.8-Flash-Next UD-IQ3_XXS (78 GB) with one 16 GB GPU: generation 14.95 ->
  19.26 tok/s with all experts in RAM and an 8000 MiB cache.
- **A slice that exists on no machine** -> `blocked` ("slice L{}-{} exists on
  no machine in the cluster"). `plan.blocked` and empty `steps` are the two
  400 refusals of `deploy`.
- **A busy machine is not a `plan` error**: `alternatives`/
  `merged_alternatives` list every machine as a target for each range with the
  reason (`ok`/`note` - "already has the slice", "only N GB free on disk",
  "would receive N GB"). Being busy is deliberately outside `ok`: a machine
  that already serves another range is exactly the one the operator wants to
  use as a parallel route, and hiding it would make redundancy impossible. A
  peer without the local slice also shows up when it has room to receive the
  copy. Capacity uses GPU first and RAM as fallback.

**`steps` always comes out in layer order**, refused plan included, and
`position` is numbered after that sort. The order is not cosmetic: pipeline
order *is* layer order, `run` reads the launch order straight from this list,
and the screen aligns the plan with rows it keeps sorted by layer. The
grouping loop walks in layer order and only joins consecutive entries when
`merge` is on, so separate ranges on the same machine stay independent stages.
For the same reason **an empty `target_node_id` never groups**: "no machine" is
not a machine, and each range without a target keeps its own step and reason.

The `Plan` carries `total_layers` with `total_layers_inferred` (derived from
coverage when the slice does not declare it), `transfer_bytes`, `steps` (each
with `ready` when the machine already has the slice, and `merged` when it is a
merged unit with `parts`), `blocked`, `merge`, `available`, `machines`,
`missing`, `overlap`, `warnings`, and the two engineering numbers derived from
`params` (`prompt_tokens` and `chunk_tokens`, from the same formulas as the
gateway - `prompt_budget_of` and `prompt_chunk_of`), `deployment_name`,
`strategy`, `route_options` and `load_distribution`. `route_options` only shows
the bridge between machines selected in consecutive stages, with the known
directional RTT to the next stage; it does not invent latency when the source
has not published the route. Exact replicas do not create an extra hop: they
belong to the same stage and the inference router picks the copy with the
lowest measured RTT, also balancing the in-flight load. `load_distribution`
separates current load from the load projected by the plan, without opening a
second concurrent run.

### Optional vision

The catalog reads the GGUF headers of the model and of the candidate
projectors. It accepts a common GGUF identity or a file name corroborated by
dimensions; generic names also work when `embedding_length` and
`clip.vision.projection_dim` identify a single association. Quantization does
not pick the projector. Without proof or with incompatible dimensions, the
model gets `mmproj_path: null` and `mmproj_reason` explains how to fix the
inventory.

For slices, `ShardSet.mmproj_path` inherits a whole model's path only when the
manifest label identifies that file and its SHA-256 matches `pom_id` exactly.
The catalog replicates the proven path to the slices of the same group. The
`Plan` receives the chosen head's path and reason; on deploy, the screen sends
`vision` and the backend surveys again before resolving the plan once more.

`DeployRequest.vision` can only be set when the plan has a projector. In that
case, `deploy::lifecycle` sets `use_mmproj` only on the target whose
`start_layer` is `0`. Only the head carries `--mmproj <path>` and turns the
image into a hidden state; the following peers receive that state and never
touch the raw image. Before starting the server, `NodeConfig::validate`
rereads the headers and refuses a dimensional mismatch or a contradictory GGUF
identity, including on a path persisted by an old configuration.

### Sidecars: vision projector and MTP draft anywhere in the network

A projector or an external MTP draft rarely sits next to the file a peer
loads: a slice lives in the shard folder, and the sidecars stay in a models
folder, often the folder of another quantization. Three pieces handle that.

- **Inventory.** Each node's catalog lists `sidecars` (`catalog::Sidecar`)
  under the shard folder and the GGUF libraries, read from the header: a
  projector has `general.architecture == "clip"`; an MTP draft has
  `<arch>.nextn_predict_layers > 0`, no split and at most
  `MTP_DRAFT_MAX_TENSORS` tensors (the `mtp-` prefix is only a hint, because a
  draft's `general.name` may say nothing about its model). Sidecars are never
  listed as models. `Sidecar::compatible_with` is the one rule: a projector
  matches the model's `embedding_length`; a draft also its architecture.
- **Link.** `SidecarLink` (`model_key`, `kind`, `mode` = file / embedded /
  none, file name + size) is kept on the principal in `model_sidecar_links`.
  `model_key` is the model label, the same string for a slice set
  (`model_label`) and a whole model (`name`). A file is named by name + size,
  never by path, because each machine keeps it in its own folder.
- **Plan and launch.** `deploy::attach_sidecars` resolves the screen's choice
  (`DeployRequest.vision_choice` / `mtp_choice`) or else the link: the head
  gets the projector, the tail gets the MTP draft. A target that already has a
  file with that name and size uses it; otherwise a holder is chosen, the copy
  joins `transfer_bytes` and the `pulls` of `lifecycle::run` (by path, landing
  in the target's `received/` folder), and a file nobody has blocks the plan
  naming it. `LaunchTarget`/`ApplyRequest` carry `mmproj_file` and
  `mtp_draft_file`; the target resolves them on its own disk at apply time
  (`catalog::find_local_sidecar`), so a file copied a moment ago is found
  without waiting for the catalog cache. A model without `nextn` reports MTP as
  supported when a compatible draft exists anywhere in the network. With no
  choice and no link, the old defaults apply: the catalog's sibling projector
  and the `mtp-` sibling of the loaded file.

### The model and its size

`model_size` resolves the model's total layers with the precedence
**Declared > Inferred > Unknown**: a slice that declares
`pom.original_block_count` rules; without a declaration, the total is derived
from the coverage reach across the whole cluster (never a guess from a single
slice); without any of that, `Unknown`. That is what prevents the 2026-08-10
bug, when the plan claimed to be complete and every `apply` died in
`NodeConfig::validate` after `launch` had already stopped inference on the
target machines. Slices without a total are named "L{}-{} on {}" (`nameless`)
and only show up that way.

## 3.1 Capacity preview (projected VRAM)

After `plan_endpoint` builds the steps (and only if `blocked` is empty),
`enrich_capacity_previews` asks each target where the step is already
materialized locally how that step would fill its VRAM with this plan's
parameters. That is what the Deploy screen uses to draw the cards as a
**preview of the system after deployment**, not as the current state - one bar
per GPU, projected usage against the total, recomputed on every change of
context, batch or sessions (260 ms debounce and one round trip, like the rest
of the plan).

- **One call per `Step`** (`POST /api/ui/deploy/capacity-preview`, handler in
  `webui/routes/admin.rs`), only for `!merged` steps with a resolved `target_path` - a step
  that still depends on a copy or a merge has no local file for the math to
  examine, and the preview never materializes anything. Waiting steps show up
  as "projection after the copy". Tensor Parallelism is left out in this
  version, since the real split is `split-mode tensor`.
- **Analytical estimate.** The route reads `gguf::read_layout` and
  `gguf::read_info`, charges weights and KV with `plan::range_cost` and
  distributes the result in proportion to each GPU's current free VRAM
  (`split_projection`). Cache
  quantization counts: `cache_type_k`/`cache_type_v` come from the
  installation config and KV is only added to VRAM when `offload_kqv` is on.
  Otherwise the cache lives in host RAM and the projection notes the
  difference. The external MoE executor is left out because the route neither
  opens the model nor computes expert ownership. The compute buffer is also
  left out: the result is a planning signal, not a replacement for the real
  load.
- **Multi-GPU split.** Always proportional to current free VRAM. The answer
  marks the split as estimated and the real load stays the authority on
  effective layers, context and final layout. The route never calls
  `llama_backend_init`, `llama_rs_fit_params` or `fit_plan`, so it does not
  compete with the native load.
- **Three visible states, no blocking.** Consistent with the "memory is an
  estimate" rule (`a_target_short_on_memory_warns_and_still_deploys`):
  **green** fits the analytical projection; **amber** (`fits && degraded`) the
  raw math does not fit, but `fit_memory` is on and the layers were left at the
  default, so the real load can trim the request; **pink** (`!fits`) the math
  does not fit and no automatic fit is allowed. Amber is an explicit
  heuristic, not a claim about llama.cpp's result. All three states are
  warnings and the screen still asks for a `Confirm` before deploying.

  The endpoint admits four previews per process. When all slots are taken, it
  answers `429` immediately; the coordinator caps its fan-out at four
  simultaneous calls with `buffer_unordered`, without forming a native queue.

  Pink opens a `Confirm` naming the machines before deploying. `canDeploy` does
  not change - a coverage gap and `blocked` are still the only real blocks;
  capacity stays a warning.

`MachineView.capacity` (`MachineCapacity { fits, degraded, note, gpus }`) is the
per-GPU sum of that machine's steps - two non-adjacent steps on the same card
add up. `Step.capacity` (`CapacityPreview`) is the step alone.

## 4. Execution (`deploy_endpoint` -> `run`)

1. `deploy_endpoint` rescans (`survey`), replans, refuses `blocked`/empty,
   creates the job ("Deploy {deployment_name} on {n} nodes") and spawns the
   execution.
2. `run` builds the list of `Pull`s - one unit of movement per slice that has
   to reach a target. For pre-sliced TP, the unit is each `TpTarget` that does
   not have the rank on the target yet. For a merged step, it is one `Pull`
   per missing part; the machine "has" the merged unit when it already has all
   parts, and then the merge runs locally with nothing crossing the network. A
   rank or slice with nowhere to come from is an error.
3. The bytes travel over the transfer transport (see `file-transfer.md`): HTTP
   `Range` with resume through `.part`, MD5 checked before the file takes its
   final name, progress in the bell coming from the nodes (closing the tab
   interrupts nothing). For a part by hash, the receiver fetches and merges the
   `.pom` before the weight bytes. The deployment's disk check counts the
   **union** of the slices plus what is missing - the machine needs room for
   the joined model *and* for the parts. The slice folder limit tolerates child
   subdirectories the node's user cannot read: it sums what could be measured
   and logs each skipped entry at `debug`. If the root itself does not exist,
   is not a directory or cannot be read, `apply` fails with the path, the cause
   and advice to fix the folder or its permissions. Since the sum can
   underestimate bytes that became inaccessible, it only serves the folder's
   configured limit; physical capacity is still checked through the system's
   free space in `survey` and again before each copy in `transfer.rs`.
4. Each target receives the `ApplyRequest` through `apply`/`launch` (details in
   section 8).
5. `done` = "the model is serving on the network"; an error becomes a `failed`
   job.

## 5. Deploy routes

| Route | Role |
|---|---|
| `POST /api/ui/deploy/plan` | preview (read, called on every edit); enriches the steps with the capacity projection |
| `POST /api/ui/deploy/capacity-preview` | per-step preview, local to the peer: estimates from GGUF metadata and free VRAM, without calling the native fit, and returns the projected VRAM per GPU (`deploy/preview.rs`) |
| `POST /api/ui/deploy` | executes (rescans, refuses `blocked`/empty, job) |
| `POST /api/ui/deploy/node/options` | catalog and local options of the machine chosen for an incremental change |
| `POST /api/ui/deploy/node` | adds a machine to a pipeline already live |
| `POST /api/ui/deploy/node/remove` | takes a machine out of the model (frees the memory) |
| `POST /api/ui/undeploy` | takes the whole pipeline offline |

All of `/api/ui/deploy*` and `/api/ui/undeploy` are PrincipalOnly in the route
table; reading `GET /api/ui/deployments` is also open to `node`.

## 6. Adding and removing a machine

`add_node_options_endpoint` rescans only the real catalog of the selected
machine and returns the materialized slices it has, the matching whole model
when there is one and the local GPU/KV defaults. When the machine is changed
in the Dashboard, the screen redoes that query and does not reuse another
peer's options or the old plan while the inventory is still being read.

`add_node_endpoint` receives an option + target machine and calls
`webui::launch` with a single `LaunchTarget` (`skip_split: true` - the file
already exists and the step is only to load). `cache_type_k`/`cache_type_v` are
applied in the local `pom.json` or through the peer's API before the launch;
`n_gpu_layers` and `offload_kqv` go along as the machine's effective
overrides. `offload_kqv` does not select a GPU other than the weights': on, the
KV follows the layer's device; off, it goes to RAM. Details hidden in the
call:

- **`peer_index`/`peer_count`** - the position is the pipeline's, not "this
  node alone": without it the added node claims to be the last of the split.
- **`primary_node_id`** - the pipeline's entry does not change by adding a
  node: only whoever holds layer 0 receives the prompt. `primary` is only set
  when `start_layer == 0`.
- Answer `{applied, overlapping_layers, peer_index, peer_count}` - overlap is
  reported, not refused (the machine joins and the screen shows what
  collides).

When the request identifies a persisted deployment, the `add_node_endpoint`
answer also carries `deploy_id`, `version` and `kind`. Overlap is reported,
not refused. A whole machine and one or more slices can coexist in the same
deployment, which then uses `kind: "mixed"` and exposes both layer 0 heads as
possible entries for the same model.

`remove_node_endpoint` ("this machine is not serving this model" if the target
does not serve) computes `uncovered_after_removal` against the remaining
machines. **The memory is freed by its owner**: target == self uses
`runtime.stop()`; otherwise it forwards `POST /api/ui/unload` to the target's
gateway (the same route as the button that unloads one machine). Answer
`{removed, uncovered_layers}`; empty means another machine covers the same
range - the network loses a parallel route and keeps serving; non-empty is a
gap, and the confirmation is explicit. The answer also reports `deploy_id`,
`version` and `serving`. The removal appends a version with the target out of
the set. While a machine remains, `serving` stays true; the last removal writes
an empty active version, ends the selection as a live deployment and keeps the
whole history.

## 7. Undeploy

`POST /api/ui/undeploy` is the opposite of deploying, in a single act.
Removing machine by machine meant clicking in the right order hoping not to
forget one - and a forgotten one is RAM and VRAM stuck in a slice that serves
no purpose, because the pipeline only plans with closed coverage.

**What this does NOT do: touch the disk.** No slice is deleted. Undeploying
gives the memory back; the files stay where they are and the same deployment
comes back with a click instead of 17 minutes of transfer per machine. Deleting
slices is what the Models screen does, with its own confirmation, and it is a
different decision from this one.

- An empty `UndeployRequest.group` = "whichever pipeline is live"; two live
  pipelines is a 400 - never guess which.
- `undeploy_endpoint` rescans the cluster (never trusts the screen) and answers
  200 with one `UndeployOutcome { node_id, hostname, start_layer, end_layer,
  released, error }` per machine, always - including the one that did not
  answer: a silent peer is `released: false` with the reason in `error`, never
  counted as success. Half undeployed is the cluster's real state, and hiding
  it behind an error would leave the operator not knowing which machines still
  hold memory.
- Release goes through the peer's own `/api/ui/unload` (the same route as the
  one-machine button), which still works for any node.

## 7.1 Redeploying the same model, and a deployment's versions

A deployment is **versioned** (`Deployment`, `DEPLOYMENT_VERSION = 2`).
`versions` keeps every configuration that ever ran under the same `deploy_id`
and `active_version` says which one is live; `nodes` is the flattened list of
the active version. What tells a version apart is the whole deployment's
`RuntimeParams` - per-machine settings (`n_gpu_layers`/`offload_kqv`) enter the
version snapshot but do not fork a version. A legacy v1 record (without
`versions`) gets a synthesized v1 from `ensure_versions` on every read.

Incremental topology changes are history events independent of the
parameters: adding or removing a machine uses `append_version`, with a new
`active_version` and that version's set of `nodes`. The coordinator publishes
the snapshot to the current participants and to the peers that already knew the
deployment, so a removed machine clears its record without unloading or
restarting the others.

`POST /api/ui/deploy` runs a pre-check before copying any file or calling
`launch`. For slices, the proof is the manifest's `pom_id`; for a whole GGUF,
it is the SHA-256 computed on the selected file. The model name is never proof.
Old records without identity can only use the canonical path when the path
belongs to the same peer; without proof, the action is refused with the reason
visible and no machine is unloaded.

**Reusing the `deploy_id`.** Before minting a new id, `deploy_endpoint` looks in
the **local** `pom.db` for a deployment of the same **shape**
(`deployment::matching_shape`): same `kind`, same proven `DeploymentIdentity`
and the same set of `(node_id, range, tp_rank)`. If found:

- `params` equal to a recorded version's -> that version becomes active again,
  without appending another;
- different `params` -> **same `deploy_id`, new version** appended
  (`upsert_version`).

Without a proven identity, or with a different shape (a range or machine
changed), it mints a new `deploy_id` - changing machine or range is a Deploy
Studio matter, not a version. When the active deployment is provably the same,
the flow calls `/api/ui/unload` on all matching peers and polls `/api/ui/node`
until each one announces `config: null`, state `disabled` and no active model
or deployment. The wait is capped at 30 seconds per round; if a peer stays
busy, the job fails naming the machine and its last state. An unload failure
also blocks the new deployment and keeps the returned reason in the job. The
endpoint's answer carries `deploy_id` and `reused`.

### `POST /api/ui/deployments/:id/redeploy` (the "Deployments" screen)

`RedeployRequest` carries `params` (optional), `version` (optional) and
`per_node`. `version` redeploys an already recorded version, without appending
another; new `params` redeploy by appending - or reactivating - a version
through the same comparison as `deploy_endpoint`. Provide one of the two
(`redeploy_endpoint` refuses with 400 when both are missing or the `version`
does not exist). `RedeployRequest::resolve` mutates the record to reflect the
chosen version and returns the base parameters; the start uses the active
version's `node.config` (the parameters with the per-machine settings baked
in), so redeploying a version gives each machine exactly what it was running.

Same deployment, same `deploy_id`, same machines and ranges - **it does not
replan** (replanning would mint a new `deploy_id` and break clients pinned to
`model@deploy_id`). `run_redeploy`:

1. **Snapshots** each node's live `NodeConfig` (`model_path`, `model_label`,
   `deployment_name`, `no_embeddings`) - the record keeps range, position,
   `part_sha256` and `source_model_path` (the file that node loaded), but an
   old record may have no path at all.
2. **Unloads** every machine that still announces this `deploy_id`
   (`release_one` in parallel, the same `/api/ui/unload` as undeploy). An
   orphan machine that left the plan is also released here.
3. **Starts one at a time**, waiting for each one to report `ready` before the
   next (`wait_for_peer_serving_with_timeout`, 30 min ceiling): sequential on
   purpose, so VRAM peaks do not stack. Each node receives
   `/api/ui/reconfigure` with a `ReconfigureSlice` built from the snapshot +
   the record; `reconfigure_local` rebuilds the `NodeConfig` from it **whether
   or not the node serves something**. The file is resolved in three steps:
   the snapshotted `model_path` (or the record's `source_model_path`); the
   `part_sha256` (`transfer::find_part`); and, without a part hash - a whole
   model or a slice from an old round -, the local catalog matching model label
   + layer range (`resolve_deployment_slice_file`). A node that no longer has
   the file by any of the three returns an error naming the range, not a mute
   500.
4. Writes to the record only the parameters each machine accepted, syncs the
   active version's snapshot (`commit_active_nodes`) and publishes.

The job carries the per-machine result in `outputs` (`"<host>: <reason>"`),
which the Deployments screen shows below the count. Tensor Parallelism keeps
the old path (`reconfigure` in place, without unload): the collective does not
survive an unload and the record does not keep the whole `TpLaunch`. The
versions' `resolve` runs before the detour to the TP path, so versions apply
to both.

Known limit: `deploy_endpoint`'s shape search only looks at the local `pom.db`.
A coordinator that is not a participant of the existing deployment does not
find it and mints a new id; the "Deployments" screen, which joins the
cluster's records, does not have this limit.

## 8. Applying and serving a slice (`webui/`)

`POST /api/ui/apply` (applied by `deploy` to each target) carries an
`ApplyRequest`: `{ start_layer, end_layer, total_layers, peer_index,
peer_count, primary, skip_split, merge: Option<Vec<MergeSource>>, params }`.
The derived facts the machine needs to know:

- `is_first`/`is_last` - role in the pipeline (the tail has the output head).
- `is_last_peer`/`single_peer` - `single_peer` = `peer_count <= 1 && is_first
  && is_last`.
- `no_embeddings` - auto-detect: only the last peer of a multi-peer pipeline
  skips the embedding; a checkbox on screen forces it on all.

`run_apply` decides the path:

- `merge` -> `merge_parts` (temp with a unique suffix; rebuilds the per-layer
  metadata, validates the final GGUF with `read_layout` before the rename and
  `finalize_run`, reuses an already complete file and invalidates the
  catalog).
- `skip_split` or `(is_first && is_last && peer_count <= 1)` -> serves the file
  as is (the slice already exists on disk).
- otherwise -> `extract_shard` (local slicing, "slicing" job, progress through
  `set_job_progress`, `invalidate_catalog`, outcome `reused`/produced).

Once applied, the slice is recorded in `<config>.last-slice.json` as `pending`
by `runtime::apply`. The watcher only promotes it to `serving` after two
consecutive `/health` probes with `model_loaded`; a first answer does not
clear the journal, since a native crash can happen during that confirmation.
When the idle monitor unloads the model, it records `idle` and keeps the
deployment active so `reload_model` can restore it. The journal counts pending
attempts across processes and blocks automatic resume at
`MAX_AUTOMATIC_ATTEMPTS=10`. That state decides what a restart resumes
(`serving` or `idle`), not the `apply`. See `slicing.md` section 6 and the
`AGENTS.md` of `inference/`.

The `POST /api/ui/leave` path is a control operation independent of the model:
any profile can take its own machine off the network, even with an empty,
`Starting` or `Failed` runtime. It stops the runtime, clears the journal and
unannounces the node. That is different from `POST /api/ui/network/leave`,
which wipes the local trust and goes back to setup.

`/api/ui/plan` (`plan_split_endpoint`) plans by cluster budget and
**delegates** to the peer that owns the GGUF when `layout_node_id` points to
another machine: it clears the hop marker and calls `POST {base}/api/ui/plan` -
the peer that has the file computes how many layers fit in each one's memory.

### Releasing a slice

`POST /api/ui/release` is never refused (a machine can always leave the
model). With `reassign` (default), `find_release_candidate` finds another
machine with the same slice to take over; answer `{ reassigned_to }` or
`{ uncovered: true }` when nobody covers it - and the planner refuses a
pipeline with a gap, so the screen warns.

## 9. What the screen shows

- **Settings always open, on the right** - the twelve numbers and switches
  that govern the deployment stay visible next to the machine choice; picking
  machines with them hidden is picking blind.
- **Alternatives** - each range shows every possible machine as a target with
  the reason (`alternatives`), which makes changing a slice's owner one click.
- **The last deployment of each model is kept in `localStorage`** and comes
  back filled in, **after** the first new plan - the memory is from another
  day, and what does not survive the current plan (`usableSelection`) simply
  does not come back.
- **A merged unit shows as one row** naming the union (e.g. `m_0_15.gguf`) and
  only takes the `select` of the machines that do not have all the parts.

## 10. Tests

They use the `gateway` testkit (see `http-api.md`): `state_with` with the peer
table seeded by hand and an axum `fake_peer` on loopback (in the undeploy case,
counting the hits on `/api/ui/unload`). The deploy tests live in
`crates/node/src/backend/deploy/tests/`.

- **Plan**: `merge_joins_adjacent_slices_on_one_machine` (the merged machine's
  row names the union and transfers no bytes); a gap in the set is reported;
  overlapping slicings are not stacked by the planner; a set nobody has is
  refused.
- **Add/remove**: `split_position` (a machine in the middle is not the tail; at
  the end it is; swapping the same range keeps the count); overlap reported but
  not refused; a range that fills a gap overlaps nothing; partial overlap only
  reports the shared layers.
- **Per machine**: `per_machine_kv_cache_rides_along_with_the_slice` -
  `n_gpu_layers`/`offload_kqv` travel with the slice and do not become
  cluster-wide; `nvidia_memory_comes_from_vram_and_everything_else_from_ram`.
- **Versions** (`models/deployment.rs`):
  `ensure_versions_synthesizes_the_v1_of_a_legacy_record`,
  `upsert_version_reuses_the_version_with_equal_params_and_appends_when_they_differ`,
  `set_active_version_swaps_the_nodes_and_refuses_a_missing_version`,
  `matching_shape_matches_by_identity_and_ranges` +
  `matching_shape_ignores_a_record_without_identity`,
  `versions_survive_serde`. In `deploy/tests/deployments.rs`:
  `redeploy_request_resolve_picks_or_creates_a_version` and
  `redeploying_a_missing_version_is_refused`.

## 11. Defects noted

- **DEFECT NOTED: interference in the parallel run of the HTTP plan** -
  `deploy::whole_route_tests::the_route_plans_a_whole_model_from_the_machines_own_catalog`
  fails in the full parallel run, but passes alone and with
  `--test-threads=1`. The cause is pairing this route with
  `webui::the_machine_list_for_deploying_is_pinned_too` or
  `webui::cluster_models_talks_to_the_peers_through_the_pinned_client`: those tests
  change the global `peer_client::TLS_ENABLED`; the route does not hold
  `peer_client::GLOBAL_POLICY`, so it can build `https://` for the fake HTTP
  server and read the failure as a catalog without the model. The failure also
  happens on base `0adfa5b`, so it is not a regression of the slicer fixes.
