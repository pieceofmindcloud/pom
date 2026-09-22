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

# Short commit of the vendored llama.cpp fork. The Dockerfile keys the
# /build/target cache mount id to this so a fork bump gets a clean native
# tree instead of relinking a stale libllama.a — see the Dockerfile comment
# and docs/subsystems/build-execucao.md ("Docker").
#
# Not readable means stop, not `unknown`: that value is a cache mount id like
# any other, so two different checkouts that both failed this read would share
# one native tree - exactly the stale `libllama.a` the id exists to prevent.
# The fork is required anyway, since the image copies `llama.cpp/`.
if ! llama_commit=$(git -C llama.cpp rev-parse --short HEAD 2>/dev/null); then
  echo "não foi possível ler o commit do fork em ./llama.cpp." >&2
  echo "prepare o checkout (ver docs/subsystems/build-execucao.md) antes de construir a imagem." >&2
  exit 1
fi
export LLAMA_CPP_COMMIT="$llama_commit"
echo "llama.cpp fork commit: $LLAMA_CPP_COMMIT"

# The interface is built inside the image, as the last step of the builder
# stage, and read from disk at runtime - there is nothing to build here.
docker compose build
