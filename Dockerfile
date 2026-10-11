# syntax=docker/dockerfile:1
ARG CUDA_VERSION=13.4.1
ARG CUDA_RUNTIME_FLAVOR=runtime

FROM nvidia/cuda:${CUDA_VERSION}-devel-ubuntu24.04 AS build-base

ARG CUDA_VERSION

ENV DEBIAN_FRONTEND=noninteractive

# Install the native, CUDA/Vulkan and Node system dependencies once in the base.
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
    git \
    openssh-client \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Frontend npm dependencies live in their own stage so package-lock changes
# cannot invalidate the Rust builder.

# ── Rust builder ──────────────────────────────────────────────────────────────
FROM build-base AS builder

# Install the toolchain before source COPYs: application edits should reuse
# these layers instead of downloading rustup/stable again.
ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    PATH=/root/.cargo/bin:$PATH
ARG RUST_TOOLCHAIN=stable
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain ${RUST_TOOLCHAIN} --no-modify-path && \
    rustup component add rustfmt

# kache is the compiler cache for rustc and the build-script C/C++ (the fork's
# CMake build). Installed right after the toolchain so source edits reuse the
# layer; `kache init` runs in the build step below, next to the sources.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    cargo install --locked kache

ARG POM_LLAMA_CPP_MODE=release
ARG POM_LLAMA_CPP_RELEASE_TAG=""
ARG POM_LLAMA_CPP_RELEASE_REPOSITORY=pieceofmindcloud/llama.cpp

# This release layer depends only on its tag and repository, not POM/Rust sources.
RUN set -eu; \
    mkdir -p /build/llama-runtime/common /build/llama-runtime/cuda-vulkan /build/llama.cpp; \
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
          "${release_url}/llama-${POM_LLAMA_CPP_RELEASE_TAG}-bin-ubuntu-cuda-13.4-x64.tar.gz"; \
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
        cp -a /build/llama-packages/cpu/libggml-cpu-*.so* /build/llama-runtime/cuda-vulkan/; \
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
# llama-cpp-sys-2 and llama-engine come from pom-plugin-llama.cpp (a git dependency
# pinned in Cargo.toml). Its build reads this copied tree through LLAMA_SOURCE.
COPY --from=llama-fork . /build/llama.cpp/

# ── 1b. the llama engine from a published release ─────────────────────────────
# POM_PLUGIN_LLAMA_RELEASE (a tag of pom-plugin-llama.cpp, e.g. v0.1.0) makes the
# build use that release's source archive instead of fetching the locked commit
# over SSH. The archive is downloaded with the token of the `pom_gh_token` secret
# (optional, only needed here). The layer depends on the tag alone, so it is
# reused across builds of the same release. Its files keep the archive's mtimes,
# so Cargo's fingerprints stay valid while the release does not change.
ARG POM_PLUGIN_LLAMA_RELEASE=""
RUN --mount=type=secret,id=pom_gh_token,required=false \
    if [ -n "$POM_PLUGIN_LLAMA_RELEASE" ]; then \
      apt-get update && apt-get install -y --no-install-recommends gh && rm -rf /var/lib/apt/lists/* && \
      GH_TOKEN="$(cat /run/secrets/pom_gh_token)" gh release download "$POM_PLUGIN_LLAMA_RELEASE" \
        -R pieceofmindcloud/pom-plugin-llama.cpp -p 'pom-plugin-llama.cpp-*.tar.gz' -D /tmp/engine && \
      mkdir -p /build/pom-plugin-llama.cpp && \
      tar -xzf /tmp/engine/pom-plugin-llama.cpp-*.tar.gz --strip-components=1 -C /build/pom-plugin-llama.cpp && \
      rm -rf /tmp/engine; \
    fi

# ── 2. the backend ────────────────────────────────────────────────────────────
# `brand.json` and `assets/` are inputs of the Rust build, not only of the
# interface: `crates/brand` embeds the first with `include_str!` and
# `backend/auth.rs` embeds the logo with `include_bytes!`.
COPY Cargo.toml Cargo.lock brand.json ./
COPY .cargo/ .cargo/
COPY assets/ assets/
COPY crates/ crates/
# With a release, point both git dependencies at the extracted archive (only in
# this image's copy of the manifests; the repository keeps the git pins).
RUN if [ -n "$POM_PLUGIN_LLAMA_RELEASE" ]; then \
      sed -i -E 's#^llama-cpp-sys-2 = \{ git = "[^"]*", rev = "[0-9a-f]+" \}#llama-cpp-sys-2 = { path = "/build/pom-plugin-llama.cpp/vendor/llama-cpp-sys-2" }#' Cargo.toml && \
      sed -i -E 's#^llama-engine = \{ git = "[^"]*", rev = "[0-9a-f]+" \}#llama-engine = { path = "/build/pom-plugin-llama.cpp/crates/llama-engine" }#' crates/node/Cargo.toml && \
      grep -q 'path = "/build/pom-plugin-llama.cpp' Cargo.toml && \
      grep -q 'path = "/build/pom-plugin-llama.cpp' crates/node/Cargo.toml; \
    fi

# Fork cache identity passed from the host by scripts/build-docker.sh: the
# release tag in prebuilt mode, or the local commit plus source delta in source
# mode. The latter needs a clean native tree because CMake collects model files
# with a plain GLOB; a stale cache can omit a new model object.
# Native build arguments are scoped below source COPYs, so they cannot invalidate
# source layers or the independent frontend stage.
ARG POM_CUDA_ARCHITECTURES="86;120"
ENV CMAKE_CUDA_ARCHITECTURES=${POM_CUDA_ARCHITECTURES}
ARG LLAMA_CPP_CACHE_KEY="unknown"
ARG POM_CUDA_ARCH_CACHE_KEY="86-120"
# kache keeps its store in `$XDG_CACHE_HOME/kache`. Pointing it at the target
# cache mount persists the store across builds, under the same cache key as the
# Cargo output, and keeps store and targets on one filesystem so kache can link
# cached artifacts instead of copying them.
ENV XDG_CACHE_HOME=/build/target
# Build one executable with all runtime backends. GGML CPU/Vulkan/CUDA modules
# load dynamically; POM's custom CUDA MoE kernels are linked into this executable.
# `kache init` sets Cargo's `rustc-wrapper` and the build-script C/C++ `[env]`
# keys; `--no-service` because a build container has no service manager.
# The engine repository is private and fetched over SSH: the build forwards the
# host's agent (`docker compose build` with `ssh: [default]`, see
# docker-compose.common.yml). github.com's host key is pinned here.
RUN mkdir -p -m 0700 /root/.ssh && ssh-keyscan github.com >> /root/.ssh/known_hosts
RUN --mount=type=ssh \
    --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/build/target,id=pom-target-v2-${LLAMA_CPP_CACHE_KEY}-${CUDA_VERSION}-${POM_CUDA_ARCH_CACHE_KEY}-${RUST_TOOLCHAIN},sharing=locked \
    if [ "$POM_LLAMA_CPP_MODE" = release ]; then \
      export LLAMA_SOURCE=/build/llama.cpp \
        POM_LLAMA_CPP_PREBUILT_LIB_DIR=/build/llama-prebuilt/lib \
        LLAMA_BUILD_SHARED_LIBS=1; \
    else \
      export LLAMA_SOURCE=/build/llama.cpp; \
      unset POM_LLAMA_CPP_PREBUILT_LIB_DIR LLAMA_BUILD_SHARED_LIBS; \
    fi; \
    export CUDAARCHS="$POM_CUDA_ARCHITECTURES"; \
    kache init --no-service --yes && \
    CARGO_TARGET_DIR=/build/target/cuda-vulkan cargo build --release --features cuda,vulkan && \
    cp /build/target/cuda-vulkan/release/pom /build/pom && \
    CARGO_TARGET_DIR=/build/target/plugin-host cargo build --release -p pom-plugin-host && \
    cp /build/target/plugin-host/release/pom-plugin-host /build/pom-plugin-host

# ── Frontend builder ──────────────────────────────────────────────────────────
# This is a sibling of the Rust builder, not its descendant: fork/toolchain/CUDA
# build ARGs therefore cannot invalidate the frontend build cache.
FROM build-base AS frontend

COPY apps/frontend/package.json apps/frontend/package-lock.json apps/frontend/
RUN --mount=type=cache,target=/root/.npm \
    cd apps/frontend && npm ci --no-audit --no-fund

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
# The operating system of both images: nothing here depends on POM sources or
# on the commit being built, so these layers are built once and reused.
FROM nvidia/cuda:${CUDA_VERSION}-${CUDA_RUNTIME_FLAVOR}-ubuntu24.04 AS runtime-os

ENV DEBIAN_FRONTEND=noninteractive
ENV LD_LIBRARY_PATH=/usr/local/lib/pom/common:/usr/local/cuda/lib64:/usr/local/cuda/compat:/usr/lib/x86_64-linux-gnu:/usr/local/nvidia/lib:/usr/local/nvidia/lib64
# `libvulkan1` is only the loader. Without an ICD it enumerates zero devices,
# so the Vulkan-compiled binary silently falls back to CPU on an AMD box and
# the Radeon shows almost no VRAM in use. `mesa-vulkan-drivers` provides the
# RADV ICD (/usr/share/vulkan/icd.d/radeon_icd.*.json + libvulkan_radeon.so)
# that `sysmon::icd_amd()` also looks for. `vulkan-tools` ships `vulkaninfo`
# for `docker exec <container> vulkaninfo --summary` when a card is missing.
# `iproute2` provides `ip`, which `addrs::local_addrs` runs to list this
# node's own addresses; without it the node announces none, not even its
# public IPv6, and remote peers end up dialing the LAN address relayed by
# local peers.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    iproute2 \
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
# The NVIDIA runtime adds the host's device-owning groups to the processes
# (for example render, gid 992 on the ksia machine), which have no name inside the
# image. The sudo hint in Ubuntu's /etc/bash.bashrc runs `groups` in every
# interactive shell and used to print "groups: cannot find name for group id 992" in the
# plugin terminals. The hint stays; only the warning goes away.
RUN sed -i 's/\$(groups)/$(groups 2>\/dev\/null)/' /etc/bash.bashrc \
    && grep -q 'groups 2>/dev/null' /etc/bash.bashrc

# ── Development base ──────────────────────────────────────────────────────────
# The toolchain a plugin is built with inside a running node (Rust/Cargo, the
# native compilers, Node). It sits on the operating system, before anything of
# POM is copied in, so a POM build reuses these layers instead of installing
# the toolchain again; only a change of RUST_TOOLCHAIN or of this list reruns
# them. The distribution image never includes this stage.
FROM runtime-os AS dev-os
ARG RUST_TOOLCHAIN=stable
ENV CARGO_HOME=/opt/cargo \
    RUSTUP_HOME=/opt/rustup \
    PATH=/opt/cargo/bin:$PATH
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    clang \
    cmake \
    curl \
    git \
    jq \
    libclang-dev \
    libssl-dev \
    nodejs \
    npm \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain ${RUST_TOOLCHAIN} --no-modify-path \
    && chmod -R a+rX /opt/cargo /opt/rustup

# ── Distribution image ────────────────────────────────────────────────────────
FROM runtime-os AS runtime-base

ARG POM_LLAMA_CPP_MODE=release

# Host build metadata changes only the runtime-derived stages, never Cargo/npm.
ARG POM_BUILD_COMMIT=""
ENV POM_BUILD_COMMIT=${POM_BUILD_COMMIT}
ENV POM_HOME=/app
ENV POM_PLUGIN_HOST=/usr/local/bin/pom-plugin-host
WORKDIR /app
EXPOSE 42777/udp
EXPOSE 8080/tcp
EXPOSE 8181/tcp
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]

# One universal POM executable serves NVIDIA, Radeon and CPU-only nodes. The
# matching CPU, Vulkan and CUDA backend modules sit beside that executable.
FROM runtime-base AS runtime
COPY --from=builder /build/llama-runtime/ /usr/local/lib/pom/
COPY --from=builder /build/pom /usr/local/bin/pom
COPY --from=builder /build/pom-plugin-host /usr/local/bin/pom-plugin-host
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN if [ "$POM_LLAMA_CPP_MODE" != release ]; then \
      rm -rf /usr/local/lib/pom; \
    else \
      rm -f /usr/local/lib/pom/.keep; \
    fi && \
    mkdir -p /usr/local/lib/pom/common /usr/local/lib/pom/cuda-vulkan && \
    mv /usr/local/bin/pom /usr/local/lib/pom/cuda-vulkan/pom && \
    ln -s /usr/local/lib/pom/cuda-vulkan/pom /usr/local/bin/pom && \
    chmod 0755 /usr/local/bin/docker-entrypoint.sh && \
    if ldd /usr/local/bin/pom | grep 'not found' | grep -v 'libcuda.so.1 => not found' | grep -q .; then \
      echo "/usr/local/bin/pom has unresolved runtime libraries besides the host driver" >&2; \
      ldd /usr/local/bin/pom >&2; \
      exit 1; \
    fi && \
    if [ "$POM_LLAMA_CPP_MODE" = release ]; then \
      for library in /usr/local/lib/pom/cuda-vulkan/libggml-cpu-*.so* /usr/local/lib/pom/cuda-vulkan/libggml-vulkan*.so*; do \
        [ -f "$library" ] || continue; \
        if ldd "$library" | grep -q 'not found'; then \
          echo "$library has unresolved runtime libraries" >&2; \
          ldd "$library" >&2; \
          exit 1; \
        fi; \
      done; \
      for library in /usr/local/lib/pom/cuda-vulkan/libggml-cuda*.so*; do \
        [ -f "$library" ] || continue; \
        if ldd "$library" | grep 'not found' | grep -v 'libcuda.so.1 => not found' | grep -q .; then \
          echo "$library has unresolved runtime libraries besides the host driver" >&2; \
          ldd "$library" >&2; \
          exit 1; \
        fi; \
      done; \
    fi

# Keep the UI bundle as the final image layer: frontend-only edits replace this
# COPY without invalidating backend assembly or rerunning Cargo.
COPY --from=frontend /build/apps/frontend/dist /app/apps/frontend/dist

# ── Development image ─────────────────────────────────────────────────────────
# The distribution image's files on top of the development base. Everything of
# POM is copied from `runtime`, already assembled and checked there, so the two
# images run the same executable and only these COPY layers change per build.
FROM dev-os AS dev-runtime
ARG POM_BUILD_COMMIT=""
ENV POM_BUILD_COMMIT=${POM_BUILD_COMMIT}
ENV POM_HOME=/app
ENV POM_PLUGIN_HOST=/usr/local/bin/pom-plugin-host
WORKDIR /app
EXPOSE 42777/udp
EXPOSE 8080/tcp
EXPOSE 8181/tcp
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
COPY --from=runtime /usr/local/lib/pom/ /usr/local/lib/pom/
COPY --from=runtime /usr/local/bin/pom-plugin-host /usr/local/bin/docker-entrypoint.sh /usr/local/bin/
RUN ln -s /usr/local/lib/pom/cuda-vulkan/pom /usr/local/bin/pom
COPY --from=runtime /app/apps/frontend/dist /app/apps/frontend/dist
