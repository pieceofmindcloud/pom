# Image pipelines in Deploy, Dashboard and Chat - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Image models deployed as a three-stage pipeline (text encoder -> diffusion -> VAE) with replicas from the Deploy studio, shown and managed on the Dashboard and Deployments screens, and usable from the Chat through ComfyUI workflows whose images are kept on the requesting node.

**Architecture:** The image deployment keeps its own record (`image_deployments`), now versioned with stages and replicas; new files hold the record (`image_records.rs`), the stage plan and deploy (`image_plan.rs`) and the Chat image flow (`chat_image.rs`), while `components.rs` keeps assignments, the live view and call routing (now replica-aware). The ComfyUI plugin adds `in_flight`, `workflows` and a `/pom/workflow/run` route. The UI gains an image mode in `DeployModel`, an image pipeline view on the Dashboard, image rows on Deployments and a Generate image switch in the Chat.

**Tech Stack:** Rust (axum, tokio, serde), React + TypeScript (vitest), Python inside ComfyUI (aiohttp, unittest).

**Spec:** `docs/superpowers/specs/2026-11-07-image-pipelines-deploy-dashboard-chat-design.md`

## Global Constraints

- Stage order everywhere: `text_encoder`, `diffusion`, `vae`.
- Compatibility: `Deployment`, `DeploymentKind`, `/v1/models`, the LLM deploy routes unchanged; old `image_deployments` records load; `POST /api/ui/image-deploy` keeps accepting `components`; the record keeps writing `components` (first replica of each stage).
- A machine is offered for a stage only when it holds the stage's file (name + size identity) and publishes an `image` service not in `error`/`waiting`; copies only with `allow_transfers`.
- Chat images: `<workspace_root or project root>/.comfyui/images/<sha256 hex>.<png|jpg>`, extension from magic bytes, at most 16 images per request and 32 MiB per image, served at `GET /api/ui/chat/image/:hash` with `:hash` = 64 lowercase hex.
- Replica order per call: ready first, then measured RTT to this node ascending (unknown last), then `in_flight` ascending; sticky per `x-pom-job`; failover on transport error or 5xx, never on 4xx.
- Docs in English, no em dash, no machine names; each task updates the `AGENTS.md`/subsystem doc/guide it changes (root `AGENTS.md`, "Documentation duty"); guides in both languages.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. pom-jair work on branch `jair`; plugin work on `main` of `pom-plugin-comfyui`.

## Review Focus

- A machine dropped on a stage goes offline before Deploy: the deploy must report that replica and still succeed when the stage has another replica (test in Task 3).
- The same file name with a different size on two machines is not the same file: the plan must refuse the machine with the other size (test in Task 2).
- Undeploy pressed on an admin node that did not deploy the model: every serving machine must be released (test in Task 4).
- A Chat request while the chosen workflow's deployment lost its last diffusion replica: the bubble must show the plugin's reason, not hang (test in Task 7 for the 502 answer, Task 13 for the bubble).
- The same image generated twice: one file on disk, both messages show it (test in Task 7).

---

### Task 1: Versioned image deployment record

**Files:**
- Create: `crates/node/src/image_records.rs`
- Modify: `crates/node/src/components.rs` (move `ImageDeployment`, `ComponentChoice`, load/save there; re-export), `crates/node/src/lib.rs` or `main.rs` module list
- Test: in `image_records.rs`

**Interfaces:**
- Produces:
  - `pub(crate) const STAGES: [&str; 3] = ["text_encoder", "diffusion", "vae"];`
  - `pub(crate) struct FileIdentity { name: String, size_bytes: u64 }`
  - `pub(crate) struct Replica { node_id: String, hostname: String, path: String }`
  - `pub(crate) struct ImageStage { component: String, file: FileIdentity, replicas: Vec<Replica> }`
  - `pub(crate) struct ImageDeploymentVersion { version: u32, created_at: String, created_by: String, stages: Vec<ImageStage> }`
  - `ImageDeployment` gains `created_by: String` (default ""), `family: Option<String>`, `versions: Vec<ImageDeploymentVersion>` (default empty), `active_version: u32` (default 1); keeps `components: Vec<ComponentChoice>`.
  - `impl ImageDeployment { fn ensure_versions(&mut self); fn active(&self) -> &ImageDeploymentVersion; fn push_version(&mut self, stages: Vec<ImageStage>, by: &str) -> u32; fn sync_components(&mut self) }`
  - `pub(crate) fn load_image_deployments(db) -> Vec<ImageDeployment>` (calls `ensure_versions` on each), `pub(crate) fn save_image_deployments(db, &[ImageDeployment])` (calls `sync_components` first).
  - `pub(crate) fn sort_stages(stages: &mut Vec<ImageStage>)` (order of `STAGES`).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn an_old_record_becomes_version_one() {
    let raw = r#"[{"deploy_id":"img-aaaaaaaaaaaa","name":"z","created_at":"t",
      "components":[{"component":"vae","node_id":"n2","path":"/m/ae.safetensors"},
                    {"component":"text_encoder","node_id":"n1","path":"/m/qwen.safetensors"}]}]"#;
    let mut list: Vec<ImageDeployment> = serde_json::from_str(raw).unwrap();
    list[0].ensure_versions();
    let v = list[0].active();
    assert_eq!(v.version, 1);
    assert_eq!(v.stages.iter().map(|s| s.component.as_str()).collect::<Vec<_>>(), ["text_encoder", "vae"]);
    assert_eq!(v.stages[1].replicas[0].node_id, "n2");
    assert_eq!(v.stages[1].file.name, "ae.safetensors");
}

#[test]
fn components_mirror_the_first_replica_of_each_stage() { /* push_version with 2 diffusion replicas; sync_components -> 1 diffusion choice, node of replica 0 */ }

#[test]
fn a_new_version_becomes_active() { /* push_version twice -> active_version == 2, versions.len() == 2 */ }
```

- [ ] **Step 2: Run** `cargo test -p node image_records` - FAIL (module missing).
- [ ] **Step 3: Implement** the types and functions above; size of a migrated file is 0 when unknown (old records carry no size).
- [ ] **Step 4: Run** `cargo test -p node image_records components::` - PASS (existing component tests still pass).
- [ ] **Step 5: Commit** `components: versioned image deployment record with stages and replicas`.

### Task 2: The stage plan

**Files:**
- Create: `crates/node/src/image_plan.rs`
- Modify: `crates/node/src/components.rs` (`MachineFacts` gains `files: Vec<ImageFileFact>`, `service_status: Option<String>`, `gpu_free_mb: u64`, `ram_free_mb: u64`; `survey_machines` fills them from `/api/ui/models` `image_files` (`path`, `name`, `size_bytes`, `category`) and the peer's announced free memory, the same numbers the LLM plan's `machines` use), `crates/node/src/backend/webui/routes/common.rs`
- Test: in `image_plan.rs`

**Interfaces:**
- Consumes: Task 1 types.
- Produces:
  - `pub(crate) struct StageRequest { component: String, file: FileIdentity, node_ids: Vec<String> }`
  - `pub(crate) struct ImagePlanRequest { name: String, stages: Vec<StageRequest>, #[serde(default)] allow_transfers: bool }`
  - `pub(crate) struct MachineVerdict { node_id: String, hostname: String, ok: bool, reason: Option<String>, path: Option<String> }`
  - `pub(crate) struct StagePlan { component: String, file: FileIdentity, chosen: Vec<MachineVerdict>, alternatives: Vec<MachineVerdict> }`
  - `pub(crate) struct MachineMemory { node_id: String, gpu_free_mb: u64, ram_free_mb: u64, need_mb: u64 }`
  - `pub(crate) struct Copy { component: String, file: FileIdentity, from_node_id: String, to_node_id: String }`
  - `pub(crate) struct ImagePlan { name: String, stages: Vec<StagePlan>, memory: Vec<MachineMemory>, copies: Vec<Copy>, blocked: Vec<String> }`
  - `pub(crate) fn plan_image(request: &ImagePlanRequest, machines: &[MachineFacts]) -> ImagePlan`
  - `POST /api/ui/image-deploy/plan` -> `ImagePlan` JSON (admin, like `image-deploy`).
- Reasons (exact strings, shown on screen): `"does not run ComfyUI"`, `"ComfyUI is <status>"`, `"does not hold <file>"`, `"holds another <file> (<size> bytes)"`, `"<file> is a <category>, not a <component>"`. Blocked lines: `"<component> has no machine"`, `"unknown component <x>"`, `"an image deployment needs a name"`, `"<component> is chosen twice"`.

- [ ] **Step 1: Write the failing tests** - `the_plan_orders_stages_by_flow`, `a_machine_on_two_stages_is_fine`, `two_machines_on_one_stage_are_replicas`, `an_empty_stage_blocks`, `a_machine_without_comfyui_is_refused_with_its_reason`, `the_same_name_with_another_size_is_another_file` (asserts reason `holds another ...`), `a_checkpoint_fits_every_stage`, `alternatives_list_every_machine_with_a_reason`, `a_holder_elsewhere_becomes_a_copy` (machine lacks the file, another holds it: `copies.len()==1`, verdict `ok`, and `blocked` contains `"copies need your permission"` when `allow_transfers` is false), `memory_sums_the_files_a_machine_loads` (`need_mb` = sum of its stages' sizes, rounded up).
- [ ] **Step 2: Run** `cargo test -p node image_plan` - FAIL.
- [ ] **Step 3: Implement** `plan_image` (pure) and the route (survey, then `plan_image`).
- [ ] **Step 4: Run** `cargo test -p node image_plan components::` - PASS.
- [ ] **Step 5: Commit** `components: the image stage plan`.

### Task 3: Deploying stages with replicas

**Files:**
- Modify: `crates/node/src/image_plan.rs` (deploy), `crates/node/src/components.rs` (`deploy_image` becomes a thin wrapper that converts the old `components` request into stages; `deploy_response`), `crates/node/src/backend/webui/jobs_apply.rs` only to reuse `new_job`
- Test: in `image_plan.rs`

**Interfaces:**
- Consumes: Task 2 `plan_image`, Task 1 record.
- Produces:
  - `ImageDeployRequest` accepts either `components` (old) or `stages: Vec<StageRequest>` (+ `allow_transfers`).
  - `pub(crate) async fn deploy_stages(db, machines, call: &TargetCall, request: ImagePlanRequest, by: &str, progress: impl Fn(&str, f64)) -> Result<DeployOutcome, String>`
  - `pub(crate) struct DeployOutcome { deploy_id: String, failed: Vec<MachineVerdict> }`
  - `POST /api/ui/image-deploy` answers `{deploy_id, job_id, failed}` at once and runs the applies in a task with a `Job { kind: "deploy", deploy_id, deployment_name, title: "Deploy <name>" }` (so the studio's progress window and the bell work); the job fails with the reason when a stage ends with no replica.
- Behaviour: apply each (stage, replica) with `APPLY_PATH` (one assignment per component; a machine with two stages gets two applies); a failed replica is released and listed in `failed`; if any stage has no applied replica, release everything applied and fail with `"<component>: no machine could serve it (<host>: <error>; ...)"`; record only on success (version 1).

- [ ] **Step 1: Write the failing tests** - `replicas_are_all_applied`, `a_machine_on_two_stages_gets_two_assignments`, `a_failed_replica_is_reported_and_the_deploy_succeeds`, `a_stage_left_empty_releases_everything`, `the_old_components_form_still_deploys`, `a_plan_with_copies_and_no_permission_is_refused` (409 body `transfers_not_authorized` at the route).
- [ ] **Step 2: Run** `cargo test -p node image_plan` - FAIL.
- [ ] **Step 3: Implement** `deploy_stages` and the job wiring in the route.
- [ ] **Step 4: Run** `cargo test -p node image_plan components::` - PASS.
- [ ] **Step 5: Commit** `components: deploy image stages with replicas, as a deploy job`.

### Task 4: Changing a live image deployment, undeploy anywhere

**Files:**
- Modify: `crates/node/src/image_plan.rs`, `crates/node/src/components.rs` (`undeploy_image`), `routes/common.rs`
- Test: in `image_plan.rs` and `components.rs`

**Interfaces:**
- Produces routes (admin):
  - `POST /api/ui/image-deploy/:deploy_id/add` `{component, node_id}` -> `{version}`; refused 404 when the record is not on this node (`"this deployment was made from another machine"`), 400 with the plan reason when the machine cannot take the stage.
  - `POST /api/ui/image-deploy/:deploy_id/remove` `{component, node_id, #[serde(default)] confirm_gap: bool}` -> `{version, serving}`; 409 `{"error":"gap", "component"}` when it is the last replica and `confirm_gap` is false.
  - `POST /api/ui/image-deploy/:deploy_id/redeploy` `{version}` -> `{version}`: releases machines not in that version, applies the ones in it, makes it active (no new version).
  - `undeploy_image(db, call, deploy_id, served: &[NetworkComponent])`: releases the union of the record's replicas and every machine listing `deploy_id` in `served`; works with no record.
- Removing a replica of one stage on a machine that serves another stage of the same deployment releases only that component: the target route `POST /api/ui/components/release` gains optional `component` (absent = whole deployment, as today), and the plugin's `component.release` receives it.

- [ ] **Step 1: Write the failing tests** - `adding_a_replica_appends_a_version`, `removing_the_last_replica_asks_first`, `removing_one_stage_keeps_the_other_on_that_machine`, `redeploy_reactivates_an_old_version`, `undeploy_without_a_record_releases_every_server`, `release_with_a_component_drops_only_it` (assignments test).
- [ ] **Step 2: Run** - FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p node image_plan components::` - PASS.
- [ ] **Step 5: Commit** `components: add, remove and redeploy image replicas; undeploy from any admin`.

### Task 5: The live view

**Files:**
- Modify: `crates/node/src/components.rs` (`image_deployments_view`, `NetworkComponent` gains `in_flight: Option<u32>`, `ServedComponent` in `plugin_services.rs` gains optional `in_flight`)
- Test: in `components.rs`

**Interfaces:**
- Produces `GET /api/ui/image-deployments` -> `{deployments: [ImageDeploymentView]}`:

```text
ImageDeploymentView { deploy_id, name, family: Option, recorded: bool, active_version: u32,
  versions: [ImageDeploymentVersion] (only when recorded), serving: bool,
  stages: [{ component, file: {name, size_bytes}, replicas: [{node_id, hostname, status, in_flight}] }] }
```

  `status` is the service's (`ready`/`loading`/`error`) or `missing` for a recorded replica no service lists. A `deploy_id` only seen on the network is listed with `recorded: false`, its stages built from the services (file from `files[0]`, size 0). Sorted by name. `serving` = every stage has a `ready` replica.

- [ ] **Step 1: Write the failing tests** - `the_view_merges_records_and_services`, `a_deployment_seen_only_on_the_network_is_listed`, `serving_needs_a_ready_replica_per_stage`, `in_flight_is_read_from_the_service` (`ServedComponent` parse with and without `in_flight`).
- [ ] **Step 2-4:** run FAIL, implement (a pure `fn view_of(records, served, peers) -> Vec<Value>` plus the route), run PASS.
- [ ] **Step 5: Commit** `components: live view of image deployments with replicas`.

### Task 6: Routing a call to a replica

**Files:**
- Modify: `crates/node/src/components.rs` (`call_component`), `crates/node/src/plugin_host_api.rs` (`component_call` passes the RTT lookup), routing RTT source: the same per-node RTT the Flow map reads (`state` RTT table used by `GET /api/ui/network/rtt`)
- Test: in `components.rs`

**Interfaces:**
- Produces:
  - `pub(crate) fn replica_order(candidates: &[NetworkComponent], rtt_ms: &dyn Fn(&str) -> Option<f64>) -> Vec<NetworkComponent>` (Global Constraints order).
  - `static STICKY: Mutex<HashMap<String /*job*/, (String /*deploy_id:component*/, String /*node_id*/, Instant)>>` entries dropped after `JOB_TIMEOUT`.
  - `call_component(network, deploy_id, component, op, body, job, forward, rtt_ms)` tries the sticky replica first when it is still listed, then `replica_order`; moves on after `Err(_)` or a status >= 500; returns the first 2xx/4xx; when all fail, 502 naming every host tried.
  - `component_call` records telemetry for the replica that answered.

- [ ] **Step 1: Write the failing tests** - `the_nearest_ready_replica_is_called`, `ties_go_to_the_least_busy`, `a_dead_replica_fails_over`, `a_5xx_fails_over_but_a_4xx_is_returned`, `a_job_stays_on_its_replica`, `all_replicas_down_names_every_host`.
- [ ] **Step 2-4:** run FAIL, implement, run `cargo test -p node components::` PASS.
- [ ] **Step 5: Commit** `components: calls pick the nearest replica, stick per generation and fail over`.

### Task 7: Chat images on the node

**Files:**
- Create: `crates/node/src/chat_image.rs`
- Modify: `crates/node/src/backend/webui/cluster.rs` (`chat_capabilities_value` adds `image`, `image_workflows`), `routes/common.rs`, `docs/subsystems/http-api.md`, `docs/subsystems/plugins-ui.md`, `docs/subsystems/web-ui.md` (Chat section)
- Test: in `chat_image.rs`

**Interfaces:**
- Produces:
  - `pub(crate) struct Workflow { id: String, name: String, node_id: String, deploy_id: Option<String> }` and `fn workflows_of(services_by_node: &[(String, Vec<Value>)], serving: &[String] /*deploy_ids serving*/) -> Vec<Workflow>` (built-in only when its `deploy_id` is serving; published when the service is `ready`; built-ins first, then by name).
  - Capabilities gain `"image": "supported" | "unavailable"` and `"image_workflows": [{id, name, node_id}]`.
  - `pub(crate) fn images_dir(workspace_root: Option<&Path>, project_root: &Path) -> PathBuf` = `<root>/.comfyui/images`.
  - `pub(crate) fn store_image(dir: &Path, bytes: &[u8]) -> Result<StoredImage, String>`; `StoredImage { hash: String, ext: &'static str, width: u32, height: u32 }`; ext from magic (`\x89PNG` -> png, `\xFF\xD8\xFF` -> jpg), else `Err("not a PNG or JPEG image")`; > 32 MiB -> `Err("an image is larger than 32 MiB")`; write to a temp name then rename; existing file kept.
  - `POST /api/ui/chat/image` `{workflow_id, node_id, prompt, negative?, width?, height?, seed?, count?}` (chat authorization of `/v1/chat/completions` UI calls: the UI session or the node's chat key) -> `application/x-ndjson`: progress lines `{"p":n,"of":m}` relayed, then `{"images":[{hash,ext,width,height}]}` or `{"error":"..."}`. More than 16 images -> error, nothing written. The image budget check of plan B (`image_job_begin` path) runs first.
  - Forwarding: local node -> plugin `ui.upstream` then `POST http://127.0.0.1:<port>/pom/workflow/run` with `x-pom-plugin-token`; other node -> signed `POST /api/internal/plugins/:code/workflow/run` there (new internal route, live member, body <= 256 KiB), which does the local call and streams back.
  - `GET /api/ui/chat/image/:hash` -> the file with its content type; 400 unless 64 lowercase hex; 404 when missing.

- [ ] **Step 1: Write the failing tests** - `png_and_jpeg_get_their_extension`, `other_bytes_are_refused`, `an_oversized_image_is_refused`, `the_same_image_is_one_file`, `the_folder_is_under_dot_comfyui_images`, `a_bad_hash_is_refused`, `workflows_list_builtins_of_serving_deployments_first`, `a_published_workflow_needs_a_ready_service`, `too_many_images_write_nothing`, `a_failed_run_answers_the_plugin_reason` (forward stub returns `{"error":"no machine serves diffusion of img-x"}` -> last NDJSON line carries it).
- [ ] **Step 2-4:** run FAIL, implement, run `cargo test -p node chat_image` PASS.
- [ ] **Step 5: Commit** `chat: generate images through ComfyUI workflows, kept on the requesting node`.

### Task 8: ComfyUI plugin - in-flight, workflows, run

**Files (repo `pom-plugin-comfyui`):**
- Create: `runtime/pom_workflows.py`, `tests/test_pom_workflows.py`
- Modify: `runtime/pom_components.py` (count in-flight ops per `(deploy_id, component)`; expose to the service file), `src/components.rs` (`service()` adds `in_flight` per component and `workflows`), `runtime/pom_nodes.py` (register workflow routes), `runtime/web/pom.js` ("Publish to Chat" in the POM menu, admins only), `src/supervisor.rs` (write `pom_workflows.py` like the other runtime files), `docs/getting-started.md`, `AGENTS.md`
- Test: `tests/test_pom_workflows.py`, Rust tests in `src/components.rs`

**Interfaces:**
- Produces:
  - Service entry: `components[i].in_flight: u32`, `workflows: [{id, name, deploy_id?, inputs}]`; built-in id `builtin:<deploy_id>`, published id `pub:<slug>`.
  - `pom_workflows.builtin(deploy, family) -> dict` (ComfyUI API-format graph: `CLIPLoader`/`UNETLoader`/`VAELoader` with `POM ▸ <name> ▸ <file>` names, `CLIPTextEncode` x2, `EmptySD3LatentImage` (or the family's latent node), `KSampler`, `VAEDecode`, `SaveImage` with prefix `pom-chat/`); family defaults table: `z_image`: steps 8, cfg 1.0, sampler `res_multistep`, scheduler `simple`, 1024x1024; `qwen_image`: 20, 2.5, `euler`, `simple`, 1328x1328; `flux`: 20, 1.0, `euler`, `simple`, 1024x1024; default: 20, 5.0, `euler`, `normal`, 1024x1024.
  - `pom_workflows.publish(name, graph, inputs: {prompt: node_id, negative?, width?, height?, seed?}) -> id` stored in `<home>/user/pom-workflows/<slug>.json`; `published() -> list`.
  - `pom_workflows.substitute(graph, inputs_map, values) -> graph` (sets the text of the prompt node, etc.; random seed when absent).
  - Route `POST /pom/workflow/run` (bridge token) body `{workflow_id, prompt, negative?, width?, height?, seed?, count?}`: queues via `PromptServer.instance` / `execution` queue, streams NDJSON progress from `progress` events of that prompt id, ends with `{"images":[{"mime","width","height","data"}]}` read from the `SaveImage` outputs, or `{"error"}`.
  - Route `POST /pom/workflow/publish` (admin only, as `/pom/deploy/*`) and `GET /pom/workflow/list`.

- [ ] **Step 1: Write the failing tests** - `test_builtin_wires_network_components_in_flow_order`, `test_family_defaults` (z_image: steps 8, cfg 1.0), `test_substitute_sets_prompt_and_seed`, `test_publish_and_list`, `test_publish_refuses_a_graph_without_the_prompt_node`; Rust: `the_service_lists_in_flight_and_workflows`.
- [ ] **Step 2: Run** `venv/bin/python -m unittest tests.test_pom_workflows` and `cargo test` - FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the plugin's full suite (`cargo test`, `cargo clippy --all-targets -- -D warnings`, python unittests) - PASS.
- [ ] **Step 5: Commit and push to `main`** `ComfyUI: in-flight counts, Chat workflows and the workflow run route`.

### Task 9: UI types and queries

**Files:**
- Modify: `apps/frontend/src/lib/types.ts`, `apps/frontend/src/lib/queries.ts`
- Test: `apps/frontend/src/lib/queries.test.ts` (existing pattern)

**Interfaces:**
- Produces TS types mirroring Tasks 2, 5, 7 (`ImagePlanRequest`, `ImagePlan`, `StagePlan`, `MachineVerdict`, `ImageDeploymentView`, `ImageStageView`, `ImageReplicaView`, `ChatImage { hash; ext; width; height }`, capabilities `image?`, `image_workflows?`), and hooks: `useImageDeployments()` (new shape), `useImagePlan()` (mutation), `useImageDeploy()`, `useImageReplicaChange()` (add/remove/redeploy), `useImageUndeploy()`; `ChatMessage` gains optional `images?: ChatImage[]`.

- [ ] Steps: failing typecheck/test for the new hook shapes -> implement -> `npm run typecheck && npx vitest run src/lib` PASS -> commit `ui: image pipeline types and queries`.

### Task 10: Image mode in the Deploy studio

**Files:**
- Create: `apps/frontend/src/components/ImageDeployStudio.tsx`, `ImageStageCanvas.tsx`, test `ImageDeployStudio.test.tsx`
- Modify: `apps/frontend/src/routes/DeployModel.tsx` (mode bar: `Image model` becomes a radio that sets `imageMode`; when on, render `<ImageDeployStudio modeBar={...} />` in place of map/inspector/strip), `App.tsx` (remove `/deploy/image`, redirect it to `/deploy?mode=image`), delete `routes/ImageDeploy.tsx` and its test, i18n `en`/`pt-BR`, `docs/guides/screens/deploy.md` + `.pt-BR.md`, `apps/frontend/src/routes/AGENTS.md`, `docs/subsystems/web-ui.md`
- Test: `ImageDeployStudio.test.tsx`

**Interfaces:**
- Consumes: Task 9 hooks; drag payload `application/x-pom-node` (same as the LLM strip).
- Produces: `ImageDeployStudio({ modeBar }: { modeBar: ReactNode })`; `ImageStageCanvas({ plan, stages, onDrop(component, nodeId), onRemove(component, nodeId), onInspect(nodeId), inspectedNodeId })` drawing three columns `text_encoder`, `diffusion`, `vae` with machine cards (free VRAM/RAM, `need_mb`) and RTT labels between consecutive stages.
- Behaviour: file pickers (diffusion/checkpoint, then text encoder and VAE prefilled from the same folder when exactly one each), name, re-plan on every structural change, red verdict line from `blocked`, greyed strip machines with the reason for the hovered stage, Deploy -> existing `DeployProgressModal` on the returned `job_id`, copies -> existing authorization window then deploy with `allow_transfers`.

- [ ] **Step 1: Failing tests** - `drops_a_machine_on_a_stage`, `the_same_machine_can_go_on_two_stages`, `two_machines_on_one_stage_show_as_replicas`, `shows_the_verdict_and_keeps_deploy_off`, `prefills_encoder_and_vae_from_the_folder`, `deploy_opens_the_progress_window`, `old_image_route_redirects_to_image_mode`.
- [ ] **Step 2-4:** `npx vitest run src/components/ImageDeployStudio.test.tsx` FAIL -> implement -> PASS; `npm run typecheck && npm run build` PASS.
- [ ] **Step 5: Commit** `ui: image models in the Deploy studio as a three-stage pipeline`.

### Task 11: Image pipelines on the Dashboard

**Files:**
- Create: `apps/frontend/src/components/ImagePipelineMap.tsx`, `ImageAddReplicaDialog.tsx`, tests
- Modify: `components/ModelNetworkCanvas.tsx` (optional `extraModels?: {id: string; label: string; badge: string}[]` in the chip; picking one calls `onModelChange(id)`), `routes/Dashboard.tsx` (image ids `image:<deploy_id>`; when picked render `ImagePipelineMap`, top stats read stages, Add opens `ImageAddReplicaDialog`, Undeploy uses `useImageUndeploy` through the existing `UndeployPipeline` confirmation with the replica machines; properties panel shows the stages read-only), i18n, `docs/guides/screens/dashboard.md` + `.pt-BR.md`, `routes/AGENTS.md`
- Test: `ImagePipelineMap.test.tsx`, `Dashboard.test.tsx`

**Interfaces:**
- Consumes: `useImageDeployments`, `useNetworkRtt`, Task 9 mutations.
- Produces: `ImagePipelineMap({ deployment, rtt, onSelectNode })`; dialog `ImageAddReplicaDialog({ deployment, onClose })` (stage select, then machines that hold the stage file from `useImagePlan` alternatives).

- [ ] **Step 1: Failing tests** - `the_chip_lists_image_deployments`, `an_image_deployment_draws_three_stages_in_flow_order`, `replicas_stack_under_their_stage_with_status`, `undeploy_lists_every_replica_machine`, `add_offers_only_holders_of_the_stage_file`, `image_deployments_show_with_no_llm_served`.
- [ ] **Step 2-4:** run FAIL, implement, run PASS; typecheck and build.
- [ ] **Step 5: Commit** `ui: image pipelines on the Dashboard`.

### Task 12: Image deployments on the Deployments screen

**Files:**
- Modify: `apps/frontend/src/routes/Deployments.tsx`, test `Deployments.test.tsx`, i18n, `docs/guides/screens/deployments.md` + `.pt-BR.md`

**Interfaces:** rows from `useImageDeployments` with an `Image` badge, versions listing stages and replica hosts, Redeploy (recorded only) via `useImageReplicaChange`.

- [ ] Steps: failing tests `image_deployments_are_listed_with_a_badge`, `an_image_version_lists_its_stages_and_replicas`, `redeploy_calls_the_image_route` -> implement -> PASS -> commit `ui: image deployments on the Deployments screen`.

### Task 13: Generate image in the Chat

**Files:**
- Create: `apps/frontend/src/components/ChatImageGrid.tsx`, `apps/frontend/src/lib/chatImage.ts`, tests
- Modify: `routes/Chat.tsx` (switch + workflow picker when `image === "supported"`; send through `startChatImage` instead of the LLM when on), `lib/chatStore.ts` (`ChatSession.imageEnabled?`, `imageWorkflow?`; `ChatMessage.images?`), i18n, `docs/guides/screens/chat.md` + `.pt-BR.md`, `docs/subsystems/web-ui.md`
- Test: `chatImage.test.ts`, `ChatImageGrid.test.tsx`, `Chat.features.test.tsx`

**Interfaces:**
- Produces: `startChatImage({ session, prompt, workflowId, nodeId, onProgress, signal }) -> Promise<ChatImage[]>` reading the NDJSON of `POST /api/ui/chat/image`; `ChatImageGrid({ images })` loading `/api/ui/chat/image/<hash>`, a full-size dialog with Download, and the text "This image is no longer on this node" on a 404.

- [ ] **Step 1: Failing tests** - `the_switch_shows_only_when_images_are_supported`, `the_picker_lists_builtins_first`, `a_prompt_goes_to_the_image_route_when_on`, `progress_then_images_in_the_bubble`, `a_failure_shows_the_reason`, `a_missing_image_says_so`, `images_are_kept_as_hashes_in_the_session`.
- [ ] **Step 2-4:** run FAIL, implement, run PASS; typecheck and build.
- [ ] **Step 5: Commit** `ui: generate images from the Chat`.

### Task 14: Whole-branch checks and docs sweep

- [ ] Run `cargo fmt --check` (only touched files), `cargo clippy -p node --all-targets -- -D warnings`, `cargo test -p node`, `cd apps/frontend && npm run typecheck && npx vitest run && npm run build`.
- [ ] `docs/subsystems/deployment.md` section 1.7 rewritten for stages, replicas, plan, add/remove/redeploy, undeploy anywhere and replica routing; `http-api.md` lists every new route; `crates/node/src/AGENTS.md` lists `image_records.rs`, `image_plan.rs`, `chat_image.rs`.
- [ ] Commit `docs: image pipelines, replicas and Chat images`; push `jair`.

### Task 15: Live acceptance (operator's cluster, only with the operator's go-ahead to deploy)

- [ ] Z-Image Turbo: text encoder + VAE on one machine, diffusion on two machines; Dashboard shows three stages with two diffusion replicas; a Chat prompt returns an image saved under `.comfyui/images/`; stopping one diffusion replica's ComfyUI keeps generation working; Undeploy from another admin node releases all.
