# Docker dev build with a pinned llama.cpp release

## Goal

Speed up Ksia and Salem Docker iteration by reusing a published build of the POM llama.cpp fork, while retaining a source-build path for uncommitted fork work. POM's own user-facing releases and CI remain source builds.

## Decisions

- The dev Docker build defaults to `POM_LLAMA_CPP_MODE=release`; `source` keeps compiling the checkout under `llama.cpp/`.
- Release mode resolves or accepts one exact tag from the public `pereirajair/llama.cpp` fork. It downloads source at that tag for bindgen and POM shims, plus same-tag Ubuntu x64 CPU, CUDA 13.2, and Vulkan packages. Core shared libraries come from CPU; backend modules come from their matching packages.
- Source mode stages `POM_LLAMA_CPP_SOURCE_DIR` (default `./llama.cpp`) into a temporary named build context, preserving local edits while excluding `.git`, model data, and CMake outputs. Release mode passes only an empty context and does not depend on the local fork's contents.
- `llama-cpp-sys-2` copies the core libraries into `OUT_DIR` and skips CMake only when an explicit prebuilt directory is set. It still builds generated bindings, mtmd, and POM wrappers. POM-owned CUDA MoE kernels continue to compile locally for the requested GPU architectures.
- Runtime executables use separate directories and backend modules: CUDA+Vulkan, Vulkan+CPU, and CPU. Shared core libraries are in a common directory. This keeps CPU/Vulkan executables from loading the CUDA module or requiring the host CUDA driver.
- CUDA development runtime libraries are supplied by the matching CUDA runtime image. Source mode remains available and uses the smaller CUDA base image.
- The fork release workflow drops Ubuntu s390x and publishes an Ubuntu x64 CUDA 13.2.1 package. The currently-running workflow is unaffected; the edited workflow must be pushed and dispatched before the new package exists.

## Invariants

- Release mode never links libraries from a different tag than the headers used by bindgen and POM shims.
- Release mode fails clearly if required assets or core libraries are absent; it does not silently fall back to a potentially mismatched source build.
- Source mode preserves the existing local-fork build behavior.
- CPU and Vulkan-only runtime binaries do not have a direct CUDA driver dependency and do not colocate a CUDA backend module beside their executable.
- Release tags and native cache identities include the exact fork tag; POM commits still do not enter the frontend or Cargo cache keys.

## Out of scope

- Publishing POM runtime images or changing POM GitHub release artifacts.
- Windows, macOS, ARM64, or non-Ubuntu prebuilt dev images.
- Removing the local fork checkout or its source-build option.
