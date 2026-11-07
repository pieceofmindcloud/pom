# Deploying a layer range of the whole model, without slicing

Date: 2026-11-07 - Status: proposal for review

## Problem

A pipeline stage today is a **slice**: a GGUF file the machine cuts from its
own copy of the model (`docs/subsystems/slicing.md`). The slice carries only
its layers' tensors, renumbered to local indices, the embedding on the first
slice and the head on the last, and `pom.start_layer`/`pom.end_layer`/total in
its header. The engines then load the slice as an ordinary model:

- llama.cpp loads it in-process (`llama-cpp-sys-2`, the fork pinned through
  `pom-plugin-llama.cpp`) and runs it as a partial model.
- Strata (`pom-plugin-strata`) reads the slice's `pom.*` metadata
  (`gguf::inspect`), **packs** it into its own format
  (`tools/iq_pack.py`, `tools/dsv41/pack_gguf.py`, cached under
  `<POM database dir>/strata/packs/`) and runs the pack as one stage of its
  stage link (`src/program/stage_link.hpp` in the Strata fork).

Costs of that design:

- A slicing pass (and, on Strata, a pack pass) every time the cuts change.
- Disk: a node keeps the whole model **and** every slice it was cut, and
  Strata a pack per slice (about 1.5 GB each for the IQ3_XXS model).
- Cuts are effectively fixed at deploy time. Rebalancing when a machine joins
  or the context size changes means new files.

The memory-aware planner mostly exists: `plan_split_with_kv` (`plan.rs`) costs
each range as real weight bytes plus KV for `n_ctx * max_sessions` against each
peer's budget, and hybrid layouts only accept the cuts
`valid_prefix_end_layers` returns. What stands between the plan and loading is
the slice file.

## Goals

- A node that holds the whole model serves any **layer range** of it, chosen
  at deploy time, with no file written.
- **No model bytes travel between nodes.** A worker gets a model by a
  download the admin starts on it, or by the operator placing the file in its
  model folder. The file transfer feature is retired (section "Retiring file
  transfers").
- One contract in POM for both engines: a deployment names the whole model
  plus a range; the engine says whether it can load ranges, which cuts are
  valid and what a range costs in memory.
- The admin's deploy screen proposes ranges that use each machine's free VRAM,
  from live telemetry and every deployment parameter that costs memory.
- Slices stop being something the operator handles. A node may still cut one
  from its **own** copy as an internal fallback (decision 2), never sent
  anywhere.

## Non-goals

- Tensor parallelism shards (`tp_parts`): they cut **inside** tensors and
  keep slicing. A later spec may load row/column shards from the whole file.
- Changing the hidden-state wire, the stage link or the planner's pipeline
  order rules (`deployment.md`).
- Download sources other than Hugging Face (a later spec).

## Decisions

### 1. A deployment step is a range of a source

A step in the deploy plan, the last-slice journal and `/health` gains a second
form beside the slice:

```json
{ "source": { "pom_id": "<sha256 of the whole model>", "path": "<local path>" },
  "range": { "start": 0, "end": 24, "total": 48 } }
```

`pom_id` is already the model's identity in the slice manifest (the sha256 of
the logical source; for multipart, of the ordered parts), so the range form
names the same model the slices of it name. Readers keep accepting the slice
form; a node writes the range form only when it deployed by range. Persisted
files (`<config>.last-slice.json`, manifests) migrate on load per the
compatibility policy; nothing existing is rewritten.

### 2. Capability and fallback

A node announces `model/range-load-v1` when its engine for that deployment can
load ranges, and the deploy flow uses the range form only when:

- the node holds the whole model (by `pom_id`), and
- its engine reports range loading for the model's architecture
  (decision 3), and
- the cut is one the engine reports as valid.

Otherwise the node slices its **own** whole copy locally, as an internal step
of the deploy, and loads the slice; nothing is transferred. An older node
never announces the capability and gets today's slice flow, with no copies
(see "Retiring file transfers", compatibility).

Some architectures need tensors outside the layer range on every stage (on
`deepseek4`, llama.cpp requires the output head even on a non-final instance,
and the slicer copies it into each range today, `deployment.md` 1.1.1). The
range loader loads whatever the architecture requires from the same file, so
that copy disappears.

### 3. What an engine reports

For llama.cpp, POM calls the binding directly; for a plugin engine, through the
`engine.*` contract (`docs/subsystems/inference.md`, plugin engines). Both
report the same three things:

- **Range support** per architecture. Plugin engines add `"stage_ranges":
  [<architectures>]` to the describe block next to `stages` and
  `stage_architectures`. For llama.cpp, every architecture the slicer
  supports, on an engine build whose fork has external tensor data (Part A).
- **Valid cuts**: the layer ends a range may stop at. For llama.cpp, today's
  `valid_prefix_end_layers` (every whole layer for dense, architectural
  boundaries for hybrid). For a plugin, new `engine.cuts {model_path}` ->
  `{total, ends: [..]}` (for DeepSeek-V4.1 on Strata: `[20, total]`).
- **Memory estimate** of a range: new `engine.estimate {model_path, range,
  n_ctx, max_sessions, kv, settings}` -> `{weights_bytes, kv_bytes,
  compute_bytes, other_bytes}` per device, plus `cpu_offload` (whether the
  engine can run layers on the CPU) and, for a range that does not fit in
  VRAM, `{gpu_layers, cpu_layers, ram_bytes}` (see "Overcommit"). For llama.cpp, POM keeps computing
  it itself from the tensor table and KV geometry (`plan_split_with_kv`),
  extended with the compute buffer and the MTP reservation. Plugins answer
  for their own format; POM never applies GGUF math to a packed engine.

All three are optional: an engine that answers none of them keeps slicing,
with POM's current estimates.

### 4. `engine.load` with a range

A plugin engine's `engine.load` gains an optional `range: {start, end,
total}`. When present, `model_path` is the **whole model** (first shard for
multipart) and the engine loads only that range. When absent, today's
behavior: `model_path` is a slice or a whole model, and the engine reads the
range from `pom.*` metadata. The `link` fields are unchanged; the stage link
does not care where the weights came from.

### 5. The planner uses live free memory

The deploy screen gains **Auto-balance**: it gathers each candidate node's
free VRAM (and RAM for CPU layers) from telemetry, asks each engine for the
cost of candidate ranges, and solves for contiguous ranges in layer order that
fit every node with a margin, putting layer 0 on the node with the most room
(it processes every prompt and has the largest compute buffer). The operator
still drags the cuts, pins layer 0 and sees per node: weights, KV, compute,
headroom. The inputs are the deployment's real settings: `n_ctx`,
`max_sessions`, K/V cache types, flash attention, MTP and its context
reservation, prefill chunk size, and the engine's own settings.

Changing ranges on a node that loads by range is a reload with new
parameters, not a new file.

## Engine parts

### Part A: llama.cpp

Decided on 2026-11-07 during implementation: a **virtual slice**, not range
parameters in the loader. The slicer's header rewrite (block count, per-layer
arrays, counts from the start or the end, layer selectors, recurrent layers
from the `ssm_conv1d` tensors present, PLE and DeepSeek-V4.1 keys, required
heads, the identity `output_norm` of a non-final range) is the architecture
knowledge; reimplementing it in the C++ loader would keep two copies of the
rules.

- **POM** (`crates/node/src/models/splitter.rs`):
  `write_virtual_slice` writes the same header `split` writes, plus
  `general.external_data.files` (the source file or files, absolute paths),
  `general.external_data.tensor_files` (an `i32` per tensor: the file, or -1
  when the bytes are in the header) and `general.external_data.tensor_offsets`
  (a `u64` per tensor: absolute offset in that file). Tensors with data in the
  header (the injected identity `output_norm`) come first in the table. The
  file is the size of the metadata (about 11 MB for a 27B model, mostly the
  tokenizer) and holds no weights. It is valid only while the source files
  stay where they are, unchanged.
- **Fork** (`pom-external-data`: the fork's `pom` plus
  commit `bd4b9586`; `pom` itself is unchanged until this is validated): one block in `src/llama-model-loader.cpp`. With the three
  keys, the loader opens the listed files (relative to the header's folder
  unless absolute), appends them to `files`, and gives each external tensor its
  file index and offset, with bounds checked against that file. mmap, direct
  I/O and lazy reads already work per file index. Split GGUFs with external
  data are refused. Not POM specific; it could be proposed upstream.
- The in-memory model is the slice's by construction (the same header), so the
  slice runtime (hidden state in and out, KV for local layers, MTP) is reused
  unchanged, for every architecture the slicer supports.
- **Verified** on Qwen3.8-27B (IQ3_S), CPU: a header-only copy of the whole
  model and the original give identical greedy output; the real and virtual
  slices of layers 0-15 (with the internal identity `output_norm`) and of
  48-63 (last, with the MTP layer, on the fork's `pom` branch) give identical
  output. `splitter::tests::a_virtual_slice_points_at_the_same_bytes_the_real_slice_copies`
  checks every tensor's bytes on a synthetic model.
- **MoE executor**: the expert readers in `llama-engine` map the model file
  themselves. On the engine branch `pom-external-data` (`pom-plugin-llama.cpp`)
  they open it through `moe::format_q4_0::map_tensor_source`, which follows a
  virtual slice into its one source file with absolute offsets; a MoE source in
  several files stays on real slicing.

POM wiring (phase 2, first part done): `run_apply`, where a node holding the
whole model cut its range locally, writes the virtual slice instead when
`POM_VIRTUAL_SLICES=1`, unless the model is a mixture of experts split in
several files. The header
lives under `<project root>/virtual-slices/`, named by the source's path, size,
modification time and the range, and is rewritten on every apply. The engine
loads it as a slice and `/health` reports the range from its `pom.*` keys.
Verified live on one CPU node with Qwen3.8-27B: layers 0-31 and 32-64 (last,
MTP layer) each written in about 15 ms, loaded, and run by the slice speed
test. Not verified yet: a two-node pipeline generating end to end, GPUs.
The range step form exists as `whole_ranges` in the deploy request
(`deployment.md` 1.1.1): each machine is applied with its own whole file and
its range (`Step::from_whole_path`), with no cut on another machine and no
copy. A node with `POM_VIRTUAL_SLICES=1` announces `model/range-load-v1`
(`pipeline::CAP_RANGE_LOAD`) and the planner then charges it no disk for such
a range. The capability follows the opt-in rather than a check of the build:
a node cannot tell whether its llama.cpp loads external tensor data (a
prebuilt fork library cannot be inspected), so the operator sets it on builds
that do. Deploy Studio sends `whole_ranges` from its "Split the layers over N
machines" action and per-range - / + controls (first part of the Deploy
Studio section); locks, Auto-balance, several ranges per machine and the
overcommit display are still to do. The POM branch pins the engine branch `pom-external-data` (`dc9e951`),
whose `llama-cpp.lock` points at the fork branch of the same name.

### Part B: Strata

Strata never runs the GGUF: it runs a pack. The range form is:

- **One whole-model pack per machine.** The whole-model pack path already
  exists; a range deployment reuses it instead of packing a slice.
- **Strata fork: a stage from a range of a whole pack** (`--layer-start`,
  `--layer-end` or the fork's equivalent), loading only those layers, the
  embedding on the first stage and the head on the last.
- **Plugin** (`pom-plugin-strata`, `src/engine.rs`): `stage_ranges` in the
  describe block, `engine.cuts`, `engine.estimate`, and `engine.load` with
  `range` starting Strata on the whole pack with the range.
- Strata's valid cuts are architectural: DeepSeek-V4.1-Flash only at layer 20
  today; Flash-Next per its layout. Qwen3.6 stays whole-model only.

Before Part B ships: the split output of DeepSeek-V4.1 must be compared with
the whole model (the plugin README notes it has not been), and the stage link
tested on Metal if Macs are to take part.


## Retiring file transfers

### What transfers carry today

`crates/node/src/backend/transfer.rs`, `docs/subsystems/file-transfer.md`:

| Use | After this spec |
|---|---|
| Slices (`parts`) to a node that lacks them, by deploy or by hand | Gone: a node loads a range of its own whole copy |
| A whole model to a node that lacks it (deploy copy, `received/`) | Replaced by a remote download started by the admin, or manual placement |
| Sidecars: vision projector to the head, MTP draft to the tail (`deploy::attach_sidecars`, `pulls`) | Same as a model: downloaded or placed on the machines that need them |
| Tensor parallel shards (`tp_parts`) | Each rank cuts its own shard from its own whole copy; no transfer |
| "Send a file" panel and the slice lists of Shards / Nodes | Gone |

So every use has a replacement. What is lost:

- **Air-gapped or offline networks**: with no internet on the workers, the
  only way in is manual placement (a disk, a share, `scp`). The LAN copy
  between peers was the convenient path there.
- **Models with no download source**: private fine-tunes, files only in a
  local LM Studio / Ollama store. Same answer: manual placement, or the
  operator publishing them to a private Hugging Face repository.
- **Disk**: every serving node keeps the whole model (already a cost of
  range loading).

Decision: retire transfers. Offline networks are served by manual placement
plus a rescan; a later spec may add other download sources (an S3/HTTP URL, a
private registry) through the same remote download.

### Remote download (new)

The admin starts a model download **on another machine**:

- `POST /api/ui/model-provider/downloads` gains an optional `node_id`. When it
  names another node, the admin's node forwards the call signed by the admin
  (the route is already ServingMember: an admin of the chain may call it on a
  worker; a visitor never holds models and refuses). The worker downloads with
  its **own** Hugging Face token setting and into its own models folder.
- The same for cancel, retry and dismiss, and a read of the worker's download
  list (`GET .../downloads?node_id=`), so progress shows in the admin's bell
  and screens, as deploy jobs do now.
- Sidecars: the download takes a list of files of a repository (the GGUF, its
  `mmproj`, an MTP draft), so one action fetches what a deployment needs.
- An inference-only node refuses, as it refuses transfers today (the node's
  role is enforced by the node).
- **Manual placement**: the operator copies the file into a model folder of
  the worker (`ModelPaths`, scanned roots) and asks for a rescan, from the
  admin's screen too (`POST /api/ui/models/rescan` with `node_id`, forwarded
  the same way).

Identity: the deploy flow matches models across machines by `pom_id` (sha256
of the logical source), never by file name. A worker that downloaded the same
repository revision, or got the same file placed by hand, has the same
`pom_id`. The hash is computed once per file and cached with the catalog
(size + mtime), since a full read of a large model takes minutes.

### What is removed

Backend: `transfer.rs` and every `/api/transfer/*` route, the offer and grant
flow, the `received/` landing folder, `allow_transfers`, `transfer_bytes` and
the copy phase of `lifecycle::run`, the `pulls` of sidecars, the
`whole_slice` path (`slice-ranges` on a source, then a pull) and the
`model-layer-options` source/target pair (it becomes a read on the target's
own copy). The splitter stays, as the local fallback of decision 2 and for
tensor parallel shards.

UI: the Transfers screen becomes **Downloads** (model downloads on every
machine, by machine); `SendFile.tsx`, `Shards.tsx` ("Fatias"),
`SliceModels.tsx` ("Fatiar") and the Nodes tab's slice difference list go
away. Slicing for tensor parallel (dev mode) moves into the deploy flow as a
local step.

Invariants in the root `AGENTS.md` that change: "No weight travels the network
because of slicing" becomes "No weight travels between nodes", and the
transfer/relay rules of `file-transfer.md` are retired with the subsystem.

### Compatibility

The `/api/transfer/*` routes and the transfer capability are a contract
between released nodes, so removal is staged:

1. This release: new deployments never plan copies; the copy phase only runs
   for an older admin deploying to this node. The serving side of
   `/api/transfer/*` stays, marked deprecated. The UI no longer offers sending.
2. Next release: the routes and `transfer.rs` are removed. Written in the
   release notes: an older admin can no longer copy files to a current node.

Manifests and slices already on disk keep loading; a slice set whose whole
model is not on the machine keeps serving as it does today until it is
undeployed, and the screen says it is a legacy deployment.

## Deploy Studio changes

`DeployModel.tsx` and `../components/DeploymentInspector`
(`apps/frontend/src/routes/AGENTS.md`).

**Mode bar.** "A set of shards" and "a whole model" merge into one
**Pipeline** mode that starts from a whole model. Tensor parallel (dev mode)
and the plugin inference service stay. A plan with `kind: "shards"` from an
older node is still read (`kindOf`) and shown as a legacy deployment.

**Model picker.** Lists whole models known on any machine (by `pom_id`), with
how many machines hold each. The slice-set picker (`group`) goes away.

**Machine cards on the map.** Each card shows whether the machine holds the
selected model and its sidecars:

- holds it: can take a range;
- missing: **Download here** (starts the remote download of the same
  repository revision and files) or "Place the file in a model folder, then
  Rescan"; the card cannot take a range until the model is there;
- downloading: progress on the card; the plan waits.

**Ranges.** The coverage ribbon becomes the editor: draggable cuts that snap
only to the engine's valid cuts (decision 3), ranges in pipeline order, layer
0 pinnable to a machine. **Auto-balance** fills the ranges from live free
memory (decision 5). A replica is the same range on a second machine, chosen
on its card.

**Manual control per machine.** The operator decides how many layers each
machine serves, not only Auto-balance:

- **Layer count with − / +.** Each card shows its layer count with − and +
  buttons. A click moves the card's cut by one valid step (one layer in a
  dense layout, the next architectural boundary in a hybrid one), taking the
  layers from the next machine in the pipeline, or from the previous one for
  the last machine. More precise than dragging on a model with many layers;
  dragging stays.
- **Lock.** A lock on a card keeps its manual layer count. Auto-balance then
  spreads only the remaining layers over the unlocked machines, so it never
  undoes a manual edit; with every card locked it has nothing to do and says
  so.
- **Several ranges on one machine.** "Add range" on a card gives that machine
  a second (or third) range, as POM already allows ("one node may serve
  several ranges", root `AGENTS.md`), for example a machine at both ends of
  the pipeline. With range loading both come from the same file. Pipeline
  order still follows layer order, and `merge` still only joins adjacent
  ranges on the same machine. Each range has its own − / + and lock; removing
  a range gives its layers to its neighbours.
- **Limits explained.** When − or + cannot move, the button says why: no valid
  cut there, the neighbour would be left with no layers (remove it from the
  map instead), or the engine cannot spill to the CPU (below). Memory is
  **not** a limit for these buttons: see overcommit.

**Overcommit: more layers than the VRAM holds.** The admin may give a machine
more layers than its GPUs hold. The machine then puts as many layers as fit
in VRAM and runs the rest on the CPU from system RAM. It is never the
default and the system steers away from it, but on hardware without enough
VRAM it is the only way to deploy, so it is allowed:

- **What exists today.** llama.cpp already does it: `fit_memory` (on by
  default) lowers the GPU layer count before loading, an explicit
  `n_gpu_layers` sets it, and the Dashboard warns when layers ended up on the
  CPU (Flow and Details cards). The capacity preview already has green /
  amber / pink states that warn and never block (`deployment.md`, 3.1).
- **The estimate shows the split.** `engine.estimate` (decision 3) gains, for
  a range that does not fit, `{gpu_layers, cpu_layers, ram_bytes}`: how many
  layers would stay on each device and the RAM the CPU part needs. The card
  shows it as "18 on GPU, 6 on CPU" next to the memory bars.
- **Never chosen by Auto-balance.** Auto-balance only proposes ranges that fit
  in VRAM. When the model does not fit on the machines on the map even at
  their best, it says so, proposes the least-bad overcommit (the CPU layers on
  the machines with the most RAM and fastest CPUs) and marks it as such; the
  admin applies it or adds machines.
- **The warning.** A card with CPU layers is amber with "N layers on the CPU:
  generation will be much slower". The deploy button then asks for an
  explicit confirmation naming each machine and its CPU layer count (the
  existing pink `Confirm`, with the CPU split added). After the deploy the
  Dashboard warning stays, as today.
- **Hard limits.** System RAM is the real ceiling: a range whose CPU part does
  not fit in RAM either is refused by name. An engine that cannot run layers
  on the CPU says so in `engine.estimate` (`cpu_offload: false`); its + stops
  at the VRAM limit with that reason. Strata answers it per backend; a Mac's
  unified memory has no separate CPU spill and is costed as one pool.
- **Applying it.** For llama.cpp the target gets `n_gpu_layers` equal to the
  estimate's `gpu_layers` (the existing per-machine field of
  `SelectedSlice`), so the result does not depend on the automatic fit
  guessing the same number. A plugin engine receives the same value in
  `engine.load` as an optional `gpu_layers`.

**Memory per machine.** Each card shows weights, KV, compute and headroom for
its range at the current settings, from the engine's estimate, and turns
amber when a setting change (context, sessions, KV types, MTP) pushes layers
to the CPU, or red when the CPU part no longer fits in RAM; Auto-balance can
be run again.

**Removed from the studio.** The copy list and its `allow_transfers`
confirmation, transfer sizes, the "slices available" lists, the
`whole_slice` slider that cut on a source machine, and the notes that advise
sending a file through Transfers.

**Sidecars.** The vision projector and MTP draft choices list compatible files
by `pom_id`/name; a machine that needs one and lacks it shows the same
Download here / Rescan actions.

**Validation messages.** A plan is refused by name when a machine lacks the
model or a sidecar, when a cut is not valid for the engine, or when a range
does not fit even with its CPU part in RAM; never because a copy would be
needed. A range that fits only with CPU layers is a warning with a
confirmation, not a refusal.

Guides: the deploy screen guide, the Transfers guide (replaced by Downloads),
the slicing and shards guides (removed), and the concepts guide on how models
reach machines.

## Trade-offs

Gains:

- No slicing or packing pass when cuts change; rebalancing is a reload.
- No duplicate files: one model (and on Strata one pack) per machine.
- Planning becomes numbers only, solved by the admin from live memory.
- On Strata the saving is larger: one pack instead of one per slice.

Costs and risks:

- Every node serving by range keeps the **whole** model on disk (and, on
  Strata, a whole pack). For large models that is more disk per node than a
  slice; slices stay for small-disk nodes.
- On llama.cpp the rules stay in the slicer (Part A). On Strata, the fork must
  index per-layer metadata correctly for each architecture it splits, unless
  its pack tool reads the same virtual header. Gated by equivalence tests.
- Engine estimates can be wrong; the plan keeps a margin and the load's
  real fit (`/health` fit reports, the existing fit fallback) still wins.

## Compatibility

Additions only: an optional step form, a capability, optional describe fields
and optional `engine.*` operations and arguments. Older nodes and plugins keep
slicing; a mixed network deploys by range where every condition of decision 2
holds and by slice elsewhere. Persisted slice journals and manifests keep
loading.

## Testing

- **Equivalence (gate for each architecture)**: loading range R of the whole
  file gives the same tensors (names, shapes, types, bytes) and the same
  logits for a fixed prompt as loading the slice of R, for first, middle and
  last ranges, on llama, qwen2/qwen3, mistral, gemma, then hybrid and MoE
  families; the same for Strata on DeepSeek-V4.1 and Flash-Next.
- **Contract**: `gateway::testkit` deployments mixing range and slice steps;
  a node without the capability gets a slice; an invalid cut is refused.
- **Persistence**: old last-slice journals load; a range journal survives a
  restart and returns to `serving` through the `/health` watcher only.
- **Planner**: estimates against measured `/health` memory on real loads;
  Auto-balance never proposes an invalid cut or overflows a node, keeps
  locked cards, and proposes overcommit only when nothing fits in VRAM.
- **Overcommit**: a range larger than the VRAM deploys with the estimated
  `gpu_layers`, the rest runs on the CPU, the warning and confirmation show,
  and the Dashboard reports the CPU layers; a range whose CPU part exceeds the
  RAM is refused; an engine with `cpu_offload: false` stops at the VRAM.
- **UI**: − / + move by valid cuts and explain each limit; a second range on
  one machine keeps pipeline order; removing a range hands its layers on.
- **Live**: two nodes in isolated containers deploying by range, rebalanced
  without new files.

## Open questions

- Should a node that holds both the whole model and a matching slice prefer
  the range (less disk churn) or the slice (proven path) during the rollout?
- Is manual placement enough for air-gapped customers, or does a peer copy
  of whole files need to survive as an opt-in for them?
- Should a node offer to delete legacy slices once nothing deployed uses
  them?
- Should Auto-balance run again on its own when a node joins, or only
  propose and wait for the admin?
- Does Strata's whole pack fit every machine's disk where its slices did?

## Phases

1. Virtual slices: the fork's external tensor data and `write_virtual_slice`,
   with the equivalence test (done for Qwen3.8-27B, see Part A).
2. POM: step form, capability, journal, fallback to slicing; llama.cpp only.
3. Planner and Auto-balance with live memory and the full parameter set.
4. Hybrid and MoE families on llama.cpp.
5. Strata: validation of the existing split, fork range loading from a whole
   pack, plugin operations.
6. Remote download and rescan on another machine; Downloads screen.
7. Deploy Studio: single Pipeline mode, machine cards with Download here,
   range editor, memory per machine; removal of the slice screens.
8. Retire transfers: first release stops planning copies and offering sends
   (routes kept, deprecated); the next removes `transfer.rs` and the routes.
9. Docs: `slicing.md`, `deployment.md`, `inference.md` (plugin engine
   contract), `plugins-ui.md` (describe fields), `file-transfer.md` (retired),
   `http-api.md`, the root `AGENTS.md` invariants, the guides, and the
   `pom-inference-engine` skill's contract and feature levels.

## Out of scope

- Tensor parallelism shards from the whole file.
- Engines other than llama.cpp and Strata (they keep slicing until they
  implement the contract).
- Moving a running stage's range without a reload.
