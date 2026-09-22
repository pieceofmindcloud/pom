# syntax=docker/dockerfile:1
ARG CUDA_VERSION=13.2.1

FROM nvidia/cuda:${CUDA_VERSION}-devel-ubuntu24.04 as builder

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

ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    PATH=/root/.cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
RUN rustup component add rustfmt

WORKDIR /build

# ── Build inputs, least volatile first ────────────────────────────────────────
#
# Every COPY below names exactly the paths the step under it reads. There is no
# `COPY . .`, and that is the whole point: with one, a single file anywhere in
# the tree resets the cache key of the Rust build and of the interface build at
# the same time, and both ran again with nothing of theirs changed. Measured on
# the salem checkout with the real context: dropping one `infer_server_*.log`
# next to `crates/node` - a file a node writes by itself when it is started from
# the checkout - was enough to re-run `cargo build`. A frontend-only edit did
# the same. `.dockerignore` keeps such artifacts out of the context; this order
# keeps a change that IS a real input from reaching the layers above it.
#
# The order is dependencies → llama.cpp → backend → interface, so:
#   - only `apps/frontend` changed → the interface is rebuilt, the backend and
#     the fork are reused;
#   - the backend changed too      → both are rebuilt, the fork is reused;
#   - the fork changed too         → the native tree is rebuilt as well.
# `scripts/test-docker-build-cache.sh` proves these cases, plus "nothing
# changed" and "only local build artifacts appeared", against this very file
# with a real BuildKit.

# ── 1. node_modules ───────────────────────────────────────────────────────────
# Keyed on the two manifests alone, and placed above the backend on purpose: a
# Rust change must not reinstall the interface's dependencies. The web interface
# is read from disk at runtime (src/backend/webapp.rs), not embedded in the
# binary, so the two builds share nothing but this image.
COPY apps/frontend/package.json apps/frontend/package-lock.json apps/frontend/
RUN --mount=type=cache,target=/root/.npm \
    cd apps/frontend && npm ci --no-audit --no-fund

# ── 2. the llama.cpp fork ─────────────────────────────────────────────────────
# `vendor/llama-cpp-sys-2/llama.cpp` is a symlink to `../../llama.cpp`, so both
# trees have to land, and the fork goes first: it is the most expensive input
# and the one that changes least. A COPY that stays cached also keeps the
# sources' mtimes frozen (BuildKit preserves them from the context), which is
# what lets cmake/ninja decide inside the `/build/target` cache mount that
# nothing has to be compiled again.
COPY llama.cpp/ llama.cpp/
COPY vendor/ vendor/

# ── 3. the backend ────────────────────────────────────────────────────────────
# `brand.json` and `assets/` are inputs of the Rust build, not only of the
# interface: `crates/brand` embeds the first with `include_str!` and
# `backend/auth.rs` embeds the logo with `include_bytes!`.
COPY Cargo.toml Cargo.lock brand.json ./
COPY .cargo/ .cargo/
COPY assets/ assets/
COPY crates/ crates/

ENV CMAKE_CUDA_ARCHITECTURES="86;120"

# The commit the binary reports in the interface (`/api/ui/node`). It cannot be
# read here: `.dockerignore` excludes `.git/` and this image has no `git`, so
# `crates/node/build.rs` would stamp "desconhecido" on every container. The host
# reads it and passes it in — see `scripts/build-docker.sh`.
#
# An `ARG` in scope enters the environment of every `RUN` below it and is part
# of their cache key even when the command never names it, so both ARGs are
# declared as late as they can be. What is left is deliberate: the stamp is
# compiled into the binary, so a build of a new commit does recompile the
# backend - and, because the interface comes after it, rebuilds the interface
# too. See TODO.md ("O carimbo do commit relinka o Rust inteiro").
ARG POM_BUILD_COMMIT=""

# Short commit of the vendored llama.cpp fork, passed from the host by
# scripts/build-docker.sh. The fork's src/CMakeLists.txt collects model
# sources with a plain file(GLOB) (no CONFIGURE_DEPENDS), so a /build/target
# cache produced against an older checkout can link a libllama.a that is
# missing a model .cpp added since — the "undefined symbol: vtable for
# llama_model_*" link failure. Keying the target cache id to this commit
# gives every fork checkout its own clean native tree; stale ids are dropped
# by `docker builder prune`.
ARG LLAMA_CPP_COMMIT="unknown"

# Cache mounts persist the registry, git checkouts, and incremental build
# artifacts across builds, so unchanged dependencies aren't recompiled.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/build/target,id=pom-target-${LLAMA_CPP_COMMIT} \
    cargo build --release --features cuda,vulkan && \
    cp target/release/pom /build/pom && \
    cargo build --release --features vulkan && \
    cp target/release/pom /build/pom-vulkan && \
    cargo build --release --no-default-features && \
    cp target/release/pom /build/pom-cpu && \
    command -v readelf >/dev/null && \
    for binary in /build/pom-vulkan /build/pom-cpu; do \
      if readelf -d "$binary" | grep -q 'Shared library: \[libcuda\.so\.1\]'; then \
        echo "$binary must not require libcuda.so.1" >&2; \
        exit 1; \
      fi; \
    done && \
    cargo build --release -p pom-plugin-host && \
    cp target/release/pom-plugin-host /build/pom-plugin-host

# ── 4. the interface, last ────────────────────────────────────────────────────
# `npm run build` runs `build-brand.mjs` and `build-docs.mjs` first: they read
# `brand.json` and `assets/` (already here from the backend layer), the
# subsystem READMEs, the root AGENTS.md/README.md, `scripts/AGENTS.md` and every
# AGENTS.md under `crates/` and `apps/frontend/`. `crates/` is already in place
# too, which is why only the documentation the backend layer does not carry is
# copied here.
COPY AGENTS.md README.md ./
COPY docs/ docs/
COPY scripts/AGENTS.md scripts/
COPY apps/frontend/ apps/frontend/
RUN cd apps/frontend && npm run build

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM nvidia/cuda:${CUDA_VERSION}-base-ubuntu24.04

ENV DEBIAN_FRONTEND=noninteractive
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

# The default binary is CUDA+Vulkan. Radeon-only and CPU services select the
# sibling variants so their loader never requires the host's libcuda.so.1.
COPY --from=builder /build/pom /usr/local/bin/pom
COPY --from=builder /build/pom-vulkan /usr/local/bin/pom-vulkan
COPY --from=builder /build/pom-cpu /usr/local/bin/pom-cpu
COPY --from=builder /build/pom-plugin-host /usr/local/bin/pom-plugin-host
COPY --from=builder /build/apps/frontend/dist /app/apps/frontend/dist
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod 0755 /usr/local/bin/docker-entrypoint.sh && \
    for binary in /usr/local/bin/pom-vulkan /usr/local/bin/pom-cpu; do \
      if ldd "$binary" | grep -q 'not found'; then \
        echo "$binary has unresolved runtime libraries" >&2; \
        ldd "$binary" >&2; \
        exit 1; \
      fi; \
    done

# P2P UDP, HTTP gateway, inference server
EXPOSE 42777/udp
EXPOSE 8080/tcp
EXPOSE 8181/tcp

ENV LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu/:/usr/local/cuda-13.0/compat/
# project_root() would otherwise need models/ or pom.json in /app to
# recognize it; this makes the resolution explicit instead of incidental.
ENV POM_HOME=/app
ENV POM_PLUGIN_HOST=/usr/local/bin/pom-plugin-host

WORKDIR /app
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
