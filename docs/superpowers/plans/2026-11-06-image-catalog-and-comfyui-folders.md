# Image catalog and ComfyUI on POM's model folders - Implementation Plan (Plan A)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** POM's catalog finds and shows image model files (safetensors) by category, and ComfyUI downloads into and reads from POM's model folders.

**Architecture:** A new `models/image_catalog.rs` reads safetensors headers (never weights) and classifies each file by tensor names; the catalog carries the result as an optional `image_files` field, filled beside the sidecars in `scan_catalog`. The plugin learns the node's model folders from `host.configure` (`model_dirs`) and reads the image catalog through two new `host_api` routes; its ComfyUI custom nodes download into those folders and expose the catalog's files in ComfyUI's folders as links.

**Tech Stack:** Rust (axum, serde_json) in `pom-jair`; React + TypeScript + Vitest in `apps/frontend`; Rust + Python 3.12 (stdlib `unittest`) in `pom-plugin-comfyui`.

**Spec:** `docs/superpowers/specs/2026-11-06-image-models-by-component-design.md` (sections 5.1, 5.2, 5.3). Plan B (component deployment, remote calls, ComfyUI stand-ins: spec 5.4 to 5.6) is written after this plan lands, on top of its catalog.

**Repositories:** Tasks 1-5 in `pom-jair` (worktree on branch `jair`, never pushed unless the operator asks); tasks 6-8 in `pom-plugin-comfyui` (branch `main`, pushed when its task is done, per the workspace rules). Task 9 is the rollout and needs the operator's explicit go.

## Global Constraints

- Compatibility: every contract change is an added optional field or a new route. `Catalog.image_files` is `#[serde(default)]`; `host.configure.model_dirs` is optional for the plugin; older plugins and screens ignore both.
- The catalog never reads weights and never unpickles: safetensors header only, capped at `MAX_SAFETENSORS_HEADER = 100 * 1024 * 1024` bytes; `.ckpt`, `.pt`, `.pth`, `.bin` are listed by `stat` with `category: other`.
- Categories (serialized snake_case, exactly): `diffusion`, `text_encoder`, `vae`, `checkpoint`, `lora`, `controlnet`, `clip_vision`, `upscale`, `other`.
- ComfyUI folder for each category: `diffusion` -> `diffusion_models`, `text_encoder` -> `text_encoders`, `vae` -> `vae`, `checkpoint` -> `checkpoints`, `lora` -> `loras`, `controlnet` -> `controlnet`, `clip_vision` -> `clip_vision`, `upscale` -> `upscale_models`; `other` is not exposed.
- Image file extensions: `.safetensors`, `.sft` (header read); `.ckpt`, `.pt`, `.pth`, `.bin` (stat only).
- Secrets (`host_api` token, gateway key) reach ComfyUI only through its environment; never a log, an RPC answer, the browser or a workflow.
- Docs in English, no machine names, no absolute local paths, never an em dash; each task updates the `AGENTS.md`/subsystem/guide text its code changes, in the same commit.
- pom-jair verify: `cargo test -p node <filter>`, then before each commit `cargo fmt --check` and `cargo clippy -p node --all-targets -- -D warnings`; frontend: `cd apps/frontend && npm test -- <file> && npm run build`. Plugin verify: `scripts/build-ui.sh && POM_UI_REQUIRE_DIST=1 cargo test --locked && cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && tests/test_ci_plan.sh && python3 -m unittest discover -s tests -p 'test_*.py'`.

## Review Focus

1. A safetensors file whose header length is absurd or truncated (a partial download) must yield a `metadata_error` entry, never a panic or a 100 MB+ allocation. -> Task 1 test `truncated_and_oversized_headers_are_errors_not_panics`.
2. A `.part` file of an in-progress ComfyUI download must not appear in the catalog. -> Task 1 test `partial_downloads_are_not_listed`.
3. Two model folders on different disks, the first one full: the download must go to the second, and with no folder having room it must fail before writing. -> Task 7 test `picks_the_first_folder_with_room`.
4. A file removed from POM's folders must disappear from ComfyUI's lists; a user's own file in `.comfyui/models/<folder>/` must never be touched by the link sync. -> Task 8 tests `stale_links_are_removed` and `only_links_under_pom_are_touched`.
5. The same model folder listed twice (a symlink, or listed in `model_dirs` and reached through another root) must give one entry per file. -> Task 1 test `a_folder_reached_twice_lists_each_file_once`.

---

### Task 1: Read and classify safetensors headers

**Files:**
- Create: `crates/node/src/models/image_catalog.rs`
- Create: `crates/node/src/models/fixtures/safetensors/*.json` and `crates/node/src/models/fixtures/safetensors/README.md`
- Modify: `crates/node/src/main.rs` (declare `#[path = "models/image_catalog.rs"] mod image_catalog;` next to `mod catalog;`, line 59-60; the `models/` folder has no `mod.rs`)
- Modify: `crates/node/src/models/AGENTS.md`, `docs/subsystems/slicing.md` (section 1, "Discovery and catalog": a paragraph on image files)

**Interfaces:**
- Produces:
  - `pub enum ImageCategory { Diffusion, TextEncoder, Vae, Checkpoint, Lora, Controlnet, ClipVision, Upscale, Other }` (`Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug`, `#[serde(rename_all = "snake_case")]`)
  - `pub struct ImageModelFile { pub id: String, pub name: String, pub path: String, pub size_bytes: u64, pub source: ModelSource, pub source_label: &'static str, pub format: String, pub category: ImageCategory, pub family: Option<String>, pub dtype: Option<String>, pub metadata_error: Option<String> }` (`Serialize, Deserialize, Clone, Debug`). `name` is the path relative to its root with `/` separators (`<publisher>/<repo>/<file>`); `id` is built the way `DiscoveredModel.id` is for a folder model.
  - `pub struct SafetensorsHeader { pub tensors: Vec<(String, String)>, pub metadata: BTreeMap<String, String> }` (name, dtype).
  - `pub fn read_safetensors_header(path: &Path) -> Result<SafetensorsHeader, String>`
  - `pub fn classify(header: &SafetensorsHeader) -> (ImageCategory, Option<String>, Option<String>)` (category, family, dominant dtype in lowercase: `bf16`, `f16`, `f32`, `f8_e4m3`, `i8`...)
  - `pub fn discover_image_files(roots: &[(PathBuf, ModelSource)]) -> Vec<ImageModelFile>`, sorted by lowercase `name` then `path`; headers cached by `(path, size, mtime)` like `catalog::cached_info`.

- [ ] **Step 1: Collect real header fixtures.** For each category below, save the JSON header (bytes 8..8+N of the file, the tensor table and `__metadata__`) of one real public file as `fixtures/safetensors/<name>.json`, and list each source URL in the fixture `README.md`. Fetch the headers with two HTTP `Range` requests (`bytes=0-7`, then `bytes=8-<7+N>`) so no weights are downloaded. Use the three Z-Image files already on the operator's disk for `z_image_diffusion`, `qwen3_4b_text_encoder` and `flux_ae_vae`. Required fixtures: `z_image_diffusion`, `qwen_image_diffusion`, `flux_diffusion`, `qwen3_4b_text_encoder`, `t5xxl_text_encoder`, `clip_l_text_encoder`, `flux_ae_vae`, `sdxl_vae`, `sdxl_checkpoint`, `sd15_checkpoint`, `lora` (kohya style), `lora_peft` (`lora_A`/`lora_B`), `controlnet`, `clip_vision`, `esrgan_upscale`.

- [ ] **Step 2: Write the failing tests** in `image_catalog.rs` (`#[cfg(test)] mod tests`). Helper `fn write_fixture(dir: &Path, fixture: &str) -> PathBuf` writes `u64` LE header length + the fixture JSON + zero bytes for the data size the offsets need (sparse `set_len` is enough).

```rust
#[test]
fn real_headers_are_classified() {
    for (fixture, category) in [
        ("z_image_diffusion", ImageCategory::Diffusion),
        ("qwen_image_diffusion", ImageCategory::Diffusion),
        ("flux_diffusion", ImageCategory::Diffusion),
        ("qwen3_4b_text_encoder", ImageCategory::TextEncoder),
        ("t5xxl_text_encoder", ImageCategory::TextEncoder),
        ("clip_l_text_encoder", ImageCategory::TextEncoder),
        ("flux_ae_vae", ImageCategory::Vae),
        ("sdxl_vae", ImageCategory::Vae),
        ("sdxl_checkpoint", ImageCategory::Checkpoint),
        ("sd15_checkpoint", ImageCategory::Checkpoint),
        ("lora", ImageCategory::Lora),
        ("lora_peft", ImageCategory::Lora),
        ("controlnet", ImageCategory::Controlnet),
        ("clip_vision", ImageCategory::ClipVision),
        ("esrgan_upscale", ImageCategory::Upscale),
    ] { /* write, read_safetensors_header, classify; assert_eq!(category, expected, "{fixture}") */ }
}

#[test]
fn an_unknown_tensor_table_is_other() // {"foo.weight": ...} -> ImageCategory::Other

#[test]
fn truncated_and_oversized_headers_are_errors_not_panics()
// a 5-byte file; a length of u64::MAX; a length of MAX_SAFETENSORS_HEADER + 1; JSON that is not an object
// -> read_safetensors_header returns Err for each, and discover_image_files lists the file with metadata_error Some(_)

#[test]
fn partial_downloads_are_not_listed() // "x.safetensors.part" and ".x.safetensors" are skipped

#[test]
fn pickle_formats_are_listed_without_reading() // a.ckpt with random bytes -> category Other, format "ckpt", metadata_error None

#[test]
fn a_folder_reached_twice_lists_each_file_once() // root and a symlink to it passed as two roots -> one entry
```

- [ ] **Step 3: Run them to verify they fail.** Run: `cargo test -p node image_catalog` Expected: compile error (module items missing).

- [ ] **Step 4: Implement `image_catalog.rs`.** Walk each root recursively the way the GGUF folder walk does (skip hidden entries, `.part` files and the `.slicing-*` folders; dedupe roots by resolved path as `discover_fast` does). `classify` checks, in this order, the rules of spec 5.1: `checkpoint`, `lora`, `controlnet`, `vae`, `diffusion`, `text_encoder`, `clip_vision`, `upscale`, else `other`; strip a leading `model.diffusion_model.` or `diffusion_model.` before the diffusion patterns. `family` comes from `__metadata__["modelspec.architecture"]` when present, else from distinctive tensor names (only for the families of the fixtures; `None` otherwise). Write the README's "how fixtures were made" note.

- [ ] **Step 5: Run the tests to verify they pass.** Run: `cargo test -p node image_catalog` Expected: all PASS.

- [ ] **Step 6: Commit.**

```bash
git add crates/node/src/models/image_catalog.rs crates/node/src/models/fixtures crates/node/src/main.rs crates/node/src/models/AGENTS.md docs/subsystems/slicing.md
git commit -m "catalog: read and classify safetensors image model headers"
```

### Task 2: Image files in the catalog and in `/api/ui/models`

**Files:**
- Modify: `crates/node/src/models/catalog.rs` (`Catalog` struct, `discover`, the `Catalog { .. }` literals)
- Modify: `crates/node/src/backend/webui/catalog.rs` (`scan_catalog`, its fallback literal)
- Test: `crates/node/src/backend/webui/tests/` (the catalog route tests that already exist there)
- Modify: `docs/subsystems/http-api.md` (the `/api/ui/models` answer), `crates/node/src/backend/AGENTS.md` if it lists catalog fields

**Interfaces:**
- Consumes: `image_catalog::{ImageModelFile, discover_image_files}`.
- Produces: `Catalog.image_files: Vec<ImageModelFile>` (`#[serde(default)]`), present in `GET /api/ui/models` and therefore in each machine's survey answer.

- [ ] **Step 1: Write the failing test** `image_files_are_in_the_models_answer`: a `gateway::testkit` state whose model folder holds the `flux_ae_vae` fixture file and one GGUF; `GET /api/ui/models` answers `image_files[0].category == "vae"`, `image_files[0].name == "pub/repo/ae.safetensors"`, and the GGUF still in `models` only.
- [ ] **Step 2: Run it to verify it fails.** Run: `cargo test -p node image_files_are_in_the_models_answer` Expected: FAIL (no field).
- [ ] **Step 3: Implement.** Add the field; fill it in `scan_catalog` inside the same `spawn_blocking` as the sidecars, and in `discover`, from the `model_dirs` that are not Ollama stores (never the slice folder); set `image_files: Vec::new()` in every other literal.
- [ ] **Step 4: Run it to verify it passes,** then `cargo test -p node catalog` Expected: PASS, no regressions.
- [ ] **Step 5: Commit** (`catalog: publish image model files in /api/ui/models`).

### Task 3: Delete an image file from the Full models screen

**Files:**
- Modify: `crates/node/src/backend/webui/setup.rs` (`delete_model`)
- Modify: `crates/node/src/backend/transfer.rs` only to make `ensure_gguf`'s name check reusable if needed; the transfer gate itself stays GGUF-only (sending image files is Plan B)
- Modify: `docs/subsystems/http-api.md` (`/api/ui/models/delete` accepts an image file of the catalog)

**Interfaces:**
- Produces: `pub(crate) fn ensure_deletable_model(path: &Path) -> Result<(), String>` in `setup.rs`: `Ok` for a valid GGUF (the existing `ensure_gguf`) or for a file with an image extension of the Global Constraints whose header `read_safetensors_header` accepts (stat only for pickle formats); `Err` otherwise.

- [ ] **Step 1: Write the failing tests** `an_image_file_can_be_deleted` (fixture file in a model folder -> 200, file gone) and `a_text_file_named_safetensors_is_refused` (`hello` written to `x.safetensors` -> 400, file kept).
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test -p node an_image_file_can_be_deleted a_text_file_named_safetensors_is_refused` Expected: FAIL.
- [ ] **Step 3: Implement** `ensure_deletable_model` and call it from `delete_model` instead of `ensure_gguf`; the serving and in-transit warnings stay as they are.
- [ ] **Step 4: Run the tests to verify they pass.**
- [ ] **Step 5: Commit** (`models: delete image model files from the Full models screen`).

### Task 4: Plugins learn the model folders and read the image catalog

**Files:**
- Modify: `crates/node/src/plugins.rs` (`host_context_payload`, `refresh_host_context`, their tests)
- Modify: `crates/node/src/plugin_host_api.rs` (two handlers, module doc, tests)
- Modify: `crates/node/src/backend/webui/routes/common.rs` (register the two routes)
- Modify: `docs/subsystems/plugins-ui.md` (the `host.configure` example and the `host_api` route table), `docs/subsystems/http-api.md`

**Interfaces:**
- Produces:
  - `host.configure` context gains `"model_dirs": ["<abs path>", ...]`: `NodeConfig::model_dir_paths()` of the node, without the slice folder; `host_context_payload(gateway_port, api_key, workspace_root, model_dirs: &[String])`.
  - `GET /api/plugin-host/catalog/images` -> `{"image_files": [ImageModelFile...], "metadata_complete": bool}` from `local_catalog`.
  - `POST /api/plugin-host/catalog/refresh` -> `{"ok": true}` after `invalidate_catalog` and a fresh `local_catalog`.
  - Both take `x-pom-plugin-host-token` (`caller_code`), like the existing routes.

- [ ] **Step 1: Write the failing tests:** `host_context_carries_the_model_dirs` (payload has the array, no slice folder) and, in `plugin_host_api` tests, `catalog_routes_need_the_plugin_token` (401 `unknown plugin host token` without it, extend the existing `every_route_needs_the_plugin_token` list) and `the_image_catalog_lists_the_node_files` (fixture in a model folder -> one entry).
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test -p node host_context_carries_the_model_dirs plugin_host_api` Expected: FAIL.
- [ ] **Step 3: Implement** the payload field and the two handlers; register the routes next to `/api/plugin-host/sign`.
- [ ] **Step 4: Run the tests to verify they pass.**
- [ ] **Step 5: Commit** (`plugins: model_dirs in host.configure and the image catalog over host_api`).

### Task 5: Full models screen shows image files

**Files:**
- Modify: `apps/frontend/src/lib/types.ts` (`CatalogResponse.image_files?: ImageModelFile[]`, the `ImageModelFile` and `ImageCategory` types matching Task 1)
- Modify: `apps/frontend/src/routes/WholeModels.tsx` (a two-option switch "LLM (GGUF)" / "Image" above the table; `ImageModelRow`)
- Modify: `apps/frontend/src/i18n/` en and pt-BR (`models.kindLlm`, `models.kindImage`, `models.columnCategory`, `models.columnFamily`, `models.columnDtype`, `models.imageEmpty`, one `models.category.<category>` per category)
- Test: `apps/frontend/src/routes/WholeModels.test.tsx`
- Modify: `docs/guides/screens/models.md` and `models.pt-BR.md` (lead and a card for image files; replace the screenshot in `docs/guides/images/<locale>/models.webp` with one showing the switch)

**Interfaces:**
- Consumes: `image_files` from `/api/ui/models` (Task 2), `deleteModel(path, name, confirm)` (works for image files after Task 3).

- [ ] **Step 1: Write the failing tests:** `shows image files under the Image switch` (catalog with one GGUF and one `vae` file: the GGUF row by default; after clicking "Image", a row with the file name, the translated category, `bf16` and the size) and `image rows offer delete and no slice` (a delete button, no "Slice" link).
- [ ] **Step 2: Run them to verify they fail.** Run: `cd apps/frontend && npm test -- WholeModels` Expected: FAIL.
- [ ] **Step 3: Implement** the switch (remembered in the URL query `?kind=image` so a reload keeps it), the row (name, source, category, family, dtype, size, delete) and the empty state for no image files.
- [ ] **Step 4: Run the tests to verify they pass,** then `npm run build`.
- [ ] **Step 5: Update the two guides and the screenshot; commit** (`ui: image model files on the Full models screen`).

### Task 6 (pom-plugin-comfyui): model folders and host_api reach ComfyUI

**Files:**
- Modify: `src/context.rs` (parse `model_dirs`; `launch_identity`)
- Modify: `src/supervisor.rs` (bridge environment)
- Modify: `runtime/bridge.py` (pass the variables on to ComfyUI)
- Modify: `AGENTS.md`

**Interfaces:**
- Produces:
  - `Context.model_dirs: Vec<PathBuf>` (absolute paths only; a relative entry is an error like `workspace_root`); missing field = empty.
  - `Context::launch_identity()` also covers `model_dirs` and the `host_api` base URL, so a change restarts ComfyUI.
  - ComfyUI's environment gains `POM_MODEL_DIRS` (newline-joined), `POM_HOST_API_URL`, `POM_HOST_API_TOKEN` (absent when `host_api` is absent).

- [ ] **Step 1: Write the failing tests:** `model_dirs_are_read_and_must_be_absolute` (context.rs) and `the_bridge_gets_the_model_dirs_and_host_api` (supervisor: the env map built for the bridge holds the three variables; the token value appears nowhere in `rpc.status`, extending the existing token leak test).
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test --locked model_dirs the_bridge_gets` Expected: FAIL.
- [ ] **Step 3: Implement;** `bridge.py` forwards the three variables unchanged to the ComfyUI child (it already forwards `POM_OPENAI_BASE_URL`/`POM_API_KEY`).
- [ ] **Step 4: Run the full plugin verify** (Global Constraints) Expected: PASS.
- [ ] **Step 5: Commit and push** (`ComfyUI gets POM's model folders and host_api`).

### Task 7 (pom-plugin-comfyui): downloads land in POM's model folders

**Files:**
- Modify: `runtime/pom_nodes.py` (`_download_target`, `_fetch`, `start_download`)
- Create: `tests/test_pom_nodes.py` (stdlib `unittest`; stubs `folder_paths` and `server` in `sys.modules` before importing `runtime/pom_nodes.py` by path)
- Modify: `runtime/web/pom.js` (completion toast shows the full path the server returns)
- Modify: `docs/getting-started.md`, `AGENTS.md`

**Interfaces:**
- Consumes: `POM_MODEL_DIRS`, `POM_HOST_API_URL`, `POM_HOST_API_TOKEN` (Task 6), `POST /api/plugin-host/catalog/refresh` (Task 4).
- Produces:
  - `relative_destination(url: str, name: str) -> str`: `https://huggingface.co/<org>/<repo>/resolve/<rev>/.../<file>` -> `<org>/<repo>/<name>`; `https://civitai.com/api/download/models/<id>` (and `civitai.red`) -> `civitai/<id>/<name>`.
  - `choose_root(dirs: list[str], size: int, free=shutil.disk_usage) -> str | None`: the first directory whose free space is at least `size + 512 MiB`; `None` when none has room.
  - The download state gains `"path"` (the final absolute path) for the toast.

- [ ] **Step 1: Write the failing tests:** `test_hugging_face_layout`, `test_civitai_layout`, `picks_the_first_folder_with_room` (fake `free`: first dir full, second roomy -> second; both full -> `None`, and `start_download` then fails with the not-enough-space message before creating any file), `test_falls_back_to_comfyui_models_without_dirs` (empty `POM_MODEL_DIRS` -> `<comfy models>/<directory>/<name>`, phase 1 behaviour), `test_refresh_is_called_after_a_download` (fake host_api server on loopback records the POST with the token header).
- [ ] **Step 2: Run them to verify they fail.** Run: `python3 -m unittest discover -s tests -p 'test_*.py'` Expected: FAIL.
- [ ] **Step 3: Implement.** The size comes from the response's `Content-Length` before the first byte is written; the target folder is chosen then. Keep the `.part` then rename order. After the rename, POST the refresh (errors logged, not raised), then call `sync_links()` (Task 8; a no-op until it lands).
- [ ] **Step 4: Run the full plugin verify** Expected: PASS.
- [ ] **Step 5: Commit and push** (`ComfyUI downloads into POM's model folders`).

### Task 8 (pom-plugin-comfyui): ComfyUI lists the files of POM's catalog

**Files:**
- Modify: `runtime/pom_nodes.py` (`plan_links`, `sync_links`, a `POST /pom/models/sync` route, a sync at import)
- Modify: `runtime/web/pom.js` (call `/pom/models/sync` in `setup()`, then `app.refreshComboInNodes()`)
- Modify: `tests/test_pom_nodes.py`, `docs/getting-started.md`, `AGENTS.md`

**Interfaces:**
- Consumes: `GET /api/plugin-host/catalog/images` (Task 4) through `POM_HOST_API_URL`/`POM_HOST_API_TOKEN`.
- Produces:
  - `CATEGORY_FOLDERS: dict[str, str]` exactly as in Global Constraints.
  - `plan_links(files: list[dict], models_dir: str) -> dict[str, str]`: `<models_dir>/<folder>/pom/<name>` -> the file's `path`, for every entry whose category has a folder and whose `metadata_error` is empty.
  - `sync_links(models_dir: str, plan: dict[str, str]) -> None`: creates missing links (symlink; on failure a hard link; on failure a log line), replaces links pointing elsewhere, removes links under `<folder>/pom/` that are not in the plan, and removes emptied directories there. It never touches anything outside `<folder>/pom/` and never removes a regular file.

- [ ] **Step 1: Write the failing tests:** `test_plan_links_maps_categories` (a `vae` and a `diffusion` entry, an `other` entry and one with `metadata_error` -> two links at the right folders), `stale_links_are_removed` (a link for a file no longer in the plan disappears), `only_links_under_pom_are_touched` (a regular file at `<models>/vae/mine.safetensors` and a regular file inside `<models>/vae/pom/` stay), `test_sync_is_idempotent` (two runs, same tree).
- [ ] **Step 2: Run them to verify they fail.** Run: `python3 -m unittest discover -s tests -p 'test_*.py'` Expected: FAIL.
- [ ] **Step 3: Implement;** the import-time sync runs on a background thread so ComfyUI's start never waits on the node, and a missing `host_api` skips it with one log line.
- [ ] **Step 4: Run the full plugin verify,** then extend the ignored live test (`installer::live`) to assert that a fixture file served by a fake `catalog/images` endpoint shows up in `/api/models/vae`. Run it once with `POM_COMFYUI_E2E_DIR` set Expected: PASS.
- [ ] **Step 5: Commit and push** (`ComfyUI lists the image files of POM's catalog`).

### Task 9: Rollout on the operator's machine (only on the operator's explicit go)

- [ ] **Step 1:** With the operator's confirmation, move the three files in `<workspace>/.comfyui/models/{diffusion_models,text_encoders,vae}` into a model folder as `Comfy-Org/z_image_turbo/<file>` (the layout Task 7 would have used).
- [ ] **Step 2:** Only if the operator asks for the deploy: build and bring up the cluster with the `jair` branch the way the workspace `AGENTS.md` describes, then rebuild the ComfyUI plugin from Plugins.
- [ ] **Step 3:** Check: the Full models screen's "Image" switch lists the three files with categories `diffusion`, `text_encoder`, `vae`; ComfyUI's Load Diffusion Model, Load CLIP and Load VAE list them under `pom/`; the Z-Image Turbo template runs; a new download from a template lands under `<model folder>/<org>/<repo>/`.
