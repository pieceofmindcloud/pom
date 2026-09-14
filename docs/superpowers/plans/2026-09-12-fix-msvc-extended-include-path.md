# Fix MSVC extended include path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `cc-rs` pass MSVC-compatible include paths when `LLAMA_SOURCE` resolves to a Windows `\\?\` path.

**Architecture:** Keep the extended path for bindgen and CMake, which already consume it successfully. Normalize only paths handed to `cc::Build`, converting drive paths to `D:\...` and extended UNC paths to `\\server\share\...` before `cl.exe` sees `/I`.

**Tech Stack:** Rust build script, `cc` crate, MSVC path conventions, Cargo tests.

**Spec:** Current task report: Windows `cl.exe` cannot open `llama.h` from a `\\?\D:\...\include` path even though bindgen resolves it; no GitHub Actions run is allowed.

## Global Constraints

- Do not dispatch or rerun GitHub Actions.
- Normalize paths only at the `cc::Build` boundary; do not change bindgen or CMake source selection.
- Preserve ordinary Unix paths and ordinary Windows paths unchanged.
- Cover drive-letter and UNC extended prefixes with unit tests.

---

### Task 1: Characterize the failure

**Files:**
- Read: `vendor/llama-cpp-sys-2/build.rs`
- Read: `vendor/llama-cpp-sys-2/src/build_support.rs`
- Read: the locked `cc` crate source under `$HOME/.cargo/registry/src`

- [x] **Step 1: Trace the path**

`canonicalize()` can yield `\\?\D:\...`; `source_include_dirs()` derives paths from it; `cc-rs` stores and forwards those paths directly as MSVC `/I` arguments.

- [x] **Step 2: Form the hypothesis**

Bindgen succeeds because Clang accepts the extended prefix, while MSVC rejects it in `/I`. The cache race hypothesis is not needed because the same file was already found by bindgen before `cc-rs` runs.

### Task 2: Add the regression test

**Files:**
- Modify: `vendor/llama-cpp-sys-2/src/build_support.rs`
- Test: the `build_support` test module

- [x] **Step 1: Write the failing tests**

Add assertions for drive paths, UNC paths, and unchanged Unix paths:

```rust
assert_eq!(
    build_tool_path(Path::new(r"\\?\D:\a\rdma-p2p\include"), true),
    Path::new(r"D:\a\rdma-p2p\include"),
);
assert_eq!(
    build_tool_path(Path::new(r"\\?\UNC\build\share\include"), true),
    Path::new(r"\\build\share\include"),
);
assert_eq!(
    build_tool_path(Path::new("/worktree/.ci-cache/llama.cpp/include"), false),
    Path::new("/worktree/.ci-cache/llama.cpp/include"),
);
```

- [x] **Step 2: Run the test and verify it fails**

Run `rustc --test vendor/llama-cpp-sys-2/src/build_support.rs -o /tmp/rdma-build-support-msvc-red && /tmp/rdma-build-support-msvc-red`.

Expected: failure because `build_tool_path` does not yet exist.

### Task 3: Normalize every `cc::Build` path

**Files:**
- Modify: `vendor/llama-cpp-sys-2/src/build_support.rs`
- Modify: `vendor/llama-cpp-sys-2/build.rs`

- [x] **Step 1: Implement `build_tool_path(path, is_windows) -> PathBuf`**

Strip `\\?\` from drive-letter paths, convert `\\?\UNC\server\share` to `\\server\share`, and return non-Windows or already-normal paths unchanged.

- [x] **Step 2: Apply it to the common wrapper**

Wrap `llama_include_root` and every `source_include_dirs(&llama_src)` path before passing them to `common_wrapper_build.include(...)`.

- [x] **Step 3: Apply it to mtmd**

Normalize mtmd include directories, the bundled `hash.cpp` and `sha256.c` paths, and every `.cpp` path returned by the mtmd glob before passing them to `cc::Build`.

- [x] **Step 4: Run the focused tests and verify they pass**

Recompile and run `/tmp/rdma-build-support-msvc-red`; all path tests and existing source-preparation tests must pass.

### Task 4: Validate both source layouts

**Files:**
- Test: temporary copied `.ci-cache/llama.cpp` tree
- Verify: `vendor/llama-cpp-sys-2/llama.cpp`

- [x] **Step 1: Run normal local validation**

Run `env -u LLAMA_SOURCE CMAKE_BUILD_PARALLEL_LEVEL=1 CARGO_BUILD_JOBS=1 cargo check --locked`.

- [x] **Step 2: Run external-cache validation**

Copy the source to a temporary `.ci-cache/llama.cpp` tree and run `LLAMA_SOURCE="$cache_root/.ci-cache/llama.cpp" CMAKE_BUILD_PARALLEL_LEVEL=1 CARGO_BUILD_JOBS=1 cargo check --locked`.

- [x] **Step 3: Run repository gates**

Run `cargo test --workspace --locked`, `rustfmt --check vendor/llama-cpp-sys-2/build.rs vendor/llama-cpp-sys-2/src/build_support.rs`, and `git diff --check`.

### Task 5: Document and publish

**Files:**
- Modify: `vendor/llama-cpp-sys-2/AGENTS.md`
- Modify: `docs/subsystems/build-execucao.md`
- Create: this plan file

- [x] **Step 1: Document the MSVC boundary**

State that extended Windows paths remain for bindgen/CMake, while `cc::Build` receives paths without `\\?\`.

- [x] **Step 2: Commit, publish, and verify without CI**

Run `git fetch origin`, `git pull --rebase origin feat/implantar3`, commit with `fix: normalize MSVC llama include paths`, push with `git push origin HEAD:feat/implantar3`, and confirm with `git ls-remote origin refs/heads/feat/implantar3`.
