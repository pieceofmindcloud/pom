#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/pom-docker-mode.XXXXXX")
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/bin" "$work/local-fork/ggml" "$work/local-fork/build"
cat > "$work/bin/docker" <<'FAKE_DOCKER'
#!/usr/bin/env bash
set -euo pipefail
[[ "$*" == "compose build" ]]
test -d "$POM_LLAMA_CPP_SOURCE_CONTEXT"
case "$POM_LLAMA_CPP_MODE" in
  source)
    test -f "$POM_LLAMA_CPP_SOURCE_CONTEXT/ggml/CMakeLists.txt"
    test -f "$POM_LLAMA_CPP_SOURCE_CONTEXT/build_support.c"
    test ! -e "$POM_LLAMA_CPP_SOURCE_CONTEXT/.git"
    test ! -e "$POM_LLAMA_CPP_SOURCE_CONTEXT/build"
    test "$CUDA_RUNTIME_FLAVOR" = base
    [[ "$LLAMA_CPP_CACHE_KEY" != release-* ]]
    ;;
  release)
    test -f "$POM_LLAMA_CPP_SOURCE_CONTEXT/.keep"
    test "$POM_LLAMA_CPP_RELEASE_TAG" = test-release-tag
    test "$CUDA_RUNTIME_FLAVOR" = runtime
    test "$LLAMA_CPP_CACHE_KEY" = release-test-release-tag
    ;;
  *) exit 1 ;;
esac
FAKE_DOCKER
chmod +x "$work/bin/docker"

(
  cd "$work/local-fork"
  git init -q
  git config user.name build-mode-test
  git config user.email build-mode-test@example.invalid
  printf 'cmake_minimum_required(VERSION 3.18)\n' > ggml/CMakeLists.txt
  printf 'source file\n' > build_support.c
  printf 'generated\n' > build/output
  git add ggml/CMakeLists.txt build_support.c build/output
  git commit -q -m fixture
)

PATH="$work/bin:$PATH" \
POM_LLAMA_CPP_MODE=source \
POM_LLAMA_CPP_SOURCE_DIR="$work/local-fork" \
CUDA_RUNTIME_FLAVOR= \
  "$repo_root/scripts/build-docker.sh" >/dev/null

PATH="$work/bin:$PATH" \
POM_LLAMA_CPP_MODE=release \
POM_LLAMA_CPP_RELEASE_TAG=test-release-tag \
CUDA_RUNTIME_FLAVOR= \
  "$repo_root/scripts/build-docker.sh" >/dev/null

printf 'Docker build mode tests passed\n'
