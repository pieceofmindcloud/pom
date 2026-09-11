#!/usr/bin/env bash

set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
tmp_root=$(mktemp -d "${TMPDIR:-/tmp}/pom-build-env.XXXXXX")
trap 'rm -rf "$tmp_root"' EXIT

without_sccache_output=$(env -i \
    HOME="$tmp_root/home-without-sccache" \
    PATH="/usr/bin:/bin" \
    bash -c 'source "$1"; test -z "${SCCACHE_DIR-}"; test -z "${SCCACHE_CACHE_SIZE-}"; test -z "${SCCACHE_SERVER_UDS-}"; test -z "${RUSTC_WRAPPER-}"; test -z "${CMAKE_C_COMPILER_LAUNCHER-}"; test -z "${CMAKE_CXX_COMPILER_LAUNCHER-}"; test -z "${CMAKE_CUDA_COMPILER_LAUNCHER-}"; test -z "${CCACHE-}"; test -z "${CARGO_TARGET_DIR-}"' _ "$repo_root/scripts/build-env.sh" 2>"$tmp_root/without-sccache.log")
test -z "$without_sccache_output"

mkdir -p "$tmp_root/fake-bin"
ln -s /usr/bin/true "$tmp_root/fake-bin/sccache"

with_sccache_output=$(env -i \
    HOME="$tmp_root/home-with-sccache" \
    PATH="$tmp_root/fake-bin:/usr/bin:/bin" \
    bash -c 'source "$1"; printf "%s\n" "SCCACHE_DIR=$SCCACHE_DIR" "SCCACHE_CACHE_SIZE=$SCCACHE_CACHE_SIZE" "SCCACHE_SERVER_UDS=$SCCACHE_SERVER_UDS" "RUSTC_WRAPPER=$RUSTC_WRAPPER" "CMAKE_C_COMPILER_LAUNCHER=$CMAKE_C_COMPILER_LAUNCHER" "CMAKE_CXX_COMPILER_LAUNCHER=$CMAKE_CXX_COMPILER_LAUNCHER" "CMAKE_CUDA_COMPILER_LAUNCHER=$CMAKE_CUDA_COMPILER_LAUNCHER" "CCACHE=$CCACHE" "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"' _ "$repo_root/scripts/build-env.sh")

expected_output=$(printf '%s\n' \
    "SCCACHE_DIR=$tmp_root/home-with-sccache/.cache/sccache" \
    "SCCACHE_CACHE_SIZE=80G" \
    "SCCACHE_SERVER_UDS=$tmp_root/home-with-sccache/.cache/sccache/server.sock" \
    "RUSTC_WRAPPER=sccache" \
    "CMAKE_C_COMPILER_LAUNCHER=sccache" \
    "CMAKE_CXX_COMPILER_LAUNCHER=sccache" \
    "CMAKE_CUDA_COMPILER_LAUNCHER=sccache" \
    "CCACHE=sccache" \
    "CARGO_TARGET_DIR=$tmp_root/home-with-sccache/.cache/pom-target")

test "$with_sccache_output" = "$expected_output"
test -d "$tmp_root/home-with-sccache/.cache/sccache"

override_output=$(env -i \
    HOME="$tmp_root/home-with-override" \
    PATH="$tmp_root/fake-bin:/usr/bin:/bin" \
    SCCACHE_DIR="$tmp_root/custom-cache" \
    SCCACHE_CACHE_SIZE=7G \
    SCCACHE_SERVER_UDS="$tmp_root/custom-server.sock" \
    CARGO_TARGET_DIR="$tmp_root/custom-target" \
    bash -c 'source "$1"; printf "%s\n" "$SCCACHE_DIR" "$SCCACHE_CACHE_SIZE" "$SCCACHE_SERVER_UDS" "$CARGO_TARGET_DIR"' _ "$repo_root/scripts/build-env.sh")

test "$override_output" = "$(printf '%s\n' "$tmp_root/custom-cache" 7G "$tmp_root/custom-server.sock" "$tmp_root/custom-target")"

printf '%s\n' 'build-env: fallback e exports verificados'
