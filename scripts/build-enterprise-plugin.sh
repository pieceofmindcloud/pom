#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
platform=""
output="${root}/dist-release"
plugin_crate="enterprise-plugin"
profile="release"

usage() {
  printf '%s\n' \
    'Usage: scripts/build-enterprise-plugin.sh --platform <linux-x86_64|macos-aarch64|windows-x86_64> [options]' \
    '  --output <dir>          diretório do artefato (default: dist-release)' \
    '  --plugin-crate <crate>  crate cdylib (default: enterprise-plugin)' \
    '  --debug                 compila no perfil debug'
}

die() {
  printf 'build-enterprise-plugin: %s\n' "$1" >&2
  exit 2
}

while (($# > 0)); do
  case "$1" in
    --platform) (($# >= 2)) || die '--platform precisa de um valor'; platform="$2"; shift 2 ;;
    --output) (($# >= 2)) || die '--output precisa de um valor'; output="$2"; shift 2 ;;
    --plugin-crate) (($# >= 2)) || die '--plugin-crate precisa de um valor'; plugin_crate="$2"; shift 2 ;;
    --debug) profile="debug"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "opção desconhecida: $1" ;;
  esac
done

[[ -n "$platform" ]] || die '--platform é obrigatório'
case "$platform" in
  linux-x86_64) target="x86_64-unknown-linux-gnu"; extension="so" ;;
  macos-aarch64) target="aarch64-apple-darwin"; extension="dylib" ;;
  windows-x86_64) target="x86_64-pc-windows-msvc"; extension="dll" ;;
  *) die "plataforma não suportada: $platform" ;;
esac

if [[ "$profile" == release ]]; then
  cargo_args=(build --locked --release --target "$target" -p "$plugin_crate")
  artifact_dir="${root}/target/${target}/release"
else
  cargo_args=(build --locked --target "$target" -p "$plugin_crate")
  artifact_dir="${root}/target/${target}/debug"
fi

if [[ "$extension" == dll ]]; then
  artifact="${artifact_dir}/${plugin_crate}.dll"
  [[ -f "$artifact" ]] || artifact="${artifact_dir}/${plugin_crate//-/_}.dll"
else
  artifact="${artifact_dir}/lib${plugin_crate//-/_}.${extension}"
fi

(
  cd "$root"
  cargo "${cargo_args[@]}"
)
[[ -f "$artifact" ]] || die "artefato não encontrado: $artifact"

mkdir -p "$output"
destination="${output}/enterprise-${platform}.${extension}"
cp "$artifact" "$destination"
if command -v shasum >/dev/null 2>&1; then
  sha256="$(shasum -a 256 "$destination" | awk '{print $1}')"
else
  sha256="$(sha256sum "$destination" | awk '{print $1}')"
fi
size="$(wc -c < "$destination" | tr -d '[:space:]')"

printf 'artifact=%s\nsha256=%s\nsize=%s\nplatform=%s\ntarget=%s\n' \
  "$destination" "$sha256" "$size" "$platform" "$target"
