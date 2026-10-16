#!/usr/bin/env bash
#
# Builds a POM.app bundle around the already compiled binary.
#
# A Unix executable has no icon: on macOS what carries the icon is the `.app`
# bundle, not the file. Without this, Finder shows the binary with the generic
# terminal icon and there is nowhere to hang the product's identity.
#
#     ./scripts/make-macos-app.sh [binary] [destination]
#
# Defaults: `target/release/pom` and `dist-release/POM.app`.
#
# The bundle is a thin wrapper: the binary is still the same command-line
# program, and `POM.app/Contents/MacOS/pom` can be called directly with the same
# arguments. `LSUIElement` keeps the node out of the Dock; the main interface is
# still web, with quick access through the native status bar item.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
bin="${1:-$root/target/release/pom}"
destination="${2:-$root/dist-release/POM.app}"

name=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["name"])' "$root/brand.json")
slug=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["slug"])' "$root/brand.json")
icns="$root/$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["app_icons"]["macos"])' "$root/brand.json")"
# The status bar item art and the native screen's translations live together.
macos_resources="$root/assets/brand/macos"
status_icon="$macos_resources/pom-status-template.png"

[ -x "$bin" ] || { echo "binário não encontrado: $bin" >&2; exit 1; }
# pom only starts plugins through the `pom-plugin-host` beside its own
# executable (`plugin_host::host_executable`); without it no plugin installs.
host="$(dirname "$bin")/pom-plugin-host"
[ -x "$host" ] || {
    echo "pom-plugin-host não encontrado: $host" >&2
    echo "compile com: cargo build --release -p node -p pom-plugin-host" >&2
    exit 1
}
[ -f "$icns" ] || { echo "ícone não encontrado: $icns" >&2; exit 1; }
# A status bar item without art falls back to a text label, which is a worse and
# silent delivery. `scripts/derive-macos-status-icon.py` generates this art.
[ -f "$status_icon" ] || {
    echo "ícone da barra não encontrado: $status_icon" >&2
    echo "gere-o com: python3 scripts/derive-macos-status-icon.py" >&2
    exit 1
}

rm -rf "$destination"
mkdir -p "$destination/Contents/MacOS" "$destination/Contents/Resources"

cp "$bin" "$destination/Contents/MacOS/$slug"
cp "$host" "$destination/Contents/MacOS/pom-plugin-host"
cp "$icns" "$destination/Contents/Resources/$slug.icns"
# The three resolutions go in under their original name: the native side looks
# for the `@2x` and `@3x` companions beside the 1x art that Rust points it to.
cp "$macos_resources"/pom-status-template*.png "$destination/Contents/Resources/"
find "$macos_resources" -mindepth 1 -maxdepth 1 -type d -name '*.lproj' \
    -exec cp -R {} "$destination/Contents/Resources/" \;

version=$(grep -m1 '^version' "$root/Cargo.toml" | sed 's/[^0-9.]//g')
version=${version:-0.1.0}

cat > "$destination/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>$name</string>
    <key>CFBundleDisplayName</key><string>$name</string>
    <key>CFBundleIdentifier</key><string>app.$slug.node</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleExecutable</key><string>$slug</string>
    <key>CFBundleIconFile</key><string>$slug</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>LSUIElement</key><true/>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSLocalNetworkUsageDescription</key><string>O POM usa a rede local para encontrar outros nós e a internet para se conectar a nós remotos e relays.</string>
</dict>
</plist>
PLIST

# Finder caches the icon by path; without this a bundle recreated in the same
# place keeps showing the previous one.
touch "$destination"

echo "✓ $destination"
echo "  executável: $destination/Contents/MacOS/$slug"
