#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

# The commit has to be read here, on the host. `.dockerignore` keeps `.git/`
# out of the build context and the builder has no git binary, so the build
# script cannot discover it. The value is passed only to the final runtime
# stage, avoiding any invalidation of Cargo or frontend build layers.
commit=$(git rev-parse --short HEAD 2>/dev/null || echo desconhecido)
if [ -n "$(git status --porcelain --untracked-files=no 2>/dev/null)" ]; then
  commit="${commit}-sujo"
fi
export POM_BUILD_COMMIT="$commit"
echo "build commit: $POM_BUILD_COMMIT"

# Select an exact published fork release or compile the local checkout.
mode=${POM_LLAMA_CPP_MODE:-source}
case "$mode" in
  release|source) ;;
  *) echo "POM_LLAMA_CPP_MODE deve ser 'release' ou 'source' (recebido: $mode)." >&2; exit 1 ;;
esac
export POM_LLAMA_CPP_MODE="$mode"
export POM_LLAMA_CPP_RELEASE_REPOSITORY=${POM_LLAMA_CPP_RELEASE_REPOSITORY:-pieceofmindcloud/llama.cpp}
export POM_LLAMA_CPP_REF=${POM_LLAMA_CPP_REF:-pom-tensor-parallel}

# POM_PLUGIN_LLAMA_RELEASE=latest takes the newest published release of the engine
# when the build starts. The resolved tag is what the Docker layers see, so a new
# release rebuilds the engine layer and the same release reuses the cache.
if [ "${POM_PLUGIN_LLAMA_RELEASE:-}" = latest ]; then
  if ! POM_PLUGIN_LLAMA_RELEASE=$(gh release view --repo pieceofmindcloud/pom-plugin-llama.cpp \
      --json tagName -q .tagName); then
    echo "não foi possível resolver o último release de pom-plugin-llama.cpp (gh release view)." >&2
    exit 1
  fi
  export POM_PLUGIN_LLAMA_RELEASE
  echo "motor: pom-plugin-llama.cpp@${POM_PLUGIN_LLAMA_RELEASE} (latest)"
fi

llama_docker_context=$(mktemp -d "${TMPDIR:-/tmp}/pom-llama-context.XXXXXX")
trap 'rm -rf "$llama_docker_context"' EXIT
if [ "$mode" = source ]; then
  llama_source_dir=${POM_LLAMA_CPP_SOURCE_DIR:-llama.cpp}
  if ! llama_source_dir=$(cd "$llama_source_dir" 2>/dev/null && pwd -P); then
    echo "não foi possível localizar POM_LLAMA_CPP_SOURCE_DIR=${POM_LLAMA_CPP_SOURCE_DIR:-llama.cpp}." >&2
    exit 1
  fi
  if [ ! -d "$llama_source_dir/ggml" ]; then
    echo "POM_LLAMA_CPP_SOURCE_DIR precisa conter ggml/: $llama_source_dir" >&2
    exit 1
  fi
  (
    cd "$llama_source_dir"
    tar --exclude=.git --exclude=build --exclude='./build-*' --exclude='./models' \
      --exclude='*.gguf' --exclude=node_modules --exclude='*.key' \
      --exclude='*.pem' --exclude='*.p12' --exclude='*.pfx' \
      --exclude='.env' --exclude='.env.*' -cf - .
  ) | (cd "$llama_docker_context" && tar -xf -)
else
  touch "$llama_docker_context/.keep"
fi
printf '.git/\nbuild/\nbuild-*/\nmodels/\n**/*.gguf\nnode_modules/\n**/*.key\n**/*.pem\n**/*.p12\n**/*.pfx\n**/.env\n**/.env.*\n' \
  > "$llama_docker_context/.dockerignore"
export POM_LLAMA_CPP_SOURCE_CONTEXT="$llama_docker_context"

if [ "$mode" = release ]; then
  if [ -z "${POM_LLAMA_CPP_RELEASE_TAG:-}" ]; then
    if ! shallow=$(git -C llama.cpp rev-parse --is-shallow-repository 2>/dev/null); then
      echo "não foi possível consultar o histórico do fork em ./llama.cpp." >&2
      exit 1
    fi
    fetch_args=(--quiet --no-tags --filter=blob:none)
    if [ "$shallow" = true ]; then
      fetch_args+=(--unshallow)
    fi
    if ! git -C llama.cpp fetch "${fetch_args[@]}" \
      "https://github.com/${POM_LLAMA_CPP_RELEASE_REPOSITORY}.git" \
      "refs/heads/${POM_LLAMA_CPP_REF}"; then
      echo "não foi possível consultar a branch ${POM_LLAMA_CPP_REF} do fork." >&2
      exit 1
    fi
    remote_commit=$(git -C llama.cpp rev-parse --verify FETCH_HEAD)
    build_number=$(git -C llama.cpp rev-list --count "$remote_commit")
    short_hash=$(git -C llama.cpp rev-parse --short=7 "$remote_commit")
    safe_ref=${POM_LLAMA_CPP_REF//\//-}
    if [ "$POM_LLAMA_CPP_REF" = master ] || [ "$POM_LLAMA_CPP_REF" = "b${build_number}" ]; then
      POM_LLAMA_CPP_RELEASE_TAG="b${build_number}"
    else
      POM_LLAMA_CPP_RELEASE_TAG="${safe_ref}-b${build_number}-${short_hash}"
    fi
  fi
  if [[ ! "$POM_LLAMA_CPP_RELEASE_TAG" =~ ^[A-Za-z0-9._-]+$ ]]; then
    echo "POM_LLAMA_CPP_RELEASE_TAG contém caracteres inválidos." >&2
    exit 1
  fi
  export POM_LLAMA_CPP_RELEASE_TAG
  export LLAMA_CPP_CACHE_KEY="release-${POM_LLAMA_CPP_RELEASE_TAG}"
  export CUDA_RUNTIME_FLAVOR=${CUDA_RUNTIME_FLAVOR:-runtime}
  if [ "$CUDA_RUNTIME_FLAVOR" != runtime ]; then
    echo "o modo release requer CUDA_RUNTIME_FLAVOR=runtime para carregar cuBLAS." >&2
    exit 1
  fi
  echo "llama.cpp release: ${POM_LLAMA_CPP_RELEASE_REPOSITORY}@${POM_LLAMA_CPP_RELEASE_TAG}"
else
  if ! llama_commit=$(git -C "$llama_source_dir" rev-parse --short HEAD 2>/dev/null); then
    echo "não foi possível ler o commit do fork em $llama_source_dir." >&2
    echo "prepare o checkout (ver docs/subsystems/build-and-run.md) antes de construir a imagem." >&2
    exit 1
  fi
  if ! llama_source_hash=$(
    {
      git -C "$llama_source_dir" diff HEAD --binary
      git -C "$llama_source_dir" ls-files --others --exclude-standard -z |
        while IFS= read -r -d '' path; do
          shasum -a 256 "$llama_source_dir/$path"
        done
    } | shasum -a 256 | awk '{ print $1 }'
  ); then
    echo "não foi possível calcular a impressão das fontes do fork llama.cpp." >&2
    exit 1
  fi
  export LLAMA_CPP_CACHE_KEY="${llama_commit}-${llama_source_hash}"
  export POM_LLAMA_CPP_RELEASE_TAG=${POM_LLAMA_CPP_RELEASE_TAG:-}
  export CUDA_RUNTIME_FLAVOR=${CUDA_RUNTIME_FLAVOR:-base}
  echo "llama.cpp source cache key: $LLAMA_CPP_CACHE_KEY"
fi
echo "llama.cpp mode: $POM_LLAMA_CPP_MODE (CUDA runtime: $CUDA_RUNTIME_FLAVOR)"

# Derive the native-cache slug from the actual CMake architecture list so a
# caller cannot change the compiled GPU targets while accidentally reusing the
# old native cache namespace.
cuda_architectures=${POM_CUDA_ARCHITECTURES:-86\;120}
export POM_CUDA_ARCHITECTURES="$cuda_architectures"
export POM_CUDA_ARCH_CACHE_KEY=$(printf '%s' "$cuda_architectures" | sed 's/[^[:alnum:]]\+/-/g; s/^-//; s/-$//')
echo "CUDA architectures: $POM_CUDA_ARCHITECTURES (cache key: $POM_CUDA_ARCH_CACHE_KEY)"

# Cargo and the frontend build in independent image stages; this script only
# supplies their cache identities and the runtime commit stamp.
docker compose build
