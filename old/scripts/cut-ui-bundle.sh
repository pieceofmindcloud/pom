#!/usr/bin/env bash
# Generate the versioned interface bundle that release builds embed.
#
# Run this at the moment of cutting a release, never in day-to-day work: outside
# a release nothing generated belongs in the repository, and the default build
# rebuilds the interface from source anyway.
#
#     ./scripts/cut-ui-bundle.sh
#     git add web-ui/dist-release && git commit
#     cargo build --release --features release-bundle
#
# The fingerprint written beside the bundle is what lets the release build prove
# the committed files still match the source. Do not edit it by hand — a bundle
# whose fingerprint was "fixed" rather than regenerated is exactly the silent
# drift this whole mechanism exists to prevent.
set -euo pipefail

cd "$(dirname "$0")/.."
root=$PWD

echo "==> reconstruindo a interface a partir do fonte"
cd apps/frontend
[ -d node_modules ] || npm install --no-audit --no-fund
npm run build

echo "==> publicando em apps/frontend/dist-release"
rm -rf dist-release
cp -R dist dist-release

echo "==> gravando a impressao digital do fonte"
cd "$root"
cargo run --quiet --bin ui-fingerprint > apps/frontend/dist-release/.source-manifest
echo "    $(cat apps/frontend/dist-release/.source-manifest)"

cat <<'EOF'

Pacote pronto. Falta commitar:

    git add apps/frontend/dist-release
    git commit -m "chore(ui): pacote versionado da interface"

E para conferir que o release compila sem Node:

    cargo build --release --features release-bundle
EOF
