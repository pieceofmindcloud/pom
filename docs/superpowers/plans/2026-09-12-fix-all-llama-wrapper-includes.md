# Fix all llama wrapper includes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every native wrapper in `llama-cpp-sys-2` resolve headers from an external `.ci-cache/llama.cpp` checkout on Windows, Unix, macOS, and Docker.

**Architecture:** Wrapper headers use leaf header names, while `build.rs` supplies the exact source subdirectories to bindgen and `cc::Build`. This removes dependence on a compiler accepting a source-root-prefixed include and keeps the selected `LLAMA_SOURCE` as the only source of truth.

**Tech Stack:** Rust build script, bindgen, cc crate, Clang, C++ headers, Cargo tests.

**Spec:** User request in the current task: fix all hardcoded relative wrapper includes after the Windows cache materialization failure, validate `.ci-cache/llama.cpp` offline, and do not run GitHub Actions.

## Global Constraints

- Do not dispatch or rerun GitHub Actions.
- Preserve the normal versioned `vendor/llama-cpp-sys-2/llama.cpp` checkout path and external `LLAMA_SOURCE` behavior.
- Do not edit files inside the vendored `llama.cpp` source tree.
- Use a temporary copied `.ci-cache/llama.cpp` tree for the regression reproduction.
- Update the crate and build subsystem documentation in the same commit if the include contract changes.

---

### Task 1: Capture the complete include surface

**Files:**
- Read: `vendor/llama-cpp-sys-2/wrapper.h`
- Read: `vendor/llama-cpp-sys-2/wrapper_common.h`
- Read: `vendor/llama-cpp-sys-2/wrapper_common.cpp`
- Read: `vendor/llama-cpp-sys-2/wrapper_mtmd.h`
- Read: `vendor/llama-cpp-sys-2/build.rs`

**Interfaces:**
- Consumes: the selected source root `llama_src` and its include subdirectories.
- Produces: a list of every wrapper include requiring either a source-root or subdirectory include path.

- [x] **Step 1: Reproduce the failing mtmd include against a copied cache tree**

Run:

```bash
repro_root=$(mktemp -d /tmp/rdma-mtmd-repro.XXXXXX)
mkdir -p "$repro_root/.ci-cache"
cp -R llama.cpp "$repro_root/.ci-cache/llama.cpp"
set +e
clang -fsyntax-only -x c++ \
  -I "$repro_root/.ci-cache/llama.cpp/include" \
  -I "$repro_root/.ci-cache/llama.cpp/ggml/include" \
  vendor/llama-cpp-sys-2/wrapper_mtmd.h
repro_status=$?
rm -rf "$repro_root"
test "$repro_status" -ne 0
```

Expected: failure at `wrapper_mtmd.h` with `tools/mtmd/mtmd.h file not found`.

### Task 2: Replace directory-qualified wrapper includes

**Files:**
- Modify: `vendor/llama-cpp-sys-2/wrapper_mtmd.h`
- Modify: `vendor/llama-cpp-sys-2/wrapper_common.cpp`
- Modify: `vendor/llama-cpp-sys-2/build.rs`
- Test: `vendor/llama-cpp-sys-2/src/build_support.rs` or the build-script test module

**Interfaces:**
- Consumes: `llama_src` selected by automatic checkout, versioned symlink, or `LLAMA_SOURCE`.
- Produces: bindgen and C++ include flags covering `include`, `ggml/include`, `src`, `common`, `tools/mtmd`, and `vendor`.

- [x] **Step 1: Add a regression assertion for the mtmd include directory**

Test the include-directory helper or build-script configuration so the selected source contributes `tools/mtmd` to bindgen and leaf `mtmd.h` can resolve without a root-prefixed include.

- [x] **Step 2: Run the focused test and verify it fails before the implementation**

Run:

```bash
rustc --test vendor/llama-cpp-sys-2/src/build_support.rs -o /tmp/rdma-build-support-tests
/tmp/rdma-build-support-tests
```

Expected: the new include-contract test fails because the current configuration does not expose `tools/mtmd` to bindgen.

- [x] **Step 3: Implement the smallest include-path fix**

Change `wrapper_mtmd.h` to include `mtmd.h` and `mtmd-helper.h`. Change `wrapper_common.cpp` to use leaf names for headers supplied by explicit `src`, `common`, `include`, and `ggml/include` directories. Add those exact directories, including `tools/mtmd` and `src`, to bindgen and `cc::Build`.

- [x] **Step 4: Run the focused regression test**

Run the same test binary and expect it to pass.

### Task 3: Validate both cache and normal checkout paths

**Files:**
- Test: temporary copied `.ci-cache/llama.cpp` source tree
- Verify: `vendor/llama-cpp-sys-2/llama.cpp` versioned symlink
- Verify: `docs/subsystems/build-execucao.md`
- Verify: `vendor/llama-cpp-sys-2/AGENTS.md`

**Interfaces:**
- Consumes: the updated wrapper include contract and `LLAMA_SOURCE` selection.
- Produces: evidence that bindgen and the C++ bridge resolve all wrappers from the cache tree and the normal Mac checkout.

- [x] **Step 1: Preprocess mtmd and common wrappers from the copied cache tree**

Run Clang syntax-only checks with explicit cache `include`, `ggml/include`, `src`, `common`, `tools/mtmd`, `vendor`, and source-root flags. Expect exit code 0 for `wrapper_mtmd.h` and `wrapper_common.cpp`.

- [x] **Step 2: Run Cargo validation with `LLAMA_SOURCE` set to the copied cache tree**

Run:

```bash
LLAMA_SOURCE="$cache_root/.ci-cache/llama.cpp" CMAKE_BUILD_PARALLEL_LEVEL=1 CARGO_BUILD_JOBS=1 cargo check --locked
```

Expected: exit code 0 and no missing-header error.

- [x] **Step 3: Run the normal local checkout validation**

Run:

```bash
env -u LLAMA_SOURCE CMAKE_BUILD_PARALLEL_LEVEL=1 CARGO_BUILD_JOBS=1 cargo check --locked
```

Expected: exit code 0 using the existing versioned symlink checkout.

- [x] **Step 4: Confirm no hardcoded wrapper paths remain**

Run:

```bash
rg -n '#[[:space:]]*include[[:space:]]*[<"][^>"]*(llama\.cpp/|tools/|ggml/|common/|src/)' \
  vendor/llama-cpp-sys-2 --glob '!llama.cpp/**'
```

Expected: no matches in wrapper source files; Cargo manifest package paths are not wrapper includes.

### Task 4: Document, review, and commit

**Files:**
- Modify: `vendor/llama-cpp-sys-2/AGENTS.md`
- Modify: `docs/subsystems/build-execucao.md`
- Verify: `git diff --check`

**Interfaces:**
- Consumes: the final include contract and validation results.
- Produces: durable documentation, a clean commit, and a branch ready for firstmate.

- [x] **Step 1: Document that wrappers use explicit source subdirectories**

State that external cache and normal checkout use the same `build.rs` include directories, including `tools/mtmd` and the internal `src`/`common` directories.

- [x] **Step 2: Run final validation**

Run the focused tests, both Cargo checks, `git diff --check`, and `git status --short --branch`. Do not run a GitHub Actions workflow.

- [x] **Step 3: Commit the fix**

```bash
git add vendor/llama-cpp-sys-2 docs/subsystems/build-execucao.md docs/superpowers/plans/2026-09-12-fix-all-llama-wrapper-includes.md
git commit -m "fix: make all llama wrapper includes cache-safe"
```
