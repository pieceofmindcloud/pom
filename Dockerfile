# ── Stage 1: dependency planner ──────────────────────────────────────────────
FROM lukemathwalker/cargo-chef:latest-rust-1 AS planner
WORKDIR /build
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 2: builder ──────────────────────────────────────────────────────────
FROM ubuntu:24.04 AS builder

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    cmake \
    libclang-dev \
    ca-certificates \
    curl \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_HOME=/root/.cargo \
    RUSTUP_HOME=/root/.rustup \
    PATH=/root/.cargo/bin:$PATH

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path

RUN cargo install cargo-chef --locked

WORKDIR /build

# Cook dependencies without GPU features (CPU-only default).
# Cached as long as Cargo.toml / Cargo.lock don't change.
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --no-default-features --recipe-path recipe.json

# Build the real binary. web/ assets are embedded via include_str!.
COPY . .
RUN cargo build --release --no-default-features

# ── Stage 3: runtime ──────────────────────────────────────────────────────────
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libstdc++6 \
    libgomp1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/rdma-p2p /usr/local/bin/rdma-p2p

# P2P UDP, HTTP gateway, inference server
EXPOSE 42777/udp
EXPOSE 8080/tcp
EXPOSE 8181/tcp

ENTRYPOINT ["rdma-p2p"]
