#!/usr/bin/env bash
#
# Builds the POM installer disk image: POM.app, a shortcut to Applications and
# the README, laid out on the window of dmg-background*.png (see
# make-dmg-background.py, which also holds the icon coordinates).
#
#     packaging/macos/make-dmg.sh <POM.app> <README.txt> <output.dmg>
#
# Needs create-dmg (brew install create-dmg). Finder positions the icons, so this
# runs only in a logged-in macOS session, which a macOS runner is.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
here="$root/packaging/macos"
icon="$root/assets/brand/pom.icns"

app="${1:?usage: make-dmg.sh <POM.app> <README.txt> <output.dmg>}"
readme="${2:?usage: make-dmg.sh <POM.app> <README.txt> <output.dmg>}"
out="${3:?usage: make-dmg.sh <POM.app> <README.txt> <output.dmg>}"

[ -d "$app" ] || { echo "app não encontrado: $app" >&2; exit 1; }
[ -f "$readme" ] || { echo "README não encontrado: $readme" >&2; exit 1; }
command -v create-dmg >/dev/null || { echo "create-dmg não encontrado (brew install create-dmg)" >&2; exit 1; }

# One TIFF with the 1x and 2x images, so the window is sharp on Retina screens.
work="$(mktemp -d)"
background="$work/dmg-background.tiff"
tiffutil -cathidpicheck "$here/dmg-background.png" "$here/dmg-background@2x.png" -out "$background"

# The disk image holds a copy, so the bundle's seal is copied with ditto (cp can drop attributes).
staging="$work/staging"
mkdir "$staging"
ditto "$app" "$staging/POM.app"
# TextEdit opens .txt without requiring a Markdown application. This guide must
# remain readable before the user authorizes the non-notarized app.
cp "$readme" "$staging/README.txt"

rm -f "$out"
create-dmg \
    --volname "POM" \
    --volicon "$icon" \
    --background "$background" \
    --window-pos 200 120 \
    --window-size 660 400 \
    --icon-size 96 \
    --text-size 14 \
    --icon "POM.app" 165 215 \
    --hide-extension "POM.app" \
    --app-drop-link 495 215 \
    --icon "README.txt" 330 330 \
    "$out" "$staging"

echo "✓ $out"
