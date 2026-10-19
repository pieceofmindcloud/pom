#!/usr/bin/env bash
#
# Builds the macOS installer disk image on a developer Mac, for testing a change
# in the packaged app. It runs the same steps as `.github/workflows/release-macos.yml`
# but for this Mac's architecture only (no lipo), so it needs no second Rust target.
#
#     packaging/macos/build-local-dmg.sh [output.dmg]
#
# Default output: `dist-release/pom-local-<arch>-apple-darwin.dmg`.
#
# Needs: Rust >= 1.88 (`RUSTUP_TOOLCHAIN=<version>` picks one that is not the
# default), cmake, Node >= 20 (`npm`), create-dmg (`brew install create-dmg`),
# SSH access to the engine repository, and the llama.cpp fork at `<repo>/llama.cpp`
# (or `LLAMA_SOURCE`), see docs/subsystems/build-and-run.md, "Preparing a local
# worktree". `POM_SKIP_UI=1` reuses an existing `apps/frontend/dist`.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

arch="$(uname -m)"
[ "$arch" = "arm64" ] && arch=aarch64
out="${1:-$root/dist-release/pom-local-$arch-apple-darwin.dmg}"

command -v create-dmg >/dev/null || { echo "create-dmg not found (brew install create-dmg)" >&2; exit 1; }
if [ -z "${LLAMA_SOURCE:-}" ] && [ ! -d "$root/llama.cpp/ggml" ]; then
    echo "llama.cpp fork not found at $root/llama.cpp; clone it with:" >&2
    echo "  git clone --depth 1 --branch pom-tensor-parallel https://github.com/pieceofmindcloud/llama.cpp.git llama.cpp" >&2
    exit 1
fi

if [ "${POM_SKIP_UI:-0}" != "1" ]; then
    node_major="$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)"
    [ "$node_major" -ge 20 ] || { echo "Node >= 20 required on PATH (found: $(node -v 2>/dev/null || echo none))" >&2; exit 1; }
    (cd apps/frontend && npm ci && npm run build)
fi
[ -f apps/frontend/dist/index.html ] || { echo "apps/frontend/dist is missing" >&2; exit 1; }

# Same floor as the release: the app runs from macOS 11 on.
MACOSX_DEPLOYMENT_TARGET=11.0 \
    cargo build --release --locked -p node -p pom-plugin-host --features metal

work="$root/dist-release/local-package"
rm -rf "$work"
./scripts/make-macos-app.sh target/release/pom "$work/POM.app"
mkdir -p "$work/POM.app/Contents/Resources/apps/frontend"
cp -R apps/frontend/dist "$work/POM.app/Contents/Resources/apps/frontend/"

# Signed last: any change after the seal makes Gatekeeper call the app "damaged".
codesign --force --deep --sign - "$work/POM.app"
codesign --verify --deep --strict "$work/POM.app"

mkdir -p "$(dirname "$out")"
packaging/macos/make-dmg.sh "$work/POM.app" packaging/macos/README.md "$out"
