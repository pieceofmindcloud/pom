# syntax=docker/dockerfile:1
ARG CUDA_VERSION=13.2.1
ARG CUDA_RUNTIME_FLAVOR=runtime

FROM nvidia/cuda:${CUDA_VERSION}-devel-ubuntu24.04 AS build-base

ARG CUDA_VERSION

ENV DEBIAN_FRONTEND=noninteractive

# Download and import the NodeSource security keys and repository (Node 22 example)

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    libclang-dev \
    clang \
    ca-certificates \
    curl \
    pkg-config \
    libssl-dev \
    gnupg \
    mold \
    nodejs \
    npm \
    libshaderc-dev \
    libvulkan-dev \
    glslc \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# node_modules is shared by the independent Rust and frontend stages. Rust or
# CUDA build inputs cannot invalidate this layer.
COPY apps/frontend/package.json apps/frontend/package-lock.json apps/frontend/
RUN --mount=type=cache,target=/root/.npm \
    cd apps/frontend && npm ci --no-audit --no-fund

# ── Rust builder ──────────────────────────────────────────────────────────────
FROM build-base AS builder

ARG POM_LLAMA_CPP_MODE=release
ARG POM_LLAMA_CPP_RELEASE_TAG=""
ARG POM_LLAMA_CPP_RELEASE_REPOSITORY=pereirajair/llama.cpp

# This release layer depends only on its tag and repository, not POM/Rust sources.
RUN set -eu; \
    mkdir -p /build/llama-runtime/common /build/llama-runtime/cuda-vulkan \
      /build/llama-runtime/vulkan /build/llama-runtime/cpu /build/llama.cpp; \
    touch /build/llama-runtime/.keep; \
    case "$POM_LLAMA_CPP_MODE" in \
      source) ;; \
      release) \
        if [ -z "$POM_LLAMA_CPP_RELEASE_TAG" ]; then \
          echo "POM_LLAMA_CPP_RELEASE_TAG is required in release mode; use scripts/build-docker*.sh." >&2; \
          exit 1; \
        fi; \
        release_url="https://github.com/${POM_LLAMA_CPP_RELEASE_REPOSITORY}/releases/download/${POM_LLAMA_CPP_RELEASE_TAG}"; \
        mkdir -p /build/llama-packages/cpu /build/llama-packages/cuda /build/llama-packages/vulkan \
          /build/llama-prebuilt/lib; \
        curl -fL --retry 3 -o /tmp/llama-cpu.tar.gz \
          "${release_url}/llama-${POM_LLAMA_CPP_RELEASE_TAG}-bin-ubuntu-x64.tar.gz"; \
        curl -fL --retry 3 -o /tmp/llama-cuda.tar.gz \
          "${release_url}/llama-${POM_LLAMA_CPP_RELEASE_TAG}-bin-ubuntu-cuda-13.2-x64.tar.gz"; \
        curl -fL --retry 3 -o /tmp/llama-vulkan.tar.gz \
          "${release_url}/llama-${POM_LLAMA_CPP_RELEASE_TAG}-bin-ubuntu-vulkan-x64.tar.gz"; \
        curl -fL --retry 3 -o /tmp/llama-source.tar.gz \
          "https://github.com/${POM_LLAMA_CPP_RELEASE_REPOSITORY}/archive/refs/tags/${POM_LLAMA_CPP_RELEASE_TAG}.tar.gz"; \
        tar -xzf /tmp/llama-cpu.tar.gz --strip-components=1 -C /build/llama-packages/cpu; \
        tar -xzf /tmp/llama-cuda.tar.gz --strip-components=1 -C /build/llama-packages/cuda; \
        tar -xzf /tmp/llama-vulkan.tar.gz --strip-components=1 -C /build/llama-packages/vulkan; \
        tar -xzf /tmp/llama-source.tar.gz --strip-components=1 -C /build/llama.cpp; \
        for name in libllama.so libggml.so libggml-base.so libllama-common.so; do \
          test -f "/build/llama-packages/cpu/$name" || { echo "missing $name in CPU release package" >&2; exit 1; }; \
          cp -L "/build/llama-packages/cpu/$name" "/build/llama-prebuilt/lib/$name"; \
        done; \
        for name in libllama.so libggml.so libggml-base.so libllama-common.so; do \
          cp -a "/build/llama-packages/cpu/$name"* /build/llama-runtime/common/; \
        done; \
        for variant in cpu vulkan cuda-vulkan; do \
          cp -a /build/llama-packages/cpu/libggml-cpu-*.so* "/build/llama-runtime/$variant/"; \
        done; \
        cp -a /build/llama-packages/vulkan/libggml-vulkan*.so* /build/llama-runtime/vulkan/; \
        cp -a /build/llama-packages/vulkan/libggml-vulkan*.so* /build/llama-runtime/cuda-vulkan/; \
        cp -a /build/llama-packages/cuda/libggml-cuda*.so* /build/llama-runtime/cuda-vulkan/; \
        ;; \
      *) echo "POM_LLAMA_CPP_MODE must be 'release' or 'source'" >&2; exit 1 ;; \
    esac

# ── Build inputs, least volatile first ────────────────────────────────────────
#
# Every COPY names only the paths consumed by that stage; there is no
# `COPY . .`. The previous whole-context copy made an unrelated
# `infer_server_*.log` or a frontend-only edit rerun Cargo as well. The explicit
# inputs and sibling stages below keep changes from reaching unrelated builds.
#
# The Rust builder and frontend builder are siblings. Their inputs are copied
# only into the stage that consumes them, so a frontend edit never reruns Cargo
# and a fork/toolchain/CUDA-architecture change never rebuilds the UI.
# `scripts/test-docker-build-cache.sh` proves these cases, plus "nothing
# changed" and "only local build artifacts appeared", against this very file
# with a real BuildKit.

# ── 1. the llama.cpp fork ─────────────────────────────────────────────────────
# `llama-fork` is a named build context. It defaults to ./llama.cpp, but
# POM_LLAMA_CPP_SOURCE_DIR can point at another local checkout for source mode.
# The vendored sys crate's symlink resolves to this copied tree in the image.
COPY --from=llama-fork . /build/llama.cpp/
COPY vendor/ vendor/

# ── 2. the backend ────────────────────────────────────────────────────────────
# `brand.json` and `assets/` are inputs of the Rust build, not only of the
# interface: `crates/brand` embeds the first with `include_str!` and
# `backend/auth.rs` embeds the logo with `include_bytes!`.
COPY Cargo.toml Cargo.lock brand.json ./
COPY .cargo/ .cargo/
COPY assets/ assets/
COPY crates/ crates/

# Fork cache identity passed from the host by scripts/build-docker.sh: the
# release tag in prebuilt mode, or the local commit plus source delta in source
# mode. The latter needs a clean native tree because CMake collects model files
# with a plain GLOB; a stale cache can omit a new model object.
ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    PATH=/root/.cargo/bin:$PATH
ARG RUST_TOOLCHAIN=stable
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain ${RUST_TOOLCHAIN} --no-modify-path
RUN rustup component add rustfmt

# Build-only arguments are declared after source COPYs and after rustup so
# changing them invalidates only the Rust build, not source layers or the UI.
ARG POM_CUDA_ARCHITECTURES="86;120"
ENV CMAKE_CUDA_ARCHITECTURES=${POM_CUDA_ARCHITECTURES}
ARG LLAMA_CPP_CACHE_KEY="unknown"
ARG POM_CUDA_ARCH_CACHE_KEY="86-120"

# Cache mounts persist the registry, git checkouts, and incremental build
# artifacts across builds. Separate target directories retain each feature
# set's Cargo fingerprints; the mount id also isolates native CMake output by
# fork, CUDA toolkit, Rust toolchain selector, and architecture.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/build/target,id=pom-target-${LLAMA_CPP_CACHE_KEY}-${CUDA_VERSION}-${POM_CUDA_ARCH_CACHE_KEY}-${RUST_TOOLCHAIN} \
    if [ "$POM_LLAMA_CPP_MODE" = release ]; then \
      export LLAMA_SOURCE=/build/llama.cpp \
        POM_LLAMA_CPP_PREBUILT_LIB_DIR=/build/llama-prebuilt/lib \
        LLAMA_BUILD_SHARED_LIBS=1; \
    else \
      export LLAMA_SOURCE=/build/llama.cpp; \
      unset POM_LLAMA_CPP_PREBUILT_LIB_DIR LLAMA_BUILD_SHARED_LIBS; \
    fi; \
    export CUDAARCHS="$POM_CUDA_ARCHITECTURES"; \
    CARGO_TARGET_DIR=/build/target/cuda-vulkan cargo build --release --features cuda,vulkan && \
    cp /build/target/cuda-vulkan/release/pom /build/pom && \
    CARGO_TARGET_DIR=/build/target/vulkan cargo build --release --features vulkan && \
    cp /build/target/vulkan/release/pom /build/pom-vulkan && \
    CARGO_TARGET_DIR=/build/target/cpu cargo build --release --no-default-features && \
    cp /build/target/cpu/release/pom /build/pom-cpu && \
    command -v readelf >/dev/null && \
    for binary in /build/pom-vulkan /build/pom-cpu; do \
      if readelf -d "$binary" | grep -q 'Shared library: \[libcuda\.so\.1\]'; then \
        echo "$binary must not require libcuda.so.1" >&2; \
        exit 1; \
      fi; \
    done && \
    CARGO_TARGET_DIR=/build/target/plugin-host cargo build --release -p pom-plugin-host && \
    cp /build/target/plugin-host/release/pom-plugin-host /build/pom-plugin-host

# ── Frontend builder ──────────────────────────────────────────────────────────
# This is a sibling of the Rust builder, not its descendant: fork/toolchain/CUDA
# build ARGs therefore cannot invalidate the frontend build cache.
FROM build-base AS frontend

# `npm run build` runs `build-brand.mjs` and `build-docs.mjs` first: they read
# `brand.json`, `assets/`, the subsystem READMEs, root AGENTS.md/README.md,
# `scripts/AGENTS.md` and every AGENTS.md under `crates/` and `apps/frontend/`.
COPY AGENTS.md README.md brand.json ./
COPY assets/ assets/
# build-docs.mjs reads crate AGENTS.md files, not Rust sources; copying only
# these documents prevents backend code edits from invalidating the UI bundle.
COPY --parents ./crates/**/AGENTS.md ./
COPY docs/ docs/
COPY scripts/AGENTS.md scripts/
COPY apps/frontend/ apps/frontend/
RUN cd apps/frontend && npm run build

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM nvidia/cuda:${CUDA_VERSION}-${CUDA_RUNTIME_FLAVOR}-ubuntu24.04 AS runtime

ARG POM_LLAMA_CPP_MODE=release

ENV DEBIAN_FRONTEND=noninteractive
ENV LD_LIBRARY_PATH=/usr/local/lib/pom/common:/usr/local/cuda/lib64:/usr/local/cuda/compat:/usr/lib/x86_64-linux-gnu:/usr/local/nvidia/lib:/usr/local/nvidia/lib64
# `libvulkan1` is only the loader. Without an ICD it enumerates zero devices,
# so the Vulkan-compiled binary silently falls back to CPU on an AMD box and
# the Radeon shows almost no VRAM in use. `mesa-vulkan-drivers` provides the
# RADV ICD (/usr/share/vulkan/icd.d/radeon_icd.*.json + libvulkan_radeon.so)
# that `sysmon::icd_amd()` also looks for. `vulkan-tools` ships `vulkaninfo`
# for `docker exec <container> vulkaninfo --summary` when a card is missing.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libstdc++6 \
    libgomp1 \
    libssl3t64 \
    libvulkan1 \
    mesa-vulkan-drivers \
    vulkan-tools \
    && rm -rf /var/lib/apt/lists/*

# mesa-vulkan-drivers drops an ICD for every Mesa driver (intel, nouveau,
# llvmpipe, ...). Keep only RADV: the NVIDIA card is served by CUDA, and the
# software rasterizer `llvmpipe` is an active hazard here - the Vulkan backend
# would happily "offload" a model onto a CPU device. This also makes the AMD
# index the Vulkan index by construction (sysmon::icd_amd / the ksia note).
RUN find /usr/share/vulkan/icd.d -type f ! -name 'radeon_icd*.json' -delete

# Release mode stages backend modules beside the matching executable. The core
# shared libraries are common and discovered through LD_LIBRARY_PATH.
COPY --from=builder /build/llama-runtime/ /usr/local/lib/pom/

# The default binary is CUDA+Vulkan. Radeon-only and CPU services select the
# sibling variants so their loader never requires the host's libcuda.so.1.
COPY --from=builder /build/pom /usr/local/bin/pom
COPY --from=builder /build/pom-vulkan /usr/local/bin/pom-vulkan
COPY --from=builder /build/pom-cpu /usr/local/bin/pom-cpu
COPY --from=builder /build/pom-plugin-host /usr/local/bin/pom-plugin-host
COPY --from=frontend /build/apps/frontend/dist /app/apps/frontend/dist
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN if [ "$POM_LLAMA_CPP_MODE" = release ]; then \
      mv /usr/local/bin/pom /usr/local/lib/pom/cuda-vulkan/pom && \
      mv /usr/local/bin/pom-vulkan /usr/local/lib/pom/vulkan/pom-vulkan && \
      mv /usr/local/bin/pom-cpu /usr/local/lib/pom/cpu/pom-cpu && \
      ln -s /usr/local/lib/pom/cuda-vulkan/pom /usr/local/bin/pom && \
      ln -s /usr/local/lib/pom/vulkan/pom-vulkan /usr/local/bin/pom-vulkan && \
      ln -s /usr/local/lib/pom/cpu/pom-cpu /usr/local/bin/pom-cpu && \
      rm -f /usr/local/lib/pom/.keep; \
    else \
      rm -rf /usr/local/lib/pom && mkdir -p /usr/local/lib/pom/common; \
    fi && \
    chmod 0755 /usr/local/bin/docker-entrypoint.sh && \
    for binary in /usr/local/bin/pom-vulkan /usr/local/bin/pom-cpu; do \
      if ldd "$binary" | grep -q 'not found'; then \
        echo "$binary has unresolved runtime libraries" >&2; \
        ldd "$binary" >&2; \
        exit 1; \
      fi; \
    done && \
    if ldd /usr/local/bin/pom | grep 'not found' | grep -v 'libcuda.so.1 => not found' | grep -q .; then \
      echo "/usr/local/bin/pom has unresolved runtime libraries besides the host driver" >&2; \
      ldd /usr/local/bin/pom >&2; \
      exit 1; \
    fi && \
    if [ "$POM_LLAMA_CPP_MODE" = release ]; then \
      for library in /usr/local/lib/pom/vulkan/libggml-vulkan*.so* /usr/local/lib/pom/cpu/libggml-cpu-*.so*; do \
        if ldd "$library" | grep -q 'not found'; then \
          echo "$library has unresolved runtime libraries" >&2; \
          ldd "$library" >&2; \
          exit 1; \
        fi; \
      done; \
      for library in /usr/local/lib/pom/cuda-vulkan/libggml-cuda*.so*; do \
        if ldd "$library" | grep 'not found' | grep -v 'libcuda.so.1 => not found' | grep -q .; then \
          echo "$library has unresolved runtime libraries besides the host driver" >&2; \
          ldd "$library" >&2; \
          exit 1; \
        fi; \
      done; \
    fi

# P2P UDP, HTTP gateway, inference server
EXPOSE 42777/udp
EXPOSE 8080/tcp
EXPOSE 8181/tcp

# project_root() would otherwise need models/ or pom.json in /app to
# recognize it; this makes the resolution explicit instead of incidental.
ENV POM_HOME=/app
ENV POM_PLUGIN_HOST=/usr/local/bin/pom-plugin-host

WORKDIR /app
# The commit is runtime metadata. Keeping this ARG out of both build stages means
# a new app commit can update the stamp without invalidating Cargo or npm layers.
ARG POM_BUILD_COMMIT=""
ENV POM_BUILD_COMMIT=${POM_BUILD_COMMIT}
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
