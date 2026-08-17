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

pushd apps/frontend
npm run build
popd
docker compose build
