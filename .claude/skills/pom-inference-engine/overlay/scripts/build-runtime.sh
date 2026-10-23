#!/usr/bin/env bash
# Builds the __NAME__ engine runtime into build/runtime from the commit that
# engine.lock pins. The runtime is never committed nor embedded in the library:
# a release attaches it (pinned URL, size and SHA-256), a development node reads
# build/runtime from the mounted source (src/launch.rs, find_runtime).
#
#   scripts/build-runtime.sh [--source <engine checkout>]
#
# Layout the plugin expects (src/launch.rs, is_runtime):
#   build/runtime/bin/server     the engine's OpenAI-compatible server
#   build/runtime/lib/           shared libraries the server needs, if any
#   build/runtime/RUNTIME.json   {"version", "engine_commit", "platform"}
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
# shellcheck disable=SC1091
source "$root/engine.lock"
source_dir="$root/build/runtime-src"
while (($# > 0)); do
  case "$1" in
    --source) source_dir="$(cd "$2" && pwd -P)"; shift 2 ;;
    *) printf 'build-runtime: unknown argument %s\n' "$1" >&2; exit 2 ;;
  esac
done

if [[ "$source_dir" == "$root/build/runtime-src" ]]; then
  if [[ ! -d "$source_dir/.git" ]]; then
    git clone --filter=blob:none "$ENGINE_REPOSITORY" "$source_dir"
  fi
  git -C "$source_dir" fetch --quiet origin "$ENGINE_BRANCH"
  git -C "$source_dir" checkout --quiet --detach "$ENGINE_COMMIT"
fi
commit="$(git -C "$source_dir" rev-parse HEAD)"

out="$root/build/runtime"
staging="$root/build/runtime.new"
rm -rf "$staging"
mkdir -p "$staging/bin" "$staging/lib"

# --- engine-specific: build the server and copy it into "$staging" ---------
# Replace this block with the engine's own build (cmake, make, pip, cargo).
# Keep: no network at run time, no shared library the node image lacks
# (copy them into "$staging/lib"), the server at "$staging/bin/server".
printf 'build-runtime: fill the engine build step in scripts/build-runtime.sh\n' >&2
exit 3
# ---------------------------------------------------------------------------

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) platform=linux-x86_64 ;;
  Linux-aarch64) platform=linux-aarch64 ;;
  Darwin-arm64) platform=macos-aarch64 ;;
  *) platform="$(uname -s)-$(uname -m)" ;;
esac
printf '{"version": "%s", "engine_commit": "%s", "platform": "%s"}\n' \
  "${commit:0:12}" "$commit" "$platform" > "$staging/RUNTIME.json"
rm -rf "$out"
mv "$staging" "$out"
printf 'build-runtime: %s at %s\n' "$out" "${commit:0:12}"
