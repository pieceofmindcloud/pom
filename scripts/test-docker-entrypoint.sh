#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
entrypoint="$repo_root/docker-entrypoint.sh"
test -x "$entrypoint"

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/pom-docker-entrypoint.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT
mkdir -p "$work_dir/bin"

cat >"$work_dir/bin/pom" <<'STUB'
#!/bin/sh
set -eu
printf '%s\n' "${POM_PLUGIN_HOST-}" > "$POM_ENTRYPOINT_CAPTURE"
printf '%s\n' "$*" >> "$POM_ENTRYPOINT_CAPTURE"
STUB
cat >"$work_dir/bin/pom-plugin-host" <<'STUB'
#!/bin/sh
exit 0
STUB
chmod +x "$work_dir/bin/pom" "$work_dir/bin/pom-plugin-host"

POM_BIN="$work_dir/bin/pom" \
POM_PLUGIN_HOST="$work_dir/bin/pom-plugin-host" \
POM_ENTRYPOINT_CAPTURE="$work_dir/capture" \
  "$entrypoint" --gateway-port 8080

test "$(sed -n '1p' "$work_dir/capture")" = "$work_dir/bin/pom-plugin-host"
test "$(sed -n '2p' "$work_dir/capture")" = "--gateway-port 8080"
# Without an init as PID 1, orphans left by plugins become zombies of `pom`.
entrypoints=$(grep -c '^ENTRYPOINT ' "$repo_root/Dockerfile")
with_tini=$(grep -c '^ENTRYPOINT \["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"\]$' "$repo_root/Dockerfile")
test "$entrypoints" -gt 0 && test "$entrypoints" = "$with_tini"
printf 'docker entrypoint test passed\n'
