# POM_DESENV Deployment Values Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve every deployment-specified development parameter on peers regardless of local `POM_DESENV`, and make `llama.cpp` the default inference engine.

**Architecture:** Remove the backend's environment-dependent parameter rewriting from all deployment boundaries. Keep `POM_DESENV` as a frontend visibility flag only, preserving received values when visible controls are edited. Change the shared inference-mode default from `auto` to `dense` while retaining explicit `auto` parsing and execution support.

**Tech Stack:** Rust, serde, Axum deployment backend, React, TypeScript, Vitest, Cargo tests.

**Spec:** Captain bug report in the task prompt, with the regression introduced by commit `4d806b52`.

## Global Constraints

- `POM_DESENV` controls local UI visibility only; it never rewrites effective deployment behavior.
- Explicit deployment values must round-trip through plan, apply, launch, redeploy, and node handover paths.
- Missing inference mode defaults to `dense` (`llama.cpp`); explicit `moe` and explicit `auto` remain meaningful.
- Tests must cover all development-only fields that were gated by `POM_DESENV`.
- Update the affected folder guidance and subsystem documentation in the same commit.
- Do not modify generated files or `CHANGELOG.md`; do not push or open a PR.

---

### Task 1: Establish failing regression coverage

**Files:**
- Modify: `crates/node/src/backend/deploy/tests/core_b.rs`
- Modify: `crates/node/src/backend/webui/tests/core_a.rs`
- Modify: `apps/frontend/src/components/RuntimeParamsForm.capabilities.test.tsx`
- Modify: `apps/frontend/src/components/DeploymentInspector.idle.test.tsx`

**Interfaces:**
- Consumes the existing `RuntimeParams`, `effective_params_for_kind`, and gated UI components.
- Produces failing tests proving explicit values survive a local `POM_DESENV=0` boundary, all gated fields are preserved, and missing mode defaults to `dense`.

- [x] **Step 1: Change the Rust default and deployment expectations in tests first**

Make the default assertion expect `InferenceMode::Dense`. Make the deployment round-trip assertion expect the explicit `InferenceMode::Moe` for every deploy kind, independent of `crate::pom_desenv_enabled()`. Replace the helper characterization that expects gated fields to reset with a test that expects inference mode, cycle settings, MTP CPU fallback, and idle settings to remain exactly as requested.

- [x] **Step 2: Change the frontend regression expectations**

Replace the test that expects `applyPomDesenvDefaults(..., false)` to replace values with one that expects explicit values to remain unchanged. After toggling a visible control while `pomDesenv={false}`, assert the callback still contains the explicit inference, cycle, MTP fallback, and idle values. Keep assertions that the gated controls are absent.

- [x] **Step 3: Run the focused tests and verify the failures are causal**

Run:
```bash
rtk cargo test -p node backend::deploy::tests::core_b::tests::inference_mode_round_trips_for_whole_shards_and_tensor_parallel
rtk cargo test -p node backend::webui::tests::core_a::tests::inference_mode_defaults_to_dense
cd apps/frontend && rtk npm test -- --run src/components/RuntimeParamsForm.capabilities.test.tsx src/components/DeploymentInspector.idle.test.tsx
```
Expected: the tests fail because the current backend still resets values under the local flag and the current defaults are `auto`.

### Task 2: Remove environment-dependent deployment rewriting

**Files:**
- Modify: `crates/node/src/backend/webui/jobs.rs`
- Modify: `crates/node/src/backend/webui/jobs_apply.rs`
- Modify: `crates/node/src/backend/webui/lifecycle.rs`
- Modify: `crates/node/src/backend/deploy/types.rs`
- Modify: `crates/node/src/backend/deploy/lifecycle.rs`
- Modify: `crates/node/src/backend/deploy/node.rs`
- Modify: `crates/node/src/backend/deploy/tests/core_b.rs`
- Modify: `crates/node/src/backend/webui/tests/core_a.rs`

**Interfaces:**
- Consumes deployment requests and persisted `RuntimeParams`.
- Produces the same requested parameters at every deployment boundary, with only the existing Tensor Parallelism `fit_memory` safety rule remaining in `effective_params_for_kind`.

- [x] **Step 1: Implement the minimal Rust fix**

Remove `RuntimeParams::with_pom_desenv` and every call to it in apply, launch, reconfigure, add-node, redeploy, and plan paths. Change the `InferenceMode` default to `Dense`. Do not alter explicit `Auto` parsing or the runtime selector's explicit-mode behavior.

- [x] **Step 2: Run the focused Rust tests**

Run:
```bash
rtk cargo test -p node backend::webui::tests::core_a
rtk cargo test -p node backend::deploy::tests::core_b
```
Expected: PASS, including the new preservation and default tests.

### Task 3: Make UI gating visibility-only

**Files:**
- Modify: `apps/frontend/src/components/RuntimeParamsForm.tsx`
- Modify: `apps/frontend/src/components/DeploymentInspector.tsx`
- Modify: `apps/frontend/src/routes/DeployModel.tsx`
- Modify: `apps/frontend/src/routes/Deployments.tsx`
- Modify: `apps/frontend/src/components/RuntimeParamsForm.capabilities.test.tsx`
- Modify: `apps/frontend/src/components/DeploymentInspector.idle.test.tsx`

**Interfaces:**
- Consumes `NodeInfo.pom_desenv` for conditional rendering.
- Produces callbacks and deploy payloads that preserve the current `RuntimeParams` object, including hidden fields, while using `dense` only when an inference mode is actually absent.

- [x] **Step 1: Remove the local default merge from editable state**

Stop using `applyPomDesenvDefaults` to initialize, display, or submit deploy parameters. Use the received/edit state directly, and spread that original state when a visible field changes. Remove the orphaned `POM_DESENV_DEFAULTS` merge and update comments so the UI contract is visibility-only.

- [x] **Step 2: Keep the engine selector honest**

Keep the existing selector options `dense` and `moe`, map an absent mode to `dense` for presentation, and never emit `auto` merely because the local section is hidden. Preserve explicit `auto` when it arrives from an API or stored deployment.

- [x] **Step 3: Run frontend regression tests**

Run:
```bash
cd apps/frontend && rtk npm test -- --run src/components/RuntimeParamsForm.capabilities.test.tsx src/components/DeploymentInspector.idle.test.tsx src/routes/DeployModel.whole.test.tsx src/routes/Deployments.test.tsx
```
Expected: PASS, with gated controls absent under `pomDesenv={false}` and hidden values preserved in callbacks/payload state.

### Task 4: Documentation and complete verification

**Files:**
- Modify: `apps/frontend/src/components/AGENTS.md`
- Modify: `apps/frontend/src/routes/AGENTS.md`
- Modify: `crates/node/AGENTS.md`
- Modify: `crates/node/src/backend/AGENTS.md`
- Modify: `crates/node/src/inference/AGENTS.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `docs/subsystems/api-http.md`
- Modify: `docs/subsystems/inferencia.md`

**Interfaces:**
- Documents the current behavior of deployment parameter propagation and the `llama.cpp` default.

- [x] **Step 1: Update durable documentation**

Replace statements saying that disabled `POM_DESENV` clears or restores development fields. State that it only hides local controls and that deployment/apply/redeploy preserve explicit values from the request. State that missing inference mode defaults to `dense` and that `auto` remains an explicit backend mode only.

- [x] **Step 2: Run formatting, tests, build, and inspect the diff**

Run:
```bash
rtk cargo fmt --all -- --check
rtk cargo test -p node
cd apps/frontend && rtk npm test -- --run
rtk npm run build
```
Then run `rtk git diff --check`, review `rtk git diff`, and confirm no generated documentation or changelog was edited.

- [x] **Step 3: Commit the verified change**

```bash
rtk git add crates/node/src/backend apps/frontend/src/components apps/frontend/src/routes docs/subsystems
rtk git commit -m "fix: preserve deployment settings across pom desenv nodes"
```
