#!/usr/bin/env bash

# Load this file in the shell that will run Cargo:
#   source scripts/build-env.sh
#
# The helper does not change the environment when sccache is not installed. That
# way, a machine without the tool keeps using exactly the normal Cargo/CMake
# path.

if ! command -v sccache >/dev/null 2>&1; then
    printf '%s\n' 'sccache nao encontrado no PATH; usando compiladores sem cache.' >&2
else
    sccache_dir="${SCCACHE_DIR:-${HOME}/.cache/sccache}"
    sccache_cache_size="${SCCACHE_CACHE_SIZE:-80G}"
    sccache_server_uds="${SCCACHE_SERVER_UDS:-${sccache_dir}/server.sock}"
    cargo_target_dir="${CARGO_TARGET_DIR:-${HOME}/.cache/pom-target}"

    if ! mkdir -p "$sccache_dir"; then
        printf 'nao foi possivel criar SCCACHE_DIR=%s; cache desativado.\n' "$sccache_dir" >&2
    else
        export SCCACHE_DIR="$sccache_dir"
        export SCCACHE_CACHE_SIZE="$sccache_cache_size"
        export SCCACHE_SERVER_UDS="$sccache_server_uds"
        export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
        export CMAKE_C_COMPILER_LAUNCHER="${CMAKE_C_COMPILER_LAUNCHER:-sccache}"
        export CMAKE_CXX_COMPILER_LAUNCHER="${CMAKE_CXX_COMPILER_LAUNCHER:-sccache}"
        export CMAKE_CUDA_COMPILER_LAUNCHER="${CMAKE_CUDA_COMPILER_LAUNCHER:-sccache}"
        export CCACHE="${CCACHE:-sccache}"
        export CARGO_TARGET_DIR="$cargo_target_dir"

        printf 'sccache ativado: cache=%s (limite %s), target=%s\n' \
            "$sccache_dir" "$sccache_cache_size" "$cargo_target_dir" >&2
    fi
fi
