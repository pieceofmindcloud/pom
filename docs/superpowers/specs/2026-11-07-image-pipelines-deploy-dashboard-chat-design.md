# Image models as pipelines: Deploy, Dashboard and Chat

Status: design, awaiting the operator's approval. Follows
`2026-11-06-image-models-by-component-design.md` (delivered as plans A and B),
whose deploy screen the operator rejected: it is a separate form
(`/deploy/image`, `routes/ImageDeploy.tsx`) instead of the deploy studio, its
deployments do not show on the Dashboard, and a component runs on exactly one
machine.

## 1. Intent

An image model must be deployed, watched and undeployed the way an LLM is:

- **Deploy studio.** "Image model" is a mode of the Deploy screen. The model is
  drawn as a pipeline of three stages in data-flow order, **Text encoder ->
  Diffusion -> VAE**, and the admin drags machines from the bottom strip onto
  each stage, as LLM slices are placed today.
- **Redundancy.** A stage takes more than one machine. Every machine of a stage
  is a replica of that component and a valid route; a caller picks one by
  measured RTT and in-flight load, as the LLM router picks a stage's replica.
  The Deployment inspector shows per-machine properties and lets the admin add
  or remove replicas.
- **One machine, several stages.** A machine may serve more than one stage of
  the same model (the VAE is small and usually rides along with another stage).
- **Dashboard.** Image deployments appear in the model chip and on the Flow map
  as a three-stage pipeline with their machines, the RTTs between them and each
  replica's state; Add machine and Undeploy work on them from the Dashboard. The
  Deployments screen lists them with their versions.
- **Chat.** The Chat composer gains a "Generate image" option, like the RAG
  option of the Qdrant plugin. The user writes what they want; the prompt runs
  through a ComfyUI workflow served on the network, and the images the workflow
  produces appear in the conversation.
- The separate `/deploy/image` screen is removed.

## 2. Goals, non-goals, anti-goals

**Goals**

1. Image mode in the Deploy studio: model choice, a three-stage map, drag and
   drop of machines per stage, a verdict line, Deploy, the same progress window.
2. Replicas per stage, chosen by the caller per call (RTT, then in-flight load),
   with failover to the next replica when one does not answer.
3. Image deployments on the Dashboard (chip, Flow map, Details, Add machine,
   Undeploy, properties panel) and on the Deployments screen (versions,
   redeploy).
4. Chat image generation through published ComfyUI workflows, the images shown
   in the bubble and kept across reloads of the conversation.
5. Acceptance: Z-Image Turbo with the text encoder and the VAE on one machine
   and the diffusion model on two machines (two replicas); a Chat prompt
   produces an image; stopping one diffusion replica keeps generation working.

**Later (not blocked by this design)**

- Slicing the diffusion model across machines (block ranges).
- An LLM rewriting the user's request into a better image prompt before the
  workflow runs.
- Image editing in the Chat (an attached image as the workflow's input).

**Anti-goals**

- Breaking released contracts (root `AGENTS.md`, compatibility policy). The
  LLM `Deployment` record, `DeploymentKind`, `/v1/models` and the deploy
  routes stay as they are; everything new is an optional field, a new route or
  a new kind of service. An older node never fails to read anything an image
  deployment writes.
- An image stage in the LLM planner, in `/v1/models` or in a chat model list.
- A copy the user did not authorize: a machine is offered for a stage only when
  it holds the stage's file; copies go through the existing "Authorize the
  copies" flow.
- Generated images in the Chat's `localStorage` (a few images exceed its
  quota and break the whole history).
- A ComfyUI that stops working alone.

## 3. What exists (the base)

- `crates/node/src/components.rs`: assignments per node (`served_components`
  setting, `component.serve` / `component.release`), `deploy_image` /
  `undeploy_image` with one machine per component, records in the deploying
  node's `image_deployments` setting, `network_components` (every live peer's
  `image` service list), and `component_call`, which forwards an opaque body to
  the one machine serving a component.
- The ComfyUI plugin publishes `kind: "image"` with `components: [{deploy_id,
  name, component, files, status}]`, serves components (`pom_components.py`)
  and uses network components in ComfyUI's loaders (`pom_remote.py`, `POM ▸`).
- Deploy studio (`routes/DeployModel.tsx`, `components/DeployNetworkCanvas.tsx`,
  `ModelPlacement`), Dashboard (`routes/Dashboard.tsx`,
  `components/ModelNetworkCanvas.tsx`, `UndeployPipeline`, `AddNodeDialog`,
  `DeploymentInspector`), Deployments (`routes/Deployments.tsx`).
- Chat (`routes/Chat.tsx`, `lib/chatStore.ts`): history in `localStorage`, RAG
  switch driven by `GET /api/ui/chat/capabilities` (`rag`, `rag_collections`).

## 4. Decisions

| Question | Decision |
|---|---|
| Stage order | Data flow: Text encoder -> Diffusion -> VAE (operator). |
| Several stages on one machine | Allowed (operator). |
| Several machines on one stage | Allowed: replicas, routed per call (operator: redundancy). |
| Where the record lives | Its own record (section 5.1), not `Deployment`: a new `DeploymentKind` value would make older nodes fail to read the LLM records. The screens show both kinds side by side. |
| What the Dashboard trusts | The live services of the network (`network_components`), so every admin node sees every image deployment, not only the one that deployed it. |
| Chat images storage | Copied to the node that made the request and saved as `<workspace>/.comfyui/images/<sha256>.<png|jpg>`, so they survive the serving machine going away; the conversation keeps only the hash (operator, section 7.4). |
| Which workflows the Chat offers | Each image deployment's built-in "Text to image" workflow, plus workflows an admin publishes from ComfyUI (section 7.1). |

## 5. Backend

### 5.1 The image deployment record

The `image_deployments` record (deploying node's `pom.db` setting) becomes
versioned, keeping the old form readable:

```text
ImageDeployment {
  deploy_id, name, created_at, created_by,
  family: Option<String>,                // from the diffusion file, for defaults
  versions: [ImageDeploymentVersion],    // empty in an old record: synthesized on read
  active_version,
  components: [ComponentChoice],         // old field: the active version, flattened (read by older code)
}
ImageDeploymentVersion {
  version, created_at, created_by,
  stages: [ImageStage],                  // always in order: text_encoder, diffusion, vae
}
ImageStage {
  component,                             // "text_encoder" | "diffusion" | "vae"
  file: { name, size_bytes },            // the identity across machines (name + size)
  replicas: [{ node_id, hostname, path }],
}
```

A record without `versions` is migrated on load: one version whose stages hold
the single machine of each old `ComponentChoice`. `components` keeps being
written (first replica of each stage) so a node on the previous release still
lists it.

### 5.2 Planning and deploying

`POST /api/ui/image-deploy/plan` (new, any admin) takes `{name, stages:
[{component, file, node_ids: [..]}]}` and answers, without touching a machine:

- `stages`: per stage, per chosen machine, `ok` or the reason (does not run
  ComfyUI, does not hold the file, ComfyUI in `error`), plus `alternatives`:
  every machine of the network with its reason, as the LLM plan does, so the
  strip can grey out machines that cannot take a stage.
- `memory`: per machine, free VRAM/RAM now and the projection after loading
  (the sum of the files it would load, a rough need, as section 5.4 of the
  previous design).
- `blocked`: a stage with no machine, a file of the wrong category (a
  `checkpoint` fits all three), a missing file.
- `copies`: when a chosen machine lacks the file and another holds it; the
  deploy then needs `allow_transfers`, unchanged.

`POST /api/ui/image-deploy` gains the same `stages` form (the old
`components` form keeps working: one replica per stage). It applies every
(stage, machine) pair with `POST /api/ui/components/apply`, now allowing several
components of one deployment on one machine (the assignment key is
`(deploy_id, component)`, already so). It reports progress through the
existing deploy job and bell (`Job` with `deploy_id`), so the studio's progress
window works unchanged. A replica that fails is reported and released; the
deploy fails only when a stage is left with no replica.

### 5.3 Changing a live deployment

- `POST /api/ui/image-deploy/:deploy_id/add` `{component, node_id}` applies one
  replica and appends a version.
- `POST /api/ui/image-deploy/:deploy_id/remove` `{component, node_id}` releases
  it; removing the last replica of a stage needs `confirm_gap: true` and leaves
  the deployment not serving until a replica comes back.
- `POST /api/ui/image-deploy/:deploy_id/redeploy` `{version}` re-applies a
  version.
- `POST /api/ui/image-undeploy` `{deploy_id}` (exists) also works on an admin
  node that holds no record: it releases every machine that lists the
  `deploy_id` in its `image` service. Like the LLM undeploy, it answers one
  outcome per machine, a silent one included.

### 5.4 The live view

`GET /api/ui/image-deployments` returns, for every `deploy_id` seen in the
network's `image` services or in local records:

```text
{ deploy_id, name, family, recorded: bool, active_version, versions?,
  stages: [{ component, file, replicas: [{node_id, hostname, status, in_flight}] }],
  serving: bool }        // every stage has a ready replica
```

`in_flight` comes from the plugin's service entry (new optional field, below).
The Dashboard and the Deployments screen read this route; `GET /api/ui/cluster`
is not changed.

### 5.5 Routing a call to a replica

`components_for_call` keeps every replica of a component instead of one. For
each call the node orders the ready replicas by measured RTT to this node
(the same RTT table the Flow map shows), then by `in_flight`, and posts to the
first; a connection failure or a 5xx moves to the next replica, once per
replica. A 4xx is the plugin's answer and is returned. The diffusion stage is
called once per sampling step: replica choice is sticky per ComfyUI execution
(`x-pom-execution` header from the plugin) so the steps of one image stay on
the replica that has the weights warm.

### 5.6 Plugin contract additions (ComfyUI)

- Service entry: optional `in_flight` per component, and optional `workflows`
  (section 7.1).
- `component.serve` may arrive for several components of one deployment on one
  machine; the plugin already keys by `(deploy_id, component)`.
- New op `workflow.run` (section 7.2).

## 6. Screens

### 6.1 Deploy studio

The mode bar gains **Image model** beside Shards and Whole model (only when
some machine publishes an `image` service). In image mode:

- **Top bar:** the diffusion file (the network's `diffusion` and `checkpoint`
  image files), then the text encoder and VAE files, prefilled from the same
  folder when it holds exactly one of each, and the deployment's name.
- **Map:** three stage columns in flow order, each with its file, size and the
  machines dropped on it. A machine card shows its free VRAM/RAM and the
  projection, as on the LLM map. The lines between stages carry the measured
  RTT between the machines of consecutive stages.
- **Strip:** the network's machines; dragging one onto a stage adds a replica.
  A machine that cannot take a stage is greyed on hover with the plan's reason.
  The same machine can be dropped on several stages.
- **Inspector:** Global (name, versions note) and a machine tab with that
  machine's stages, files, free memory and a Remove button.
- **Verdict line and Deploy:** from `image-deploy/plan`; copies open the
  existing authorization window.

The implementation reuses `DeployNetworkCanvas` and the strip of the LLM mode,
with a stage-shaped layout instead of a layer ribbon; the builder decides how
much is shared.

### 6.2 Dashboard

- The model chip lists image deployments after the LLMs, marked "Image".
- Flow: the three stages as columns of machine cards (the existing card), the
  replicas of a stage stacked, RTT on the lines, a ComfyUI "client" end in
  place of the gateway. Coverage reads "3 of 3 stages" instead of layers.
- Details: the machines of the deployment, each with its stages.
- Add machine: the dialog asks for the stage and offers the machines that hold
  its file. Undeploy: `image-undeploy`, the list of machines shown first.
- Properties panel: the read-only inspector of section 6.1.

### 6.3 Deployments screen

Image deployments are listed with an "Image" badge, their versions (the stages
and replicas of each) and Redeploy.

### 6.4 Removed

`routes/ImageDeploy.tsx`, its route and its link in the mode bar; the guides
lose the "Image models" form section and describe the studio mode instead.

## 7. Chat image generation

### 7.1 Which workflows are offered

The ComfyUI plugin lists in its `image` service:

```text
workflows: [{ id, name, deploy_id?, published_by?, inputs: ["prompt", "negative"?, "width"?, "height"?, "seed"?] }]
```

- **Built-in:** for every image deployment it can reach, the plugin offers a
  "Text to image" workflow wired to that deployment's network components
  (text encoder -> sampler on the diffusion model -> VAE decode -> image),
  with defaults per family (steps, CFG, sampler, size; Z-Image Turbo: 8 steps,
  CFG 1). It runs on any machine whose ComfyUI is installed.
- **Published:** an admin opens a workflow in ComfyUI and uses "Publish to
  Chat" (POM menu), naming it and choosing the text node that receives the
  user's prompt (and optionally the negative prompt, size and seed nodes). The
  plugin stores it beside ComfyUI's user data and lists it. A published
  workflow runs on the machine that published it, since it may use local files.

`GET /api/ui/chat/capabilities` gains `image: "supported" | "unavailable"` and
`image_workflows: [{id, name, node_id}]`, gathered from the network's `image`
services; only workflows whose deployment is serving (built-in) or whose machine
is ready (published) are listed.

### 7.2 Running one

`POST /api/ui/chat/image` `{workflow_id, node_id, prompt, negative?, width?,
height?, seed?, count?}` (any profile allowed to chat; the image budget of plan
B applies). The node forwards it to that machine's plugin op `workflow.run`
(through the signed peer call for another machine). The plugin queues the
workflow in its ComfyUI with the inputs substituted, streams progress
(`{"p": step, "of": steps}`) and ends with the images themselves (`{"images":
[{mime, width, height, data}]}`, PNG or JPEG bytes, base64 in the frame) or
`{"error"}`. The requesting node stores them (section 7.4) before answering;
its route answers as NDJSON so the bubble can show progress, ending with
`{"images": [{hash, ext, width, height}]}`. The request log records it as an
image request (plan B).

### 7.3 The Chat

- A **Generate image** switch in the composer's options, shown when
  `image === "supported"`, with the workflow picker beside it (the built-in
  ones first). On, the message goes to `/api/ui/chat/image` instead of the LLM;
  the conversation keeps the choice, like the RAG switch.
- The reply bubble shows the progress, then the images in a grid; a click opens
  one full size with Download. Failure shows the plugin's reason.

### 7.4 Keeping the images

The images are copied to the node that made the request, so they are not lost
when the machine that ran the workflow goes offline, deletes its outputs or
leaves the network:

- The requesting node writes each image to
  `<workspace>/.comfyui/images/<sha256>.<ext>`, where `<sha256>` is the hex
  digest of the file's bytes and `<ext>` is `png` or `jpg` from its content
  (magic bytes, never the plugin's word). The same image twice is one file.
  `<workspace>` is the node's `workspace_root`; a node without one uses its
  project root. The folder is created when missing, whether or not ComfyUI is
  installed on that node.
- Limits: at most 16 images per request and 32 MiB per image; a larger or
  unrecognized file is refused with the reason and nothing is written.
- `GET /api/ui/chat/image/:hash` serves the file from that folder (same
  authorization as the Chat; the hash is checked to be 64 hex digits and the
  extension looked up, so no path is taken from the request).
- The `ChatMessage` stores only `images: [{hash, ext, width, height}]`. A
  conversation opened in another browser of the same node shows them; opened
  through another node, the bubble says the image lives on the node that
  generated it. When the file is missing the bubble says so instead of a
  broken image.
- The machine that ran the workflow keeps ComfyUI's own output as ComfyUI
  does; POM deletes nothing in this phase, and the guide says where the copies
  live.

## 8. Compatibility

- Old `image_deployments` records load and are migrated in memory; the old
  `components` field keeps being written.
- `POST /api/ui/image-deploy` keeps accepting `components`.
- An older node that reads the `image` services ignores `in_flight` and
  `workflows`; a node on this release talking to an older plugin sees no
  workflows and no in-flight count and routes by RTT alone.
- `chat/capabilities` only gains fields.

## 9. Tests (what proves it)

- Backend: record migration; plan reasons (no ComfyUI, missing file, wrong
  category, empty stage, copy needed); deploy with replicas and several stages
  per machine; a replica failure releases only it; add/remove/redeploy append
  versions; undeploy from a node with no record; replica ordering by RTT then
  in-flight; failover on connection error and 5xx but not 4xx; sticky replica
  per execution; chat capabilities listing; `chat/image` forwarding and NDJSON.
- Plugin: built-in workflow per family; publish and list; `workflow.run` input
  substitution, progress, output files; `in_flight` counting.
- UI: image mode drag and drop per stage and the same machine on two stages;
  verdict line; Dashboard chip, Flow stages, Add and Undeploy; Deployments
  badge and versions; Chat switch, picker, progress, image grid, missing image.
- Images: the copy lands in `.comfyui/images/<sha256>.<ext>` on the requesting
  node, the extension follows the bytes, a duplicate is one file, a bad hash or
  oversized image is refused.
- Live acceptance of goal 5 on the operator's cluster.

## 10. Out of scope

Diffusion slicing, LLM prompt rewriting, image-to-image in the Chat, deleting
old generated images, image models on the public `/v1` API.
