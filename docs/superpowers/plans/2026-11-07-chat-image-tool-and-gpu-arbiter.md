# Chat image tool and GPU arbiter - Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The Chat LLM generates images through a `generate_image` tool with a prompt it writes from the conversation, and one physical GPU shared by an LLM and image components is arbitrated in the LLM's favour.

**Architecture:** The tool is Chat-only (the node's `/v1` routes already stream tool calls). The arbiter lives in the node (`gpu_arbiter.rs` + `gpu_identity.rs`): a gate before every image component op on the serving node, a yield route that unloads an idle LLM, and a hook that frees ComfyUI's VRAM before a yielded LLM reloads. ComfyUI gets `POST /pom/gpu/yield`.

**Tech Stack:** Rust (axum, tokio), React + TypeScript, Python in ComfyUI.

**Spec:** `docs/superpowers/specs/2026-11-07-chat-image-tool-and-gpu-arbiter-design.md`

## Global Constraints

- `RuntimeParams.yield_gpu_to_images: bool`, `#[serde(default)]` false for every engine, in the inspector's POM settings card.
- GPU identity: NVIDIA `nvidia-smi --query-gpu=index,uuid`; AMD sysfs `PCI_SLOT_NAME` of `/sys/class/drm/card*/device` with vendor `0x1002`; published as optional `NodeInfo.gpu_ids: [{index, id}]`. No identity, no arbitration.
- Grace before resuming images after an LLM request: 10 s of zero in-flight. Gate wait bounded by the component call timeout.
- Compatibility: only optional fields and new routes; older plugins without `/pom/gpu/yield` are tolerated (404 ignored).
- Docs in English, no em dash; guides in both languages; commits end with the attribution lines.

## Review Focus

- An LLM that is mid-generation must never be unloaded for an image (gate test on in-flight > 0).
- A deployment with `yield_gpu_to_images` off is never unloaded (test).
- Nodes without GPU identity behave exactly as before (test).
- The tool must not be offered when no workflow exists or in Text only (test).
- A tool call streamed in fragments must be reassembled before running (test).

---

### Task 1: GPU identity and the yield setting
**Files:** Create `crates/node/src/gpu_identity.rs`; modify `crates/node/src/backend/webui/params.rs` (`RuntimeParams.yield_gpu_to_images`), `NodeInfo` (`gpu_ids`), `main.rs`.
**Produces:** `pub(crate) struct GpuId { index: u32, id: String }`; `pub(crate) fn parse_nvidia(csv: &str) -> Vec<GpuId>`; `pub(crate) fn parse_amd(cards: &[(String /*vendor*/, String /*uevent*/)]) -> Vec<GpuId>`; `pub(crate) async fn gpu_ids() -> Vec<GpuId>` (cached).
- [ ] Tests `nvidia_uuids_are_read`, `amd_cards_are_read_by_pci_slot`, `other_vendors_are_skipped`, `yield_defaults_to_off` (RuntimeParams from `{}`). Implement; `cargo test -p node --bin pom gpu_identity params` PASS; commit.

### Task 2: The arbiter gate and the yield/release routes
**Files:** Create `crates/node/src/gpu_arbiter.rs`; modify `components.rs` (gate in `internal_component_route` and the local path of `component_call`), `inference/runtime.rs` (`yield_for_images()`: the idle unload without the timer, only with `yield_gpu_to_images` and zero in-flight; a release hook before an idle reload), routes.
**Produces:** `pub(crate) struct Member { node_id, gpu_ids: Vec<String>, llm_loaded: bool, in_flight: u32, yields: bool, free_vram_mb: Option<u64> }`; `pub(crate) enum Verdict { Go, Yield(Vec<String>), Wait }`; `pub(crate) fn decide(gpu: &str, need_mb: u64, members: &[Member], quiet_since: &dyn Fn(&str) -> Option<Duration>) -> Verdict`; routes `POST /api/internal/gpu/yield` (live member, signed) and `POST /api/internal/gpu/release-images` (asks the local ComfyUI `/pom/gpu/yield`).
- [ ] Tests `no_llm_on_the_gpu_goes`, `room_in_vram_goes`, `an_idle_yielding_llm_is_asked_to_yield`, `a_busy_llm_makes_the_image_wait`, `an_llm_that_does_not_yield_makes_it_wait`, `a_recently_busy_llm_waits_for_the_grace`, `another_gpu_is_not_a_member`. Implement; PASS; commit.

### Task 3: ComfyUI frees its VRAM on request
**Files (pom-plugin-comfyui):** `runtime/pom_components.py` (`POST /pom/gpu/yield` under `_gpu_lock` calls `_unload_gpu`), tests, AGENTS.md.
- [ ] Test `test_yield_unloads_under_the_gpu_lock`; implement; plugin suite PASS; commit, push main.

### Task 4: POM settings toggle
**Files:** `apps/frontend/src/components/DeploymentInspector.tsx`, types, i18n, `docs/guides/settings/*` (both languages).
- [ ] Test the toggle renders in POM settings, off by default, and writes `yield_gpu_to_images`; implement; PASS; commit.

### Task 5: The generate_image tool in the Chat
**Files:** `apps/frontend/src/lib/chatStream.ts` (keep tool calls), `lib/chatStore.ts` (pass tools, expose the final call), `routes/Chat.tsx` (menu modes Automatic/Text only/Always image; run the workflow on a `generate_image` call; prompt disclosure), `lib/chatImageTool.ts` (tool schema, aspect to size), i18n, guides.
- [ ] Tests `the_tool_is_offered_only_in_automatic_with_workflows`, `fragments_reassemble_into_one_call`, `a_call_runs_the_workflow_with_the_llm_prompt`, `the_menu_size_wins_over_aspect`, `text_only_sends_no_tool`; implement; PASS; commit.

### Task 6: Docs and checks
- [ ] `deployment.md` (arbiter), `http-api.md` (routes, `gpu_ids`), `web-ui.md` (tool, menu modes), backend AGENTS; `cargo test` on touched modules, `npx tsc -b`, vitest; commit; push `jair`.
