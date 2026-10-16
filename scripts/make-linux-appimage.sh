#!/usr/bin/env bash
#
# Builds the POM AppImage around the already compiled binaries.
#
# The AppImage is one file that runs by double click (after `chmod +x`) on Ubuntu and
# other distributions, with no installation. Inside it the interface is compiled into
# `pom` (feature `embed-ui`), so there is no `apps/` folder to carry, and the plugin
# host sits beside it, as the node requires.
#
#     APPIMAGETOOL=/path/to/appimagetool ./scripts/make-linux-appimage.sh \
#         <pom> <pom-plugin-host> <version> <destination-dir>
#
# The output is `<destination-dir>/pom-<version>-x86_64.AppImage`.
#
# Not bundled on purpose: the NVIDIA driver (`libcuda`) and the Vulkan loader
# (`libvulkan.so.1`) belong to the machine's driver, and the libc is the one of the
# system. The package runs wherever the glibc is at least the one it was built with.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
bin="$1"
host="$2"
version="$3"
destination="$4"
tool="${APPIMAGETOOL:?set APPIMAGETOOL to the appimagetool executable}"

[ -x "$bin" ] || { echo "binário não encontrado: $bin" >&2; exit 1; }
[ -x "$host" ] || { echo "pom-plugin-host não encontrado: $host" >&2; exit 1; }
[ -x "$tool" ] || { echo "appimagetool não encontrado: $tool" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
appdir="$work/POM.AppDir"
mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/icons"

install -m 755 "$bin" "$appdir/usr/bin/pom"
install -m 755 "$host" "$appdir/usr/bin/pom-plugin-host"

# AppRun puts the program's folder on PATH, so the desktop entry's `Exec=pom` works.
cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="$HERE/usr/bin:$PATH"
exec "$HERE/usr/bin/pom" "$@"
EOF
chmod 755 "$appdir/AppRun"

# The desktop entry and the icon: appimagetool reads both from the root of the AppDir.
install -m 644 "$root/assets/brand/pom.desktop" "$appdir/pom.desktop"
install -m 644 "$root/assets/brand/pom.desktop" "$appdir/usr/share/applications/pom.desktop"
cp -R "$root/assets/brand/linux/hicolor" "$appdir/usr/share/icons/"
install -m 644 "$root/assets/brand/linux/hicolor/256x256/apps/pom.png" "$appdir/pom.png"

mkdir -p "$destination"
out="$destination/pom-$version-x86_64.AppImage"
# `--appimage-extract-and-run` runs the tool without FUSE, so the build machine needs no
# FUSE mount for it.
ARCH=x86_64 "$tool" --appimage-extract-and-run "$appdir" "$out"
chmod 755 "$out"
echo "AppImage: $out"
