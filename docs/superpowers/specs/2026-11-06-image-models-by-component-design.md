# Image models on the POM network, split by component

Status: design, awaiting the operator's approval. Phase 2 of the ComfyUI
plugin (`pom-plugin-comfyui`); phase 1 (ComfyUI inside POM, POM's LLMs as
ComfyUI nodes) is delivered.

## 1. Intent

POM serves LLMs from GGUF files across the machines of its network. Image
models (Z-Image, Qwen Image, Flux, SDXL...) are not served at all: POM's
catalog sees only GGUF, and ComfyUI, inside POM since phase 1, runs every model
on the GPU of the machine it was opened on, from its own private folder.

The operator wants image models to be a first-class kind of model in POM:

- POM finds and shows them (`.safetensors` and the other formats ComfyUI
  reads) next to the GGUF files, in the same model folders, with what each
  file is: a diffusion model, a text encoder, a VAE, and so on.
- An admin deploys an image model on the network **by component**: the text
  encoder on one machine, the diffusion model on another, the VAE on a third,
  chosen on the deploy screen.
- ComfyUI on any machine of the network **sees the components served on the
  network and uses them** in its ordinary nodes, without the weights being on
  its own machine. The ComfyUI on each serving machine is the engine that runs
  its component.
- Files ComfyUI downloads land in POM's model folders, not in a folder only
  ComfyUI knows.

Why: the network's GPUs are spread over machines that are each too small for a
whole modern image model plus the LLMs they already serve. Splitting by
component lets a 16 GB card hold the diffusion model while the text encoder
(often 8 GB or more) and the VAE live elsewhere, the same reason POM slices
LLMs.

## 2. Goals, non-goals, anti-goals

**Goals (this design)**

1. The catalog lists image model files from the configured model folders, with
   a category read from the file's content, and the Full models screen shows
   them.
2. ComfyUI's downloads go into a POM model folder, and ComfyUI's loaders list
   every image file POM's catalog knows on that machine.
3. A deployment kind for image models whose units are components, each on one
   machine the admin chose on the deploy screen, served by the ComfyUI plugin
   on that machine.
4. ComfyUI's ordinary loader nodes list the components served on the network
   and run them remotely, so ComfyUI's own workflows and templates work by
   picking a network component.
5. First acceptance: Z-Image Turbo (text encoder `qwen_3_4b`, diffusion
   `z_image_turbo_bf16`, VAE `ae`), one component per machine, generating an
   image from a ComfyUI opened on a fourth or any of those machines.

**Later (the design must not block them)**

- Slicing the diffusion model itself across machines (block ranges, like LLM
  layer slices). The operator wants it; it is deliberately deferred.
- Downloading image models from the Find models screen (POM's Hugging Face
  provider is GGUF only today).
- GGUF image models (for example GGUF quantizations of Flux) classified as
  image models instead of LLMs.
- LoRAs and other patches applied to a remote diffusion model.

**Anti-goals (a failure even if it works)**

- **A copy the user did not authorize.** The deploy screen offers, for each
  component, the machines that already hold its file. A copy to another
  machine happens only through the existing "Authorize the copies" flow
  (`allow_transfers`), never automatically.
- Breaking a released contract: the catalog JSON, `host.configure`, the
  plugin `engine.*` operations, the announced capabilities, `/v1/models` and
  the deploy routes only gain optional fields and new values (root
  `AGENTS.md`, compatibility policy). Older nodes and plugins keep working and
  simply do not see image models.
- An image component planned as an LLM pipeline hop, or listed by
  `/v1/models` as a chat model.
- Any change to the llama.cpp fork.
- A ComfyUI that stops working on its own: local models in `.comfyui/models`
  and ComfyUI's own workflows keep working with no network.

## 3. What exists today (the base)

- **Model folders.** `NodeConfig::model_dir_paths()` (`model_dirs` in
  `pom.json`, `POM_MODEL_DIRS` overrides) plus the slice folder. On the
  operator's machines these are the "full models" folders on two disks, laid
  out by publisher and repository (`<root>/<publisher>/<repo>/<file>`).
- **Catalog.** `crates/node/src/models/catalog.rs`: `discover_fast` lists GGUF
  files with `stat` only, `enrich` reads GGUF headers in the background,
  `Catalog { models, shard_sets, scanned, sidecars }`, served by
  `GET /api/ui/models` and read by the deploy survey on every machine.
- **Plugin inference services.** `docs/subsystems/inference.md` section 7.1
  and `deployment.md` section 1.6: `RuntimeParams.inference_service`, the
  plugin's `engine.load / status / upstream / unload`, the runtime supervisor
  on the target (`plugin_engine::run`), the node announcing
  `engine/plugin-v1:<code>`, signed node-to-node routes that forward to the
  plugin's loopback server (`/api/internal/plugins/:code/engine/...`), and
  `plugin_stage_link` carrying binary frames between two engines. All of it is
  shaped around text generation and layer ranges.
- **Copies.** `deployment.md` section 4: a plan's `copies` need
  `allow_transfers`, which the deploy screen sends only after its "Authorize
  the copies" window. Transfers never cross a public relay.
- **Plugin to node.** `host.configure` (`workspace_root`, `gateway`,
  `host_api`) and the loopback `host_api` routes
  (`crates/node/src/plugin_host_api.rs`: node, peers, a JSON peer call with a
  15 s timeout, sign).
- **ComfyUI plugin (phase 1).** `pom-plugin-comfyui`: installs ComfyUI 0.43.0
  in its own venv, runs it behind `runtime/bridge.py` on loopback, writes
  `runtime/pom_nodes.py` into `custom_nodes/pom_nodes` on every start, and
  already takes over the missing-model download button so files download on
  the server (`window.__comfyDesktop2.downloadModel`). ComfyUI's base folder
  is `<workspace>/.comfyui`.

## 4. Decisions the operator made

| Question | Decision |
|---|---|
| What "split" means | By component (text encoder, diffusion, VAE on different machines). Slicing the diffusion model across machines too, but later. |
| How ComfyUI uses network models | Transparently: ComfyUI's ordinary nodes list the network's models and use them. |
| Who places components | The admin, on the deploy screen. |
| A machine without the file | The screen offers only machines that already hold the file; a copy happens only with the user's explicit authorization. |
| Where ComfyUI downloads | Into POM's model folders, not `.comfyui/models`. |
| First model | Z-Image Turbo, one component per machine (proposed by the designer, accepted). |

## 5. Design

### 5.1 Catalog: image model files

A new optional field on `Catalog`, so older screens and nodes ignore it:

```text
Catalog.image_files: Vec<ImageModelFile>
ImageModelFile {
  id, name,                 // name: "<publisher>/<repo>/<file>" relative to its root, like GGUF entries
  path, size_bytes, source, source_label,
  format,                   // "safetensors" | "ckpt" | "pt" | "pth" | "bin" | "sft"
  category,                 // see below
  family: Option<String>,   // best-effort model family ("flux", "qwen_image", "z_image", "sdxl"...), for display and matching only
  dtype: Option<String>,    // dominant tensor dtype ("bf16", "fp8_e4m3fn", "int8"...)
  metadata_error: Option<String>,
}
```

- **Discovery** joins the existing two phases: `discover_fast` also lists the
  image extensions by name and `stat`; `enrich` reads the header. A
  safetensors header is 8 bytes of length plus a JSON table of tensor names,
  dtypes and shapes, read without the weights (cap the header read at 100 MiB,
  refuse larger as a `metadata_error`). Pickle formats (`.ckpt`, `.pt`,
  `.pth`, `.bin`) are never unpickled by POM: they are listed with
  `category: other` unless their name says otherwise (builder's call), since
  reading them safely is not worth it for this phase.
- **Category from content**, by tensor-name patterns, in this order:
  - `checkpoint` - one file holding a diffusion model, a VAE and a text
    encoder (the SD 1.5 / SDXL single-file form: `model.diffusion_model.*`,
    `first_stage_model.*`, `cond_stage_model.*` or `conditioner.*`).
  - `lora` - `lora_up`/`lora_down`, `lora_A`/`lora_B`, `.alpha` patterns.
  - `controlnet` - `control_model.*`, `input_hint_block`, `controlnet_*`.
  - `vae` - an encoder/decoder pair of conv blocks without attention
    transformer blocks (`decoder.up_blocks`, `decoder.up.*`, `encoder.down.*`,
    `post_quant_conv`).
  - `diffusion` - transformer or UNet denoisers (`double_blocks`,
    `single_blocks`, `joint_blocks`, `transformer_blocks`, `layers.N.adaLN*`,
    `input_blocks`/`output_blocks`, `time_embed`, optionally under
    `model.diffusion_model.`).
  - `text_encoder` - language or CLIP text towers (`model.layers.N.self_attn`,
    `encoder.block.N` (T5), `text_model.encoder`, `transformer.resblocks`).
  - `clip_vision` - `vision_model.*` without a text tower.
  - `upscale` - ESRGAN-style (`conv_first`, `model.0.weight` chains) and
    similar.
  - `other` - anything unrecognized. Never hidden.

  The builder owns the exact patterns and must test them against real headers
  of at least the families in section 7. `__metadata__` keys
  (`modelspec.architecture`) may refine `family` but never override a
  content-based `category`.
- **Identity across machines** (needed by the deploy survey to know who holds
  "the same file"): file name plus size, the same light identity the sidecars
  use (`SidecarFile`). No content hash in the scan.
- The record in `pom.db` that keeps the GGUF catalog is extended or paralleled
  for image files only if the Full models screen needs history; otherwise the
  in-memory catalog is enough (builder's call).

### 5.2 Full models screen

The Full models screen (`/models`, guide `docs/guides/screens/models.md`)
gains image files: a type switch or tab ("LLM (GGUF)" / "Image") and, for
image rows, the category, family, dtype, size and source. Send (to another
machine, through Transfers) and Delete work for image files as for GGUF;
Slice does not apply. The screen's guide is updated in both languages, with a
new screenshot.

### 5.3 ComfyUI and POM's model folders

- **The plugin learns the folders.** `host.configure` gains an optional
  `model_dirs: [abs path]` (the node's `model_dir_paths()`, without the slice
  folder). A plugin that does not read it is unaffected.
- **Downloads.** The server-side download of phase 1 writes into the first
  model folder with enough free space for the file, under
  `<root>/<publisher>/<repo>/<file>` taken from the Hugging Face URL (Civitai:
  `civitai/<model id>/<file>`), the same layout the operator's folders already
  use. The completion message shows the full path. With no `model_dirs` (an
  older node), it falls back to `.comfyui/models/<folder>` as today. After a
  download the plugin asks the node to rescan (a new `host_api` route,
  `POST /api/plugin-host/catalog/refresh`, or the catalog's existing
  invalidation; builder's call) so the file reaches the screen and the survey
  without waiting for the 20 s TTL.
- **ComfyUI's loaders see POM's files.** A new `host_api` route,
  `GET /api/plugin-host/catalog/images`, returns this node's `image_files`
  (path, name, category). The plugin maps each category onto ComfyUI's folder
  names (`diffusion` -> `diffusion_models`, `text_encoder` -> `text_encoders`,
  `vae` -> `vae`, `lora` -> `loras`, `checkpoint` -> `checkpoints`,
  `controlnet` -> `controlnet`, `clip_vision` -> `clip_vision`, `upscale` ->
  `upscale_models`) and exposes the files there. Constraint for the builder:
  every ComfyUI loader, including third-party custom nodes, must see them, and
  `.comfyui/models` keeps working beside them. A per-category folder of links
  rebuilt at start and after each refresh (`.comfyui/models/<folder>/pom/...`)
  meets that on Linux and macOS; on Windows, where symbolic links may need
  privileges, the builder picks the fallback (registering the files through
  `folder_paths` from `pom_nodes`).
- **The three Z-Image files already downloaded** into `.comfyui/models` are
  moved into a POM model folder during the rollout, with the operator's
  confirmation, not by code.

### 5.4 Deploying an image model

**What a deployment is.** An image deployment is a named set of components,
each one file (or, for a text encoder that needs two or three files, one group
of files) on exactly one machine:

```text
DeploymentKind::Components            // new value
RuntimeParams.inference_service = "comfyui"
components: [
  { component: "text_encoder", files: [<file identity>...], node_id },
  { component: "diffusion",    files: [<file identity>],    node_id },
  { component: "vae",          files: [<file identity>],    node_id },
]
```

A `checkpoint` file may supply all three components; each machine that serves
a component of it loads only that component (and needs the whole file on its
disk). The name defaults to the diffusion file's stem and is editable.
Versions, the job, the bell progress, undeploy and redeploy follow the
existing whole-model path (`deployment.md` 1.1, 7, 7.1); a machine removed
from the deployment releases its component.

**The service.** The ComfyUI plugin publishes a second service in
`host.services`, beside its `app`:

```json
{"id": "comfyui", "kind": "image", "name": "ComfyUI", "status": "...",
 "components": ["diffusion", "text_encoder", "vae"]}
```

A new `kind: "image"` (not `inference`), so no LLM deploy screen, planner or
RAG lookup ever picks it up by accident. The screen offers image deployment
only when some machine publishes it, and for each component only machines
that publish it.

**The deploy screen.** A new model type on the deploy screen, "Image model
(by component)", next to the LLM types. The admin:

1. picks the diffusion file (or a checkpoint) from the network's image files
   (the survey now also carries each machine's `image_files`);
2. picks the text encoder file(s) and the VAE file, pre-filled when the same
   repository folder holds exactly one of each (a suggestion, never binding);
3. for each component, picks one machine from those that hold its file and
   publish the `image` service; the free VRAM/RAM of each machine is shown
   next to it, as on the LLM map, with the file's size as the rough need;
4. optionally picks a machine that does not hold the file: the plan then lists
   the copy, and deploy opens the existing "Authorize the copies" window;
   without that authorization nothing is copied and the deploy is refused
   (409 `transfers_not_authorized`, unchanged).

The builder decides the exact layout; the guide `deploy.md` (both languages)
gains the image path.

**On the target.** The runtime supervisor runs the plugin's component the way
it runs a plugin engine (bind, `engine.load`, `engine.status` every 2 s,
restart budget, journal, `engine.unload` on undeploy), with new fields in
`engine.load.params`: `component`, `model_paths` (one or more), and the
`gpus`/`gpu_devices` it already sends. A component runtime has no layer range,
no `n_ctx` and no sessions; `/health` reports `model_loaded` when the plugin
says `ready`, and `capabilities` carries
`engine/component-v1:<code>:<component>` plus the deployment's id. The node
announces that capability **instead of** the hidden-state ones (the same rule
as `Network::plugin_engine_capabilities`), so the LLM planner never plans it,
and `/v1/models` leaves component deployments out. Whether this is a mode of
`plugin_engine::run` or a sibling module is the builder's call; the
supervisor, journal and idle-unload machinery must be reused, not copied.

**Idle unloading.** `unload_if_idle` applies per component as for LLMs; a call
to an idle component wakes it (the caller waits, then gets the answer).

### 5.5 Calls between ComfyUI and a remote component

**Path of a call.** ComfyUI (the "client", on any machine) -> its own node's
`host_api` -> the serving node, over the signed node-to-node transport ->
that node's plugin loopback server -> its ComfyUI. Nothing listens beyond
loopback; the client never learns another machine's address or token.

- `GET /api/plugin-host/components` (new `host_api` route): the live component
  deployments on the network: `[{deploy_id, name, component, files: [name],
  node_id, hostname, status}]`, built from the announces. Served components
  only, plus those that are `idle`.
- `POST /api/plugin-host/component/:deploy_id/:component/:op` (new `host_api`
  route): forwards an opaque body (binary, up to 64 MiB, like the stage link
  frames) to the machine that serves that component and returns its answer
  body. The node reads only the envelope. Timeout long enough for a cold load
  of a large component (builder's call, minutes, not the 15 s of the JSON peer
  call); progress of a cold load is reported to the client as a status the
  plugin can show.
- `POST /api/internal/plugins/:code/component/:deploy_id/:component/:op`
  (new node-to-node route): signed control envelope from a live member, checks
  that this node serves that component of that deployment, takes the plugin's
  loopback server from `engine.upstream` and forwards. Same authorization as
  the existing `.../engine/generate` route.

**What crosses.** The plugin owns the body format of each `op`; POM never
parses it. The intended ops and their size, for a 1024x1024 image:

| Component | Op | In | Out |
|---|---|---|---|
| text encoder | `encode` | prompt text, encoder type and options | conditioning tensors (a few MB) |
| diffusion | `load` | the client's loader options (weight dtype...) | the model's sampling configuration, so the client builds ComfyUI's `model_sampling` locally |
| diffusion | `step` | latent (about 1 MB), sigma, conditioning by reference, options | model output (about 1 MB) |
| diffusion | `cond` | conditioning tensors, once per prompt | a reference reused by every step |
| VAE | `decode` / `encode` | latent / image | image / latent |

The sampler (KSampler and friends) stays on the client: only the model's
forward pass runs remotely, once per step and per CFG branch. About 2 MB per
step crosses, so even a 1 Gbit link adds little to a step that takes hundreds
of milliseconds on the GPU.

**In the client ComfyUI** (`pom_nodes`):

- ComfyUI's ordinary loaders - Load Diffusion Model (`UNETLoader`), Load CLIP
  (`CLIPLoader`, and the dual/triple variants when one component holds the
  group), Load VAE (`VAELoader`), Load Checkpoint - list the network's
  components after the local files, as `POM ▸ <file name>` (the deployment's
  name in the tooltip when the node API allows).
- Choosing one returns a stand-in object of the type the loader returns
  (`MODEL`, `CLIP`, `VAE`) whose work goes through the calls above: a
  `CLIP` whose encode is remote, a `MODEL` whose diffusion forward is remote
  and whose sampling configuration came from `load`, a `VAE` whose decode and
  encode are remote. The rest of the workflow (samplers, schedulers, latent
  nodes, image nodes) runs unchanged.
- The loader options the workflow chose (the CLIP `type`, the UNet
  `weight_dtype`) travel with the calls, and the serving ComfyUI loads its
  file with them, keeping one loaded copy per distinct set of options.
- A remote component that is down, idle-loading, or refusing shows a ComfyUI
  error naming the component and the machine; nothing silently falls back to a
  local file.
- How the stand-ins hook into ComfyUI (wrapping the loader nodes' functions,
  extending `folder_paths` lists) is the builder's call, pinned to the ComfyUI
  release the plugin installs and covered by tests that run that release.
  Patches applied to a remote `MODEL` (LoRA, model patches) are refused with a
  clear error in this phase.

**In the serving ComfyUI** (the same plugin, in its component role): the
bridge exposes `/pom/component/...` on its loopback port behind its token;
each `op` runs on ComfyUI's own model code and memory management, on the GPU
the node assigned (`gpus`/`gpu_devices` -> ComfyUI's device flags). Requests
for one component are queued on that machine; the GPU is shared with the
interactive ComfyUI on the same machine and with whatever else POM serves
there.

### 5.6 Leaving room for diffusion slicing

Not built now, but the design must not block it:

- `component` is an open string; a later `diffusion` slice can carry an
  optional block range, the way LLM slices carry a layer range, with the
  stages exchanging their hidden states through the existing
  `plugin_stage_link` relay (binary frames, signed, never over a public relay).
- The `step` op's body is the plugin's; a sliced diffusion model keeps the same
  client-facing ops, with the first stage answering and the stages chained
  behind it.

## 6. Constraints

- Root `AGENTS.md` invariants: compatibility policy (optional fields and new
  values only), roles enforced by nodes (an inference-only node refuses to
  serve a component as it refuses a slice; a visitor is never a target),
  mutual TLS between nodes, no file over a public relay, no product name
  literal outside `brand.json`.
- The plugin `query` answers within 3 s; loads and calls run on the plugin's
  own threads and servers.
- Secrets (the plugin token, `host_api` token, gateway key) never reach the
  browser, a workflow file, a log or an RPC answer.
- Documentation duty: `catalog`, `deployment`, `inference` (or a new
  subsystem file for components), `http-api`, `plugins-ui`, the folder
  `AGENTS.md` files and the Full models and Deploy guides (both languages),
  in the same commits as the code. No machine names in docs.
- Route tests use `gateway::testkit`, never real UDP.

## 7. Verification

- Catalog: unit tests of the category rules against real safetensors headers
  (header bytes only, checked into test fixtures) of at least Z-Image, Qwen
  Image, Flux, SDXL (checkpoint and split), SD 1.5 checkpoint, a LoRA, a
  ControlNet, a CLIP vision and an upscaler.
- Deploy: survey, plan and deploy tests for a component deployment, including
  the refusal without `allow_transfers` when a component's machine lacks the
  file, and the announce rule (a component node is never planned as an LLM
  hop, never in `/v1/models`).
- Plugin: contract tests for the new service and `engine.load` fields; a live
  test, like phase 1's, that runs two ComfyUI instances on one machine (one as
  the server of a small component, one as the client) through a fake node
  forwarder.
- Acceptance, on the operator's three-node machine (one NVIDIA 16 GB card and
  one AMD card shared by the nodes): Z-Image Turbo with the diffusion model
  on the NVIDIA card, the text encoder and the VAE on the other nodes (the
  AMD card, or CPU if ROCm cannot run it), an image generated from a ComfyUI
  that holds none of the three files, and the same seed giving the same image
  as the all-local run within tolerance.

## 8. Left to the builder

Marked above as builder's calls; collected here: the exact category patterns;
pickle-format handling; whether image files get a `pom.db` record; the
download rescan mechanism; how POM's files appear in ComfyUI's folders on each
OS; the deploy screen layout; whether the component runtime is a mode of
`plugin_engine` or a sibling; the timeout and cold-load progress of component
calls; the body format of each op; how the stand-ins hook into ComfyUI.

## 9. Out of scope

Diffusion slicing across machines; downloading image models from Find models;
GGUF image models; LoRAs on remote models; video and audio models; changes to
the llama.cpp fork; automatic copies.
