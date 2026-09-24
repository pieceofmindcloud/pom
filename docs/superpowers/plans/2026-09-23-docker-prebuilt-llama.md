# Docker dev build using a pinned llama.cpp release

> Execute tasks in order. No commit, push, or deployment without explicit approval; the user explicitly requested the fork workflow push.

**Goal:** make Ksia/Salem Docker iteration use matching prebuilt llama.cpp libraries by default, with a local source-build override.

**Spec:** `docs/superpowers/specs/2026-09-23-docker-prebuilt-llama-design.md`.

## Tasks

### 1. Prepare a release that can be consumed

- [x] Remove the s390x CPU job from the fork release matrix and release notes.
- [x] Add Ubuntu x64 CUDA 13.2.1 artifacts and release links.
- [x] Validate workflow YAML and diff; push `600d315129e985a1d1edff43af5710fd428f83d4` to `pom-tensor-parallel` as requested. The workflow was not dispatched.

### 2. Add an explicit prebuilt mode to the Rust sys build

- [x] Test required core-library validation and CUDA architecture flags with standalone Rust test harnesses.
- [x] Make `POM_LLAMA_CPP_PREBUILT_LIB_DIR` skip CMake only for Linux shared linking; preserve normal source mode.
- [x] Keep dynamic cudart/driver link dependencies needed by POM-owned CUDA kernels.

### 3. Integrate release/source selection in Docker

- [x] Resolve or accept the exact release tag in the shared build script; source mode accepts any local Git checkout through `POM_LLAMA_CPP_SOURCE_DIR`.
- [x] Download same-tag source and CPU/CUDA/Vulkan packages; validate and stage required libraries/plugins.
- [x] Build POM variants against shared libraries, keep POM CUDA kernels local, and create per-variant runtime directories.
- [x] Set Ksia/Salem Compose build args to release mode by default, with source mode override.

### 4. Document and validate

- [x] Update README, `docs/subsystems/build-execucao.md`, and relevant AGENTS maps.
- [x] Run shell syntax checks, YAML/Compose validation, Docker cache-graph test, formatting, and diff checks.
- [x] Report that a full release-mode image build still requires the assets for `pom-tensor-parallel-b11048-600d315`; the pushed workflow has not run yet.
