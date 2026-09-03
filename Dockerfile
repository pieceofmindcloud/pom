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

# The web interface is read from disk at runtime (src/backend/webapp.rs), not
# embedded in the binary, so building it is independent of the Rust build and
# only touches this layer's cache when apps/frontend actually changes.
COPY apps/frontend/package.json apps/frontend/package-lock.json apps/frontend/
RUN --mount=type=cache,target=/root/.npm \
    cd apps/frontend && npm ci --no-audit --no-fund

COPY . .
RUN cd apps/frontend && npm run build

# The commit the binary reports in the interface (`/api/ui/node`). It cannot be
# read here: `.dockerignore` excludes `.git/` and this image has no `git`, so
# `crates/node/build.rs` would stamp "desconhecido" on every container. The host
# reads it and passes it in — see `scripts/build-docker.sh`.
ARG POM_BUILD_COMMIT=""
ENV POM_BUILD_COMMIT=$POM_BUILD_COMMIT
ENV CMAKE_CUDA_ARCHITECTURES="86;120"

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
    cp target/release/pom /build/pom

# ── Stage 2: runtime ──────────────────────────────────────────────────────────
FROM nvidia/cuda:${CUDA_VERSION}-base-ubuntu24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libstdc++6 \
    libgomp1 \
    libvulkan1 \
    nvidia-utils-595 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/pom /usr/local/bin/pom
COPY --from=builder /build/apps/frontend/dist /app/apps/frontend/dist

# P2P UDP, HTTP gateway, inference server
EXPOSE 42777/udp
EXPOSE 8080/tcp
EXPOSE 8181/tcp

ENV LD_LIBRARY_PATH=/usr/lib/x86_64-linux-gnu/:/usr/local/cuda-13.0/compat/
# project_root() would otherwise need models/ or pom.json in /app to
# recognize it; this makes the resolution explicit instead of incidental.
ENV POM_HOME=/app

WORKDIR /app
ENTRYPOINT ["pom"]
