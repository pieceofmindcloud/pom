# Fix CMake extended source path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ensure the llama.cpp source path passed to the CMake crate is compatible with MSVC on Windows when `canonicalize()` returns a `\\?\\` path.

**Architecture:** Reuse the existing Windows native-tool path normalization at the CMake boundary, while keeping the extended path for bindgen and source inspection. Audit every `llama_src` use to distinguish filesystem observation from external compiler/build-tool arguments.

**Constraints:** Do not run GitHub Actions. Preserve non-Windows paths and ordinary Windows paths. Validate the normal local checkout and an external `.ci-cache/llama.cpp` source tree.

### Task 1: Reproduce and characterize

- [x] Trace `Config::new(&llama_src)` and inspect the locked `cmake` crate to confirm it forwards the source path to the CMake command.
- [x] Audit all `llama_src` uses in `build.rs`; source inspection and Cargo rerun paths may remain extended, but tool arguments must be normalized.

### Task 2: Fix the CMake boundary

- [x] Add a regression assertion for the normalized Windows source path.
- [x] Pass the normalized path to `cmake::Config::new`.
- [x] Update build documentation to cover both `cc::Build` and CMake.

### Task 3: Validate and publish

- [x] Run focused path tests, formatting, and diff checks.
- [x] Run normal and external-cache `cargo check`, then workspace tests. The workspace run had one timing-sensitive prefix-reuse assertion fail; the isolated test passed on rerun.
- [ ] Rebase, commit, push to `feat/implantar3`, confirm the remote SHA, and report that no workflow was dispatched.
