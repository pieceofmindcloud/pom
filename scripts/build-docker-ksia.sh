#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

# The commit has to be read here, on the host. Inside the builder image it
# cannot be: `.dockerignore` keeps `.git/` out of the build context and the
# image has no `git` binary, so `crates/node/build.rs` would stamp
# "desconhecido" and the interface would show that for every container — the
# one build where "which binary is this peer running?" is hardest to answer
# another way.
commit=$(git rev-parse --short HEAD 2>/dev/null || echo desconhecido)
if [ -n "$(git status --porcelain --untracked-files=no 2>/dev/null)" ]; then
  commit="${commit}-sujo"
fi
export POM_BUILD_COMMIT="$commit"
echo "build commit: $POM_BUILD_COMMIT"

# Key the native cache by the fork commit and its local source delta. A commit
# alone misses staged/unstaged edits and untracked source files; those can leave
# CMake's cached model-file glob stale even though COPY invalidates the RUN.
if ! llama_commit=$(git -C llama.cpp rev-parse --short HEAD 2>/dev/null); then
  echo "não foi possível ler o commit do fork em ./llama.cpp." >&2
  echo "prepare o checkout (ver docs/subsystems/build-execucao.md) antes de construir a imagem." >&2
  exit 1
fi
if ! llama_source_hash=$(
  {
    git -C llama.cpp diff HEAD --binary
    git -C llama.cpp ls-files --others --exclude-standard -z |
      while IFS= read -r -d '' path; do
        shasum -a 256 "llama.cpp/$path"
      done
  } | shasum -a 256 | awk '{ print $1 }'
); then
  echo "não foi possível calcular a impressão das fontes do fork llama.cpp." >&2
  exit 1
fi
export LLAMA_CPP_CACHE_KEY="${llama_commit}-${llama_source_hash}"
echo "llama.cpp fork cache key: $LLAMA_CPP_CACHE_KEY"

# Derive the native-cache slug from the actual CMake architecture list so a
# caller cannot change the compiled GPU targets while accidentally reusing the
# old native cache namespace.
cuda_architectures=${POM_CUDA_ARCHITECTURES:-86\;120}
export POM_CUDA_ARCHITECTURES="$cuda_architectures"
export POM_CUDA_ARCH_CACHE_KEY=$(printf '%s' "$cuda_architectures" | sed 's/[^[:alnum:]]\+/-/g; s/^-//; s/-$//')
echo "CUDA architectures: $POM_CUDA_ARCHITECTURES (cache key: $POM_CUDA_ARCH_CACHE_KEY)"

# The interface is built inside the image, as the last step of the builder
# stage, and read from disk at runtime - there is nothing to build here.
docker compose -f docker-compose.ksia.yml build
