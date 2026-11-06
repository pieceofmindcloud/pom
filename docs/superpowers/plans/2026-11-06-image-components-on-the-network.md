# Image model components on the network - Implementation Plan (Plan B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An admin deploys an image model by component (text encoder, diffusion model, VAE), each on a machine that holds its file, and ComfyUI on any machine of the network uses those components through its ordinary loader nodes; every image generation is recorded in the request log with its telemetry and counted against an image budget of the logs plugin.

**Architecture:** POM gains a small component subsystem beside the LLM runtime: each node keeps the components it was asked to serve (in `pom.db`), hands them to its ComfyUI plugin (`component.serve`), and the plugin's service lists them (`host.services`, kind `image`). A ComfyUI that wants a network component asks its own node over `host_api`; the node finds the serving machine from the peers' services and forwards an opaque body over the signed node-to-node transport to that machine's plugin, whose ComfyUI runs the op on its own GPU. In the client ComfyUI, stand-in `MODEL`/`CLIP`/`VAE` objects replace the weights: the real ComfyUI model classes are built without weights and only the diffusion network's forward pass, the text encoding and the VAE decode/encode cross the network.

**Tech Stack:** Rust (axum) in `pom-jair` (`crates/node`), React + TypeScript + Vitest in `apps/frontend`; Rust + Python 3.12 inside ComfyUI 0.43.0 (`comfy`, `safetensors.torch`, aiohttp) in `pom-plugin-comfyui`.

**Spec:** `docs/superpowers/specs/2026-11-06-image-models-by-component-design.md` (sections 5.4 to 5.6). Plan A (`2026-11-06-image-catalog-and-comfyui-folders.md`) is done: `Catalog.image_files`, `host_api` catalog routes, downloads and links in ComfyUI.

**Repositories:** Tasks 1-6 and 11 in `pom-jair` (branch `jair`), tasks 7-10 and 12 in `pom-plugin-comfyui` (`main`), tasks 13-15 in `pom-plugin-logs` (`main`), task 17 is the acceptance run on the operator's cluster and needs the operator's go. Order: 1-6, 7-10, 11, 12, 13-15, 16, 17. Task 16 spans `pom-jair` and `pom-plugin-comfyui`.

## Request log and image budget (decided with the operator)

- One audit record per ComfyUI generation (a ComfyUI prompt execution that used at least one network component), not one per call. The record is opened by the node where that ComfyUI runs, on the generation's first component call, and closed when ComfyUI reports the execution's end (success, error or interruption); a record left open 30 minutes is closed `incomplete`.
- Caller: `Caller { key_id: "plugin:<code>", key_label: "ComfyUI", key_node_id: Some(<this node>) }`, so the generation belongs to the machine the person uses, like the node's own chat key.
- Telemetry: `{"kind": "image", "images": <n>, "width", "height", "steps", "components": [{"component", "deploy_id", "name", "node_id", "hostname", "calls", "ms", "bytes_in", "bytes_out"}], "total_ms"}`; `hops` are the serving hostnames in first-call order; `source` is `components:<hostname>,<hostname>,...`; `request_details.kind = "image"`; route `/comfyui/image`.
- Budget: the operator chose **a budget of its own for images** (images per window), set apart from the token limits on the same plans. An image generation spends only the image budget; an image limit of 0 means no limit, as for tokens. ComfyUI's LLM nodes keep using `/v1` and the token budget.

## Departures from the spec (decided while planning, from the code)

- **Components do not use the node's LLM runtime.** A node has one `NodeRuntime` (one `NodeConfig`); running a component through `plugin_engine::run` would unload the LLM it serves. Components are a registry of their own (`components.rs`), so a machine serves an LLM and image components at once. The spec's "reuse the supervisor" cannot hold; idle unloading of components is left to ComfyUI's own memory management.
- **No copies in this plan.** The deploy screen offers, for each component, only machines whose catalog holds the file (the operator's rule); a machine without it is not offered. Sending image files between machines (and therefore `allow_transfers` for components) is a later plan.
- **The plugin is opened on demand.** Serving a component opens the ComfyUI plugin on that node (`PluginManager::open_on_demand`), and the node reopens it and re-serves its components at boot. Closing ComfyUI with its X stops that machine's components until it is reopened.
- **Image deployments are listed on their own screen section**, not in the LLM Deployments history (no versions; deploy and undeploy only).

## Global Constraints

- Compatibility: only optional fields, new routes and a new service kind. `PluginService.components` is `#[serde(default, skip_serializing_if = "Vec::is_empty")]`; nodes and plugins without it keep working and never see image components.
- Component names: `diffusion`, `text_encoder`, `vae` (an open string on the wire; only these three are served in this plan).
- Service kind: `image`; capability-free (no `/health` or announce change). LLM planning, `/v1/models` and RAG never read services of kind `image`.
- Node-to-node bodies are opaque and at most `MAX_COMPONENT_BODY = 64 * 1024 * 1024` bytes; POM never parses them.
- Op bodies (plugin and ComfyUI side): a safetensors file (`safetensors.torch.save` / `load`) whose tensors carry the data and whose `__metadata__` has one key, `args`, a JSON string with every non-tensor value. Never pickle.
- An inference-only node (`visitor`) refuses to serve a component, as it refuses `apply`; a member that is not an admin cannot deploy.
- Secrets (plugin tokens, `host_api` token) never reach the browser, a log or an RPC answer.
- Docs in English, no machine names, no em dash; each task updates the `AGENTS.md`/subsystem/guide text its code changes.
- pom-jair verify: `cargo test -p node --bin pom -- <filters>` with `CARGO_TARGET_DIR` on the root disk (the projects disk is nearly full), `rustfmt --edition 2021 <touched new files>`, no new clippy warnings in touched code (the repo-wide fmt/clippy already fail on develop); frontend `npm test -- <files> && npm run build`. Plugin verify: `scripts/build-ui.sh && POM_UI_REQUIRE_DIST=1 cargo test --locked && cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && tests/test_ci_plan.sh && python3 -m unittest discover -s tests -p 'test_*.py'`.

## Review Focus

1. The serving machine goes away mid-generation (stopped, ComfyUI closed): the client's sampler must fail with a ComfyUI error naming the component and the machine, never hang. -> Task 4 test `a_vanished_server_is_a_named_error`, Task 9 test `test_remote_failure_names_the_machine`.
2. Two components of the same deployment on one machine, or two deployments of the same file: calls must reach the right one (keyed by `deploy_id` and component), and undeploying one must not drop the other. -> Task 2 test `two_components_on_one_node_are_kept_apart`.
3. A node restarts: what it served before is served again without the admin redeploying. -> Task 2 test `served_components_survive_a_restart`.
4. A huge or hostile body on the internal route (over 64 MiB, or for a component the node does not serve): refused before reaching the plugin. -> Task 4 tests `oversized_bodies_are_refused` and `unknown_components_are_refused`.
5. A LoRA or other patch applied to a remote diffusion model: a clear error, never a silent no-op that produces images without the LoRA. -> Task 9 test `test_patches_on_a_remote_model_are_refused`.
6. A generation ComfyUI never reports as finished (ComfyUI killed, browser closed mid-run): its record must not stay "in progress" forever nor be charged twice. -> Task 11 test `an_unfinished_generation_is_closed_incomplete`, Task 13 test `an_image_generation_is_charged_once`.

---

### Task 1: `PluginService.components` and the network's served components

**Files:**
- Modify: `crates/node/src/plugin_services.rs` (struct, parser, tests)
- Create: `crates/node/src/components.rs` (declared in `main.rs` next to `plugin_services`)
- Modify: `docs/subsystems/plugins-ui.md` (service fields), `crates/node/src/AGENTS.md` if it lists top-level modules

**Interfaces:**
- Produces:
  - `pub(crate) struct ServedComponent { pub deploy_id: String, pub name: String, pub component: String, pub files: Vec<String> /* file names */, pub status: String /* "ready" | "loading" | "error" */ }` (`Serialize, Deserialize, Clone, Debug, PartialEq`), and `PluginService.components: Vec<ServedComponent>`.
  - `pub(crate) struct NetworkComponent { pub deploy_id, pub name, pub component, pub files: Vec<String>, pub status, pub node_id: String, pub hostname: String, pub plugin_code: String }`.
  - `pub(crate) async fn network_components(state: &GatewayState) -> Vec<NetworkComponent>`: this node's services plus every live peer's (`plugin_engine::services_of`), only services of kind `image`, sorted by name then component then hostname.

- [ ] **Step 1: Write the failing tests:** `components_are_parsed_from_an_image_service` (a `host.services` answer with a kind `image` service carrying two components -> two `ServedComponent`s; a service without the field -> empty) and `network_components_list_only_image_services` (testkit state; local plugin services stubbed through the existing services cache test hook if present, otherwise a pure helper `components_of(node_id, hostname, services: &[PluginService]) -> Vec<NetworkComponent>` tested directly; an `inference` service with a `components` field is ignored).
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test -p node --bin pom -- components_are_parsed network_components` Expected: compile error.
- [ ] **Step 3: Implement** the field, the parser path and `components.rs` with `components_of` + `network_components`.
- [ ] **Step 4: Run to verify they pass,** plus `cargo test -p node --bin pom -- plugin_services` (no regressions).
- [ ] **Step 5: Commit** (`components: image services publish the components they serve`).

### Task 2: Serving a component on a node

**Files:**
- Modify: `crates/node/src/components.rs`
- Modify: `crates/node/src/backend/webui/routes/admin.rs` (or the file where `/api/ui/apply` is registered) and `routes/mod.rs` (scope table)
- Modify: `crates/node/src/main.rs` (restore at startup, after the plugin manager is up)
- Test: in `components.rs`
- Modify: `docs/subsystems/deployment.md` (new section "1.7 Image model components"), `docs/subsystems/http-api.md`

**Interfaces:**
- Produces:
  - `pub(crate) struct ComponentAssignment { pub deploy_id: String, pub name: String, pub component: String, pub paths: Vec<String> /* absolute, on this node */, pub plugin_code: String, #[serde(default)] pub options: serde_json::Value }`.
  - Local store: `pom.db` local setting `served_components` = JSON array of `ComponentAssignment` (`load_assignments(db)`, `save_assignments(db, &[..])`).
  - `POST /api/ui/components/apply` body `ComponentAssignment` -> 200 `{ok: true}`; same scope as `/api/ui/apply` (deploy routes: any admin of the trust chain; refused on a visitor profile like `apply`). It checks every path is in this node's `image_files` (catalog), stores the assignment (replacing one with the same `deploy_id` + `component`), calls `open_on_demand(plugin_code)` and the plugin operation `component.serve` with the assignment as payload; a plugin refusal is 502 with its reason and nothing stored.
  - `POST /api/ui/components/release` body `{deploy_id}` -> drops this node's assignments of that deployment and calls `component.release {deploy_id}`; 200 even when nothing was served.
  - `pub(crate) async fn restore(state: GatewayState)`: at startup, for each stored assignment, `open_on_demand` + `component.serve`; failures logged, retried by the next call to `restore` from the existing periodic host-context refresh (`refresh_host_context`).

- [ ] **Step 1: Write the failing tests** (plugin calls behind a small trait or closure so tests record them instead of starting a host): `a_component_is_served_and_stored`, `a_path_outside_the_catalog_is_refused` (400, nothing stored, no plugin call), `a_visitor_refuses_to_serve` (403), `two_components_on_one_node_are_kept_apart` (diffusion and vae of one `deploy_id`, plus vae of another; releasing the first deployment leaves the third), `served_components_survive_a_restart` (`restore` on a new state with the same `pom.db` replays `component.serve` for each stored assignment).
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test -p node --bin pom -- components::` Expected: compile error.
- [ ] **Step 3: Implement;** register both routes and their scope (`routes/mod.rs` table and its scope tests), and call `restore` from startup.
- [ ] **Step 4: Run to verify they pass,** plus `cargo test -p node --bin pom -- routes` (scope table tests).
- [ ] **Step 5: Commit** (`components: a node serves the image components it is assigned`).

### Task 3: Deploying and undeploying an image model (principal side)

**Files:**
- Modify: `crates/node/src/components.rs`
- Modify: route registration and scope table (as Task 2)
- Modify: `docs/subsystems/deployment.md` (1.7), `docs/subsystems/http-api.md`

**Interfaces:**
- Consumes: `POST /api/ui/components/apply|release` on each target (Task 2), through `GatewayState::control_post_json` (local node through its own loopback, as the deploy code does).
- Produces:
  - `POST /api/ui/image-deploy` body `{name: String, components: [{component, node_id, path}]}`: same authority as `POST /api/ui/deploy`. Validates: 1 to 3 components, no component named twice, each `node_id` a live peer (or this node) whose `GET /api/ui/models` lists `path` in `image_files` with a matching category (`diffusion`->`diffusion` or `checkpoint`, `text_encoder`->`text_encoder` or `checkpoint`, `vae`->`vae` or `checkpoint`), and whose plugin services include a kind `image` service. Mints `deploy_id` (`img-` + 12 hex), applies to each target in turn; on a failure releases the targets already applied and answers 502 with the machine and reason. Records `{deploy_id, name, components, created_at}` in this node's `pom.db` setting `image_deployments`. Answers `{deploy_id}`.
  - `GET /api/ui/image-deployments` -> `{deployments: [{deploy_id, name, created_at, components: [{component, node_id, hostname, path, status}]}]}`, `status` from `network_components` (`missing` when no live service lists it).
  - `POST /api/ui/image-undeploy` body `{deploy_id}` -> release on every target of the record (a target that does not answer is reported, the record is still removed) -> `{released: [{node_id, ok, error?}]}`.

- [ ] **Step 1: Write the failing tests** with `gateway::testkit` peers whose `/api/ui/models` and services are served by test listeners (follow the pattern of the existing deploy route tests in `backend/deploy/tests/`): `an_image_model_is_deployed_on_its_holders`, `a_machine_without_the_file_is_refused` (400 naming the machine and file, no apply sent), `a_wrong_category_is_refused`, `a_failed_target_releases_the_others`, `undeploy_releases_every_target`.
- [ ] **Step 2: Run them to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass.**
- [ ] **Step 5: Commit** (`components: deploy and undeploy an image model by component`).

### Task 4: Calls to a remote component

**Files:**
- Modify: `crates/node/src/components.rs`, `crates/node/src/plugin_host_api.rs`, route registration and scope table
- Modify: `docs/subsystems/plugins-ui.md` (host_api table), `docs/subsystems/http-api.md`

**Interfaces:**
- Produces:
  - `GET /api/plugin-host/components` (plugin token) -> `{components: [NetworkComponent...]}` from `network_components`.
  - `POST /api/plugin-host/component/:deploy_id/:component/:op` (plugin token; body opaque, `DefaultBodyLimit::max(MAX_COMPONENT_BODY)`): finds the serving node in `network_components` (status `ready` or `loading`); this node -> forwards locally; another node -> `control_post` of the bytes to `POST /api/internal/plugins/:code/component/:deploy_id/:component/:op` with a 15-minute timeout; answers the remote status and body unchanged. No serving node -> 404 `{error: "no machine serves <component> of <deploy_id>"}`; transport failure -> 502 `{error: "<hostname> did not answer for <component>: <cause>"}`.
  - `POST /api/internal/plugins/:code/component/:deploy_id/:component/:op` (`InternalNodeAuth` + the live-member check `plugin_engine` uses for `engine/generate`): refuses a component not in this node's stored assignments (404); asks the plugin `component.upstream {deploy_id, component}` -> `{status: "ready", port, token}`; posts the body to `http://127.0.0.1:<port>/pom/component/<deploy_id>/<component>/<op>` with `x-pom-plugin-token: <token>` and returns its status and body.
  - `op` is `[a-z_]{1,32}`; anything else is 400.

- [ ] **Step 1: Write the failing tests:** `remote_calls_reach_the_serving_node` (two testkit nodes; the serving one's plugin upstream is a loopback test server echoing the body with its path), `unknown_components_are_refused`, `oversized_bodies_are_refused` (64 MiB + 1 -> 413, upstream never called), `a_vanished_server_is_a_named_error` (the serving peer's address closed -> 502 whose error names its hostname and the component), `component_routes_need_the_plugin_token`.
- [ ] **Step 2: Run them to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass,** plus the `routes` and `plugin_host_api` tests.
- [ ] **Step 5: Commit** (`components: plugins call a component on the machine that serves it`).

### Task 5: The image deploy screen

**Files:**
- Create: `apps/frontend/src/routes/ImageDeploy.tsx`, `apps/frontend/src/routes/ImageDeploy.test.tsx`
- Modify: `apps/frontend/src/lib/api.ts` (or its deploy helpers), `apps/frontend/src/lib/types.ts`, `apps/frontend/src/lib/queries.ts` (`useImageDeployments`)
- Modify: `apps/frontend/src/routes/DeployModel.tsx` (a mode-bar entry "Image model" that navigates to `/deploy/image`), `apps/frontend/src/App.tsx` (route, full-bleed), `apps/frontend/src/lib/access.ts` (same access as `/deploy`)
- Modify: `apps/frontend/src/i18n/messages.csv` (`imageDeploy.*` keys, en and pt-BR, rows inserted only, never re-sorting the file)
- Modify: `docs/guides/screens/deploy.md` and `deploy.pt-BR.md` (an "Image models" section), `apps/frontend/src/routes/AGENTS.md`

**Interfaces:**
- Consumes: `useClusterCatalog` (each machine's `image_files`), the cluster's plugin services (machines that publish a kind `image` service), `POST /api/ui/image-deploy`, `GET /api/ui/image-deployments`, `POST /api/ui/image-undeploy`.
- Produces: the screen: a name field (default: the diffusion file's name without extension); three rows (Diffusion model, Text encoder, VAE), each a file picker over the network's image files of that category (a `checkpoint` file is offered in all three) and, once a file is chosen, a machine picker listing only machines that hold that exact file (by name and size) and publish the image service, with each machine's free VRAM; a pre-fill of the text encoder and VAE when exactly one file of that category sits in the same `<org>/<repo>` folder as the chosen diffusion file; Deploy enabled when the diffusion row is complete; below, the served image models (`useImageDeployments`) with each component's machine and status and an Undeploy button behind a confirmation.

- [ ] **Step 1: Write the failing tests:** `only machines that hold the file are offered`, `the text encoder and VAE are pre-filled from the same folder`, `deploy sends the chosen components` (asserts the POST body), `served image models are listed with their status and can be undeployed`.
- [ ] **Step 2: Run them to verify they fail.** Run: `cd apps/frontend && npm test -- src/routes/ImageDeploy.test.tsx`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass,** then `npm test -- src/i18n src/App.test.tsx` and `npm run build`.
- [ ] **Step 5: Update the two guides; commit** (`ui: deploy an image model by component`).

### Task 6: POM docs pass

**Files:** `docs/subsystems/deployment.md`, `inference.md` (a pointer from 7.1 to the new section: components are not inference services), `crates/node/src/backend/AGENTS.md`, the root `AGENTS.md` only if the map changes (a new top-level module).

- [ ] **Step 1:** Re-read every route added in tasks 2-4 against `http-api.md` and the scope table; fix any gap.
- [ ] **Step 2:** Run `cargo test -p node --bin pom -- components plugin_services plugin_host_api routes catalog` and `cd apps/frontend && npm test` Expected: PASS (the known flaky `deploy::whole_route_tests` may fail intermittently; rerun it alone).
- [ ] **Step 3: Commit and push** `jair` (`docs: image model components`).

### Task 7 (pom-plugin-comfyui): the image service and the component operations

**Files:**
- Create: `src/components.rs` (served list, persisted to `<plugin dir>/served-components.json`)
- Modify: `src/lib.rs` (`host.services` gains the second service; `component.serve`, `component.release`, `component.upstream` operations), `src/supervisor.rs` (launch ComfyUI when a component is served and it is installed; write `<home>/pom-served.json` for ComfyUI on every change)
- Modify: `AGENTS.md`, `release/manifest.json` only if operations must be declared there

**Interfaces:**
- Consumes: `component.serve` payload = POM's `ComponentAssignment` (Task 2); `component.release {deploy_id}`; `component.upstream {deploy_id, component}`.
- Produces:
  - `host.services` second entry: `{"id": "comfyui-image", "kind": "image", "name": "ComfyUI", "status", "components": [{deploy_id, name, component, files, status}]}`; `status` per component: `loading` until ComfyUI is ready, `ready` after, `error` with the supervisor's error.
  - `component.serve`: refuses (`Err`) when ComfyUI is not installed ("Open ComfyUI once on this machine to install it"), when a path does not exist, or when `component` is not one of the three; otherwise stores, writes `pom-served.json`, launches ComfyUI if stopped, answers `{"status": "accepted"}` within the 3 s query budget.
  - `component.upstream`: `{"status": "ready", "port", "token"}` of the bridge while ComfyUI is ready and the component is served; `{"status": "starting"}` while launching; `Err` otherwise.
  - `<home>/pom-served.json`: `[{deploy_id, component, paths, options}]`, rewritten atomically (write + rename).

- [ ] **Step 1: Write the failing tests:** `serving_stores_and_publishes_the_component`, `serving_requires_an_installed_comfyui`, `release_drops_only_that_deployment`, `upstream_answers_only_for_served_components`, `the_served_file_is_written_for_comfyui`, `services_never_leak_paths_or_tokens` (the service JSON has file names only).
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test --locked components`
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the full plugin verify.**
- [ ] **Step 5: Commit and push** (`Serve image model components to the POM network`).

### Task 8 (pom-plugin-comfyui): ComfyUI runs the component ops (server side)

**Files:**
- Create: `runtime/pom_components.py` (written into `custom_nodes/pom_nodes/components.py` by `install_pom_nodes`, imported from `__init__.py`)
- Modify: `src/supervisor.rs` (`install_pom_nodes` writes the new file; its test)
- Create: `tests/test_pom_components.py` (stdlib `unittest`, `comfy` stubbed; pure parts only)
- Modify: `AGENTS.md`

**Interfaces:**
- Produces (aiohttp routes on ComfyUI's server, reached through the bridge, which already checks the plugin token):
  - `POST /pom/component/{deploy_id}/{component}/{op}`; the component must be listed in `pom-served.json` (re-read when its mtime changes), else 404.
  - Wire helpers `pack(tensors: dict[str, Tensor], args: dict) -> bytes` and `unpack(body: bytes) -> (dict[str, Tensor], dict)` (safetensors with `__metadata__["args"]`); lists of tensors travel as `<key>.<index>` and are rebuilt by `args["lists"]`.
  - `diffusion`: `load` (args `{options}` -> loads with `comfy.sd.load_diffusion_model(path, model_options)`, cached per options; answers args `{config: <comfy.supported_models class name>, unet_config: {...JSON-safe}, model_type: <ModelType name>, dtype: <str>}`); `forward` (tensors `x`, `t`, optional `context`, extra conds; args `{extra_lists, options}` -> `comfy.model_management.load_models_gpu([patcher])`, then `patcher.model.diffusion_model(x, t, context=context, **extra)` under `torch.no_grad()`; answers tensor `out`).
  - `text_encoder`: `encode` (args `{text, clip_type, options}` -> `comfy.sd.load_clip(paths, clip_type=...)` cached; `clip.encode_from_tokens_scheduled(clip.tokenize(text))`; answers each `[cond, extras]` entry as tensors `cond.<i>` and `extra.<i>.<key>` plus non-tensor extras in args).
  - `vae`: `load` (answers args with `downscale_ratio`, `upscale_ratio`, `latent_channels`, `latent_dim`, `output_channels`, `memory_used_*` factors the client needs), `decode` (tensor `samples` -> `images`), `encode` (tensor `pixels` -> `latent`).
  - A request for one component runs under a per-component lock (one GPU job at a time per component).

- [ ] **Step 1: Write the failing tests** (pure, no torch weights): `test_pack_round_trips_tensors_lists_and_args` (needs `torch` and `safetensors`; skip with a message when the stdlib-only interpreter lacks them, and run under the plugin's venv in the live test), `test_unserved_components_are_refused`, `test_served_list_is_reread_when_it_changes`.
- [ ] **Step 2: Run them to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the plugin verify;** extend the ignored live test (`installer::live`) with a served `vae` op round trip: serve a tiny real VAE (`madebyollin/taesd`'s `taesd_decoder.safetensors` fetched in the test, or the operator's `ae.safetensors` when `POM_COMFYUI_E2E_VAE` names it) and decode a zero latent through the bridge; assert the image shape.
- [ ] **Step 5: Commit and push** (`ComfyUI serves image model components`).

### Task 9 (pom-plugin-comfyui): remote components in ComfyUI's loader nodes (client side)

**Files:**
- Create: `runtime/pom_remote.py` (written as `custom_nodes/pom_nodes/remote.py`)
- Modify: `runtime/pom_nodes.py` (import and install the hooks), `src/supervisor.rs` (`install_pom_nodes`)
- Create: `tests/test_pom_remote.py`
- Modify: `docs/getting-started.md`, `AGENTS.md`

**Interfaces:**
- Consumes: `GET /api/plugin-host/components`, `POST /api/plugin-host/component/...` (Task 4) through `POM_HOST_API_URL`/`POM_HOST_API_TOKEN`; the ops of Task 8.
- Produces:
  - `REMOTE_PREFIX = "POM ▸ "`; listed names `"POM ▸ <name> ▸ <file>"` per network component, in the lists of `diffusion_models` (diffusion), `text_encoders` (text_encoder) and `vae` (vae), by wrapping `folder_paths.get_filename_list` for those folder names (the remote names are appended after the local ones; a failed lookup appends nothing).
  - Hooks on the loader nodes' functions in `nodes.py` (`UNETLoader.load_unet`, `CLIPLoader.load_clip`, `VAELoader.load_vae`): a name with `REMOTE_PREFIX` returns a stand-in, any other name calls the original.
  - `RemoteDiffusion(torch.nn.Module)`: `forward(x, timesteps, context=None, control=None, transformer_options={}, **kwargs)` posts `forward` and returns `out`; `dtype` attribute from `load`. The `MODEL` is a `comfy.model_patcher.ModelPatcher` around the real model class built from `load`'s `config`/`unet_config`/`model_type` with `unet_config["disable_unet_model_creation"] = True` and `diffusion_model = RemoteDiffusion(...)`, so sampling, latent format and conditioning run locally with ComfyUI's own code; its size is reported as 0 so the client loads nothing.
  - `RemoteCLIP`: `tokenize(text, **kwargs)` keeps the text (refuses images with "remote text encoders take text only"), `encode_from_tokens_scheduled(...)` and `encode_from_tokens(...)` post `encode`; `clone()` returns itself; `add_patches`/`load_model` raise.
  - `RemoteVAE`: `decode(samples, vae_options={})`, `encode(pixels)`, and the attributes `load` returned.
  - Patches on a remote `MODEL` (`add_patches`, `set_model_patch` and friends that change weights) raise `RuntimeError("LoRAs and model patches are not supported on a POM network model yet")`.
  - Every failure of a call raises `RuntimeError("<component> on <hostname>: <cause>")`.

- [ ] **Step 1: Write the failing tests** (stubs for `folder_paths`, `nodes`, `comfy`; fake host_api server): `test_network_components_are_listed_after_local_files`, `test_a_remote_name_returns_a_stand_in_and_a_local_name_the_original`, `test_remote_failure_names_the_machine`, `test_patches_on_a_remote_model_are_refused`, `test_images_on_a_remote_text_encoder_are_refused`.
- [ ] **Step 2: Run them to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the plugin verify,** then an equivalence check in the live test: one ComfyUI serves the VAE and the client's `RemoteVAE.decode` of a fixed latent equals the local `VAE.decode` of the same file within `atol=1e-3` (both on the same machine through a fake node API that forwards to the same bridge).
- [ ] **Step 5: Commit and push** (`ComfyUI uses image components served on the POM network`).

### Task 10 (pom-plugin-comfyui): docs pass

- [ ] **Step 1:** `docs/getting-started.md`: "Use models from the network" (the `POM ▸` names, what crosses the network, LoRAs not yet supported, closing ComfyUI stops this machine's components) and "Serve components" (deploy from POM's Deploy screen, Image model).
- [ ] **Step 2: Commit and push.**

### Task 11: Image generations in the request log and the image budget (POM)

**Files:**
- Modify: `crates/node/src/components.rs` (job registry), `crates/node/src/plugin_host_api.rs` (routes), `crates/node/src/backend/quota.rs` (`admit` forwards an image estimate)
- Modify: route registration and scope table
- Modify: `docs/subsystems/http-api.md` ("Token budgets" section: the image budget), `docs/subsystems/plugins-ui.md` (host_api table)

**Interfaces:**
- Consumes: `AuditLog::begin/complete`, `quota::admit`, the forward of Task 4.
- Produces:
  - `POST /api/plugin-host/image-job/begin` body `{job_id, prompt, model, details}` (plugin token): runs `quota::admit` with `request_details = {"kind": "image", "images": details.images or 1, ...details}` and the caller above; a `Denial` answers 429 `{error, retry_after_secs?}` and records the rejected request like `/v1` does; otherwise `AuditLog::begin(AuditRequest{ id: <new id>, route: "/comfyui/image", model, caller, entry_node_id, prompt, request_details })` and stores the job (`job_id` -> audit id, start instant, calls). Idempotent per `job_id`.
  - The forward of Task 4 reads an optional `x-pom-job: <job_id>` header and, for a known job, adds the call (component, deploy_id, hostname, ms, bytes in and out) to the job.
  - `POST /api/plugin-host/image-job/complete` body `{job_id, status: "ok"|"error"|"interrupted", error?, images, width?, height?, steps?}`: `AuditLog::complete` with the telemetry above (`status` `interrupted` -> `Incomplete`), drops the job. Unknown job -> 404.
  - A sweeper (the existing periodic task the node already runs, or a `tokio::time::interval` started with the routes) closes jobs older than 30 minutes as `Incomplete` with error `"ComfyUI did not report the end of this generation"`.
  - `quota::admit` passes `images` (from `request_details`) in the `quota.check` payload when `request_details.kind == "image"`.

- [ ] **Step 1: Write the failing tests:** `an_image_generation_is_logged_once_with_its_telemetry` (begin, three forwarded calls on two machines, complete -> one `begin` and one `complete` event whose telemetry carries both components, `images: 1`, hops in first-call order), `a_spent_image_budget_refuses_before_any_gpu` (a logs plugin stub answering a refusal -> 429 and no audit begin of a running job), `the_check_carries_the_image_estimate`, `an_unfinished_generation_is_closed_incomplete` (sweeper with an injected clock).
- [ ] **Step 2: Run them to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run to verify they pass,** plus `cargo test -p node --bin pom -- quota reqlog components`.
- [ ] **Step 5: Commit and push** (`components: image generations in the request log and the image budget`).

### Task 12 (pom-plugin-comfyui): ComfyUI reports its generations

**Files:**
- Modify: `runtime/pom_remote.py` (job tracking), `tests/test_pom_remote.py`, `docs/getting-started.md`, `AGENTS.md`

**Interfaces:**
- Consumes: Task 11's routes.
- Produces:
  - On the first remote call of an execution (`server.PromptServer.instance.last_prompt_id` while executing), `image-job/begin` with `job_id = <prompt_id>`, the prompt text of the first remote `encode` (empty until known), the deployment name, and `details.images` = the latent batch size when known, else 1. A 429 raises `RuntimeError("Image budget spent on this machine: <error>")` before any GPU call.
  - Every remote call sends `x-pom-job: <prompt_id>`.
  - The execution's end, seen by wrapping `PromptServer.instance.send_sync` for the events `execution_success`, `execution_error` and `execution_interrupted`, sends `image-job/complete` with the status, the error message, `images` (images decoded by `RemoteVAE.decode` in that execution, summed over the batch dimension) and the width, height and steps observed (latent size x 8, number of `forward` calls per conditioning branch). A failed `complete` is logged, never raised.

- [ ] **Step 1: Write the failing tests:** `test_a_generation_begins_once_and_completes_with_its_images`, `test_a_spent_budget_stops_before_the_first_call`, `test_every_call_carries_the_job`, `test_an_error_completes_the_job_with_its_message`.
- [ ] **Step 2: Run them to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the plugin verify.**
- [ ] **Step 5: Commit and push** (`ComfyUI reports its image generations to POM`).

### Task 13 (pom-plugin-logs): the image budget

**Files:**
- Modify: `src/quota.rs` (schema migration, evaluation, check, status, grants, overruns), `src/lib.rs` (`note_begin`/`note_complete` call sites)
- Test: `src/quota.rs` tests

**Interfaces:**
- Produces:
  - Schema (added with `ALTER TABLE ... ADD COLUMN ... DEFAULT 0` when missing, so existing databases migrate on open): `quota_plans.image_limit`, `quota_grants.extra_images`, `quota_usage.images`, `quota_overruns.used_images`.
  - A request whose `request_details.kind` is `image` is noted with `input = output = 0` and `images` = `request_details.images` at begin (the estimate) and `telemetry.images` at completion; every other request keeps its token accounting and `images = 0`.
  - `check` with `images > 0` in its payload refuses when an applying plan has `image_limit > 0` and `used_images + images > image_limit + extra_images`; such a request is not measured against the token limits. A token request is not measured against the image limit.
  - `status` and the plan, grant and overrun JSON gain `image_limit`, `used_images`, `extra_images`; the plan editing operation accepts `image_limit` (validated like the token limits).

- [ ] **Step 1: Write the failing tests:** `an_old_database_gains_the_image_columns`, `an_image_generation_is_charged_once` (begin estimate replaced by the completion count), `a_spent_image_budget_refuses_only_images`, `a_token_budget_does_not_stop_images`, `a_grant_raises_the_image_limit`.
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test --locked quota` (with the SSH rewrite for the private dependency noted in the repository's README if the fetch fails)
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the plugin's full verify (its `AGENTS.md` names it).**
- [ ] **Step 5: Commit and push** (`Quota: a budget of images per window`).

### Task 14 (pom-plugin-logs): image generations in Request Logs and Statistics

**Files:**
- Modify: `src/lib.rs` (kind classification and filter, list rows, stats), `src/stats.rs`
- Modify: `ui/src/routes/Logs.tsx`, `ui/src/routes/Statistics.tsx`, `ui/src/telemetry/` (an image telemetry view), the plugin's i18n files, their tests
- Modify: the plugin's guides of the Logs and Statistics screens (both languages)

**Interfaces:**
- Produces:
  - `kind` gains `image` (from `request_details.kind`), beside `normal` and `jev`; the `kind` filter accepts it; a row's `kind` is `image` for those requests.
  - Stats gain `images` totals and an `image` bucket in the per-kind breakdown (`{"requests", "images"}`).
  - The request detail of an image row shows the telemetry: images, size, steps, total time and one line per component (machine, calls, time, bytes in and out), in the style of the LLM hops.
  - The list shows "Image" as the kind badge and the image count where tokens are shown for LLM rows.

- [ ] **Step 1: Write the failing tests:** Rust `image_requests_are_their_own_kind` and `stats_count_images`; UI `an image request shows its components and machines` and `the kind filter offers Image`.
- [ ] **Step 2: Run them to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the plugin's full verify.**
- [ ] **Step 5: Update the two guides; commit and push** (`Logs: image generations with their telemetry`).

### Task 15 (pom-plugin-logs): the image budget on the Quota screen

**Files:**
- Modify: `ui/src/routes/Quota.tsx`, `ui/src/quota/components.tsx`, `ui/src/quota/api.ts`, i18n, tests, the Quota guide (both languages)

**Interfaces:**
- Consumes: Task 13's JSON fields.
- Produces: an "Images per window" field in the plan editor (empty or 0 = no limit), an images meter beside the token meters wherever a plan's use is drawn (assignments, overruns, a node's status), and grants that can add images.

- [ ] **Step 1: Write the failing tests:** `the plan editor saves an image limit`, `the images meter shows use against the limit`, `a grant can add images`.
- [ ] **Step 2: Run them to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the plugin's full verify.**
- [ ] **Step 5: Update the guide; commit and push** (`Quota screen: images per window`).

### Task 16: Deploy shortcut inside ComfyUI, for admins (POM + pom-plugin-comfyui)

**Files:**
- Modify (pom-jair): `crates/node/src/plugin_host_api.rs`, `crates/node/src/components.rs`, route registration, `docs/subsystems/plugins-ui.md`
- Modify (pom-plugin-comfyui): `runtime/pom_nodes.py` (`/pom/deploy/*` routes), `runtime/web/pom.js` (the dialog), `tests/test_pom_nodes.py`, `docs/getting-started.md`, `AGENTS.md`

**Interfaces:**
- Consumes: Task 3's deploy, list and undeploy functions; `GET /api/plugin-host/node` (`role`).
- Produces:
  - POM: `GET /api/plugin-host/image-deployments`, `POST /api/plugin-host/image-deploy` and `POST /api/plugin-host/image-undeploy` (plugin token) with the bodies of Task 3; they run only when this node's role is `principal` or `admin` (403 `{error: "only an admin node deploys"}` otherwise), with this node's own authority, exactly as the deploy screen's routes. `GET /api/plugin-host/image-files` answers the network's image files per machine (`node_id`, `hostname`, `image_files`, whether it publishes the image service), the data the dialog needs to offer only holders.
  - ComfyUI: a "Deploy on POM" command in ComfyUI's menu, registered by `pom.js` only when `/pom/deploy/role` says the node is an admin; it opens a dialog with the same choices as POM's screen (name; per component a file of that category and a machine that holds it; the served image models with Undeploy). `pom_nodes` relays `/pom/deploy/*` to the host_api routes; nothing is decided in the browser.

- [ ] **Step 1: Write the failing tests:** POM `a_non_admin_node_cannot_deploy_from_a_plugin` and `a_plugin_on_an_admin_node_deploys`; plugin `test_the_deploy_command_is_offered_only_to_admins` (`/pom/deploy/role` relays the role) and `test_deploy_relays_to_the_node`.
- [ ] **Step 2: Run them to verify they fail.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run both verifies.**
- [ ] **Step 5: Commit and push both** (`Deploy image models from ComfyUI, for admins`).

### Task 17: Acceptance on the operator's cluster (only on the operator's explicit go)

- [ ] **Step 1:** The operator builds and brings up `jair`, rebuilds the ComfyUI plugin and opens ComfyUI once on each node to install it.
- [ ] **Step 2:** Deploy Z-Image Turbo from the Image model screen with the diffusion model on the NVIDIA node and the text encoder and VAE on the two others (CPU or the AMD card when the NVIDIA one is busy).
- [ ] **Step 3:** In ComfyUI on a fourth node (or any node), load the Z-Image template, pick the three `POM ▸` entries and queue it. Check: an image comes out; with the same seed it matches the all-local run within tolerance; the serving nodes' logs show the ops; undeploying makes the next run fail with a named error.
- [ ] **Step 4:** Request Logs shows the generation as one "Image" row with its three components and machines; Statistics counts it; with a plan of 1 image per window assigned to that node, the second generation is refused in ComfyUI with the budget message before any GPU work.
