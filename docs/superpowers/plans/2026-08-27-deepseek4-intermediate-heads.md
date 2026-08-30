# DeepSeek4 Intermediate Head Slicing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generate loadable non-final DeepSeek-V4-Flash slices by copying the real required output tensors and show their serialized disk cost.

**Architecture:** The architecture registry remains the source of required output tensor names. The splitter uses that registry to select real source tensors for non-final slices, while preserving the existing synthetic intermediate norm. The layout estimator calls the same selection predicate, and the UI displays the backend's exact serialized total plus the required-head overhead when available.

**Tech Stack:** Rust, GGUF header/tensor streaming, llama.cpp FFI callback, Axum JSON API, React/TypeScript localization, Cargo tests, npm build.

**Spec:** `docs/superpowers/specs/2026-08-27-deepseek4-intermediate-heads-design.md`

## Global Constraints

- Never synthesize `output.weight` or any architecture-required head tensor.
- Preserve layer metadata slicing, block renumbering, multipart source handling, and early refusal on unreadable or incomplete source tensors.
- The llama.cpp fork must be prepared on `pom-tensor-parallel` before compiling.
- Do not modify CUDA kernels; local validation cannot claim CUDA compilation.
- Stage files one by one and push only after `git pull --rebase origin feat/implantar`.

---

### Task 1: Regression tests for source-head copying and exact size

**Files:**
- Modify: `crates/node/src/models/splitter.rs` tests around `deepseek4_non_final_slice_is_refused_before_output_file_creation`
- Modify: `crates/node/src/models/gguf.rs` tests for `GgufLayout::slice_bytes`
- Modify: `crates/node/src/inference/llm.rs` tests for residual capture target aliases

**Interfaces:**
- Consumes: existing `deepseek4_test_gguf_bytes`, `parse_merged_src`, `split`, and `GgufLayout::slice_bytes`.
- Produces: failing tests proving a non-final DeepSeek4 slice contains every required tensor with source bytes, its preview equals the serialized output, and the capture callback accepts `l_last-N`.

- [x] **Step 1: Write the failing splitter test**

Replace the refusal assertion with a test that calls `split(&whole, &output, 0, 1, true, false, |_| {})`, parses the output tensor table, asserts all six required names are present, and compares each selected head payload with the corresponding source payload using the existing tensor-reading helpers.

- [x] **Step 2: Run the focused Rust test and verify RED**

Run: `rtk cargo test -p node --bin pom models::splitter::tests::deepseek4_non_final_slice_carries_the_complete_output_head -- --exact`

Expected: FAIL because `validate_slice_tensors` currently refuses every non-final DeepSeek4 range.

- [x] **Step 3: Add the exact serialized-size assertion**

Extend the DeepSeek4 layout test so it calls `layout.slice_bytes(0, 0, false)`, generates the non-final file, and asserts the estimate equals the output file length and is greater than the same layer payload without the required output tensors.

- [x] **Step 4: Add the capture alias regression**

Add a unit test for the capture-name predicate that accepts the existing `l_out-19` target and the DeepSeek4 `l_last-19` target while rejecting a different layer or unrelated tensor.

- [x] **Step 5: Run the focused tests and record RED**

Run: `rtk cargo test -p node --bin pom models::splitter::tests::deepseek4_non_final_slice_carries_the_complete_output_head models::splitter::tests::deepseek4_layout_prices_non_final_head inference::llm::tests::capture_target_aliases -- --exact`

Expected: the new tests fail for the missing copied heads, missing estimator bytes, and alias predicate.

### Task 2: Implement generic required-head selection and safe capture alias

**Files:**
- Modify: `crates/node/src/models/gguf.rs` required tensor selection and serialized size calculation
- Modify: `crates/node/src/models/splitter.rs` tensor selection and module documentation
- Modify: `crates/node/src/inference/llm.rs` capture target matching and initialization

**Interfaces:**
- Consumes: the failing tests from Task 1 and `required_output_tensors(architecture)`.
- Produces: non-final slices with real required heads, estimator parity, and residual capture for both callback naming conventions.

- [x] **Step 1: Implement one shared selection rule**

Add a helper in `gguf.rs` that identifies required non-block output tensors for an architecture. Keep `token_embd.weight` in the existing all-slice path and keep the intermediate identity `output_norm.weight`; select every other required tensor for non-final slices. Make `is_output_tensor` and layout selection use the architecture registry rather than a DeepSeek4 literal.

- [x] **Step 2: Make the splitter copy the selected real tensors**

Update `want_tensor` to use the shared selection rule. Retain the source-byte and nonzero-size validation before destination creation. Keep the synthetic `output_norm.weight` only for the intermediate norm policy, and assert no required source head is replaced by fake bytes.

- [x] **Step 3: Make the serialized estimator mirror the splitter**

Update `layout_wants_tensor` and `SerializedLayout::slice_bytes` to use the same shared rule, adding aligned headers and payload bytes for required intermediate heads. Keep final slices unchanged.

- [x] **Step 4: Accept both residual callback names**

Replace the single hard-coded capture-name comparison with a helper or target set that matches `l_out-N` and `l_last-N` for the requested local last layer. Keep the callback's compute-phase return value `true` so it never truncates the graph after capture.

- [x] **Step 5: Run focused tests and verify GREEN**

Run: `rtk cargo test -p node --bin pom models::splitter::tests::deepseek4_non_final_slice_carries_the_complete_output_head models::splitter::tests::deepseek4_layout_prices_non_final_head inference::llm::tests::capture_target_aliases -- --exact`

Expected: PASS with source-byte equality, estimator/output parity, and both callback aliases accepted.

### Task 3: Surface exact head overhead in the slicing API and UI

**Files:**
- Modify: `crates/node/src/models/plan.rs` `SizePlan` construction
- Modify: `crates/node/src/backend/webui.rs` slice-plan JSON construction
- Modify: `apps/frontend/src/lib/types.ts` `SizePlan`
- Modify: `apps/frontend/src/routes/SliceModels.tsx` plan summary
- Modify: `apps/frontend/src/i18n/locales.ts` English and pt-BR slice messages
- Test: `apps/frontend/src/routes/SliceModels.test.tsx` or the existing route test file if present

**Interfaces:**
- Consumes: backend `SizePlan` byte arithmetic and the existing `slice.size`/`slice.actualSize` display.
- Produces: a typed optional head-overhead byte field and a localized visible explanation for non-final architecture-required heads.

- [x] **Step 1: Write the failing UI/API contract test**

Add a fixture `SizePlan` with a nonzero `required_head_bytes` value and assert the slicing screen renders the localized head-cost message alongside the total size. Assert the English and pt-BR message tables both contain the new key.

- [x] **Step 2: Run the focused frontend test and verify RED**

Run: `cd apps/frontend && rtk npm test -- SliceModels`

Expected: FAIL because `SizePlan` has no head-overhead field and the screen does not render the message.

- [x] **Step 3: Add the backend field from exact layout arithmetic**

Compute the non-final required-head payload total from the serialized layout, excluding the already-present embedding and synthetic norm, and serialize it as `required_head_bytes` for size-driven and explicit-range previews.

- [x] **Step 4: Render the localized cost**

Add both locale messages, extend the TypeScript type, and show the formatted byte value only when the backend reports a positive value. Keep the existing total and per-shard sizes authoritative.

- [x] **Step 5: Run frontend tests and build**

Run: `cd apps/frontend && rtk npm test -- SliceModels && rtk npm run build`

Expected: PASS and a successful production bundle.

### Task 4: Documentation and llama.cpp preparation

**Files:**
- Modify: `crates/node/src/models/AGENTS.md`
- Modify: `docs/subsystems/fatiamento.md`
- Modify: `docs/subsystems/web-ui.md`
- Modify: `apps/frontend/src/routes/AGENTS.md` if the route contract changes
- Modify: `apps/frontend/src/i18n/AGENTS.md` if locale keys change
- Modify: `docs/AGENTS.md` if the new spec/plan convention needs a pointer

**Interfaces:**
- Consumes: implemented source-selection, estimator, API, and UI behavior.
- Produces: pt-BR documentation describing current behavior, measured disk cost, loader execution order, and the remaining refusal conditions.

- [x] **Step 1: Update the model and subsystem maps**

Document that required output tensors are copied from the source into non-final slices, that the output head still executes after residual capture, and that exact serialized size includes those tensors.

- [x] **Step 2: Update the UI contract maps**

Document the new plan byte field and localized disclosure on the slicing screen, without editing generated `apps/frontend/src/docs` files.

- [x] **Step 3: Prepare the llama.cpp fork**

Run: `rtk git -C llama.cpp switch pom-tensor-parallel && rtk git -C llama.cpp pull --ff-only origin pom-tensor-parallel && rtk git -C llama.cpp log -1 --oneline`

Expected: the local fork is on the required branch and fast-forwarded. If the required upstream path is unavailable, record the concrete blocker instead of claiming a compiled result.

### Task 5: Full verification, explicit staging, commit, and push

**Files:**
- Stage only the exact files changed by Tasks 1-4, named individually.

**Interfaces:**
- Consumes: all passing Rust/frontend verification and the fork preparation result.
- Produces: one commit pushed to `origin/feat/implantar`.

- [ ] **Step 1: Run Rust verification**

Run: `rtk cargo test -p node --bin pom models::splitter::tests -- --nocapture`

Then run: `rtk cargo test -p node --bin pom inference::llm::tests::capture_target_aliases`

Expected: exit code 0 with no failed tests.

- [ ] **Step 2: Run the workspace checks**

Run: `rtk cargo test --workspace`

Expected: exit code 0. Do not run CUDA compilation on this Mac; report that CUDA remains pending if the source changes require it.

- [x] **Step 3: Inspect the diff and stage named files**

Run `rtk git diff --check`, `rtk git diff --stat`, and `rtk git status --short`. Stage each changed file with individual `git add <path>` commands. Never use `git add -A` or `git commit -a`.

- [x] **Step 4: Commit the verified work**

Run: `rtk git commit -m "fix: copy required heads into intermediate slices"`

- [x] **Step 5: Rebase and push the shared branch**

Run: `rtk git pull --rebase origin feat/implantar` followed by `rtk git push origin HEAD:feat/implantar`.

Expected: the push succeeds without force. If another worker advances the branch between commands, repeat the rebase and push.
