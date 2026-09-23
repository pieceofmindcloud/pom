# syntax=docker/dockerfile:1
ARG CUDA_VERSION=13.2.1

FROM nvidia/cuda:${CUDA_VERSION}-devel-ubuntu24.04 as builder

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

ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    PATH=/root/.cargo/bin:$PATH

ARG RUST_TOOLCHAIN=stable
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain ${RUST_TOOLCHAIN} --no-modify-path
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

ARG POM_CUDA_ARCHITECTURES="86;120"
ENV CMAKE_CUDA_ARCHITECTURES=${POM_CUDA_ARCHITECTURES}

# The commit the binary reports in the interface (`/api/ui/node`). It cannot be
# read here: `.dockerignore` excludes `.git/` and this image has no `git`, so
# `crates/node/build.rs` would stamp "desconhecido" on every container. The host
# reads it and passes it in — see `scripts/build-docker.sh`.
#
# An `ARG` in scope enters the environment of every `RUN` below it and is part
# of their cache key even when the command never names it, so both ARGs are
# declared as late as they can be. A new commit changes the stamp input, so
# Cargo rebuilds and relinks the `node` binary; unchanged crates remain
# reusable in their persistent target directories. Since this ARG also reaches
# the later interface RUN, that layer reruns too.
ARG POM_BUILD_COMMIT=""

# Fork cache identity passed from the host by scripts/build-docker.sh. It
# combines the commit and local source delta. The fork's src/CMakeLists.txt
# collects model sources with a plain file(GLOB) (no CONFIGURE_DEPENDS), so a
# native cache from an older source state can link a libllama.a missing a model
# .cpp added since, failing with `undefined symbol: vtable for llama_model_*`.
# Keying the target cache id to this source identity gives every fork source
# state its own clean native tree; stale ids are dropped by `docker builder prune`.
ARG LLAMA_CPP_CACHE_KEY="unknown"
ARG POM_CUDA_ARCH_CACHE_KEY="86-120"

# Cache mounts persist the registry, git checkouts, and incremental build
# artifacts across builds. Separate target directories retain each feature
# set's Cargo fingerprints; the mount id also isolates native CMake output by
# fork, CUDA toolkit, Rust toolchain selector, and architecture.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/build/target,id=pom-target-${LLAMA_CPP_CACHE_KEY}-${CUDA_VERSION}-${POM_CUDA_ARCH_CACHE_KEY}-${RUST_TOOLCHAIN} \
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
