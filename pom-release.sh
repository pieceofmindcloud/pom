#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
platform=""
version=""
min_host_version="0.1.0"
channel="stable"
feature_set="core"
reason="release pom_core"
plugin="pom_core"
branch="feat/enterprise"
repo_temp=""
dry_run=false
version_override=false
features_arg=""
features_arg_set=false
feature_set_arg=""
feature_set_arg_set=false

die() {
  printf 'pom-release: %s\n' "$1" >&2
  exit 2
}

usage() {
  printf '%s\n' \
    'Usage: POM_RELEASE_TOKEN=... POM_RELEASE_API=... POM_RDMA_REPO=... ./pom-release.sh --platform linux-x86_64 [options]' \
    '  --version <version>            versão manual; sem ela consulta e incrementa build.N' \
    '  --min-host <version>            versão mínima do host' \
    '  --channel <channel>            canal (default: stable)' \
    '  --features <a,b>               features do build de pom_core' \
    '  --feature-set <a,b>             metadado feature_set do upload (default: core)' \
    '  --reason <text>                 motivo do release' \
    '  --branch <branch>               ref remota (default: feat/enterprise)' \
    '  --repo <path-or-url>             repositório rdma-p2p (default: POM_RDMA_REPO)' \
    '  --dry-run                        resolve e compila, sem enviar ao servidor'
}

# Este script só libera `pom_core` desde que o plugin `enterprise`
# (request_logs/statistics) foi extraído para o repositório pom-plugin-logs,
# que tem seu próprio `release.sh`, workflows do GitHub Actions e script de
# publicação no servidor de licenças. Ver `docs/subsystems/enterprise-licensing.md`.
while (($# > 0)); do
  case "$1" in
    --platform) (($# >= 2)) || die '--platform precisa de um valor'; platform="$2"; shift 2 ;;
    --version) (($# >= 2)) || die '--version precisa de um valor'; version="$2"; version_override=true; shift 2 ;;
    --min-host|--min-host-version) (($# >= 2)) || die '--min-host precisa de um valor'; min_host_version="$2"; shift 2 ;;
    --channel) (($# >= 2)) || die '--channel precisa de um valor'; channel="$2"; shift 2 ;;
    --features) (($# >= 2)) || die '--features precisa de um valor'; features_arg="$2"; features_arg_set=true; shift 2 ;;
    --feature-set) (($# >= 2)) || die '--feature-set precisa de um valor'; feature_set_arg="$2"; feature_set_arg_set=true; shift 2 ;;
    --reason) (($# >= 2)) || die '--reason precisa de um valor'; reason="$2"; shift 2 ;;
    --branch) (($# >= 2)) || die '--branch precisa de um valor'; branch="$2"; shift 2 ;;
    --repo) (($# >= 2)) || die '--repo precisa de um valor'; POM_RDMA_REPO="$2"; shift 2 ;;
    --dry-run) dry_run=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "opção desconhecida: $1" ;;
  esac
done

POM_RELEASE_API="${POM_RELEASE_API:-https://admin-license.pieceofmind.cloud}"
if [[ "$dry_run" != true ]]; then
  [[ -n "${POM_RELEASE_TOKEN:-}" ]] || die 'POM_RELEASE_TOKEN é obrigatório'
fi
[[ -n "${POM_RDMA_REPO:-}" ]] || die 'POM_RDMA_REPO é obrigatório'
[[ -n "$platform" ]] || die '--platform é obrigatório'

# pom_core não tem entitlement Enterprise; feature_set é só metadado do upload.
[[ "$feature_set_arg_set" == true ]] && feature_set="$feature_set_arg"

case "$platform" in
  linux-x86_64)
    os="linux"; arch="x86_64"; core_extension=""; target="x86_64-unknown-linux-gnu"; default_core_features="cuda,vulkan"
    ;;
  macos-aarch64)
    os="macos"; arch="aarch64"; core_extension=""; target="aarch64-apple-darwin"; default_core_features="metal"
    ;;
  windows-x86_64)
    os="windows"; arch="x86_64"; core_extension=".exe"; target="x86_64-pc-windows-msvc"; default_core_features=""
    ;;
  *) die "plataforma não suportada: $platform" ;;
esac

extension="$core_extension"
if [[ "$features_arg_set" == true ]]; then
  core_features="$features_arg"
else
  core_features="$default_core_features"
fi

repo="$POM_RDMA_REPO"
if [[ -d "$repo" ]] && git -C "$repo" rev-parse --git-dir >/dev/null 2>&1; then
  repo="$(cd "$repo" && pwd -P)"
else
  repo_temp="$(mktemp -d "${TMPDIR:-/tmp}/pom-release-repo.XXXXXX")"
  git clone --no-checkout "$POM_RDMA_REPO" "$repo_temp/repo" >/dev/null
  repo="$repo_temp/repo"
fi

git -C "$repo" fetch origin "$branch"
worktree="$(mktemp -d "${TMPDIR:-/tmp}/pom-release.XXXXXX")"
cleanup() {
  git -C "$repo" worktree remove --force "$worktree" >/dev/null 2>&1 || true
  rmdir "$worktree" >/dev/null 2>&1 || true
  if [[ -n "$repo_temp" ]]; then rm -rf "$repo_temp"; fi
}
trap cleanup EXIT
git -C "$repo" worktree add --detach "$worktree" "origin/$branch" >/dev/null

if [[ -z "$version" ]]; then
  version="$(awk '
    $0 == "[workspace.package]" { in_workspace = 1; next }
    in_workspace && /^\[/ { exit }
    in_workspace && /^version = / {
      sub(/^version = "/, "")
      sub(/"$/, "")
      print
      exit
    }
  ' "$worktree/Cargo.toml")"
fi
[[ -n "$version" ]] || version="0.1.0"

if [[ "$version_override" != true ]]; then
  if [[ "$dry_run" == true ]]; then
    release_list='{"releases":[]}'
  else
    releases_url="${POM_RELEASE_API%/}"
    if [[ "$releases_url" == */internal/releases/upload ]]; then
      releases_url="${releases_url%/internal/releases/upload}"
    fi
    [[ "$releases_url" == */internal/releases ]] || releases_url="${releases_url}/internal/releases"
    if ! release_list="$(curl --fail --silent --show-error \
      -H "Authorization: Bearer ${POM_RELEASE_TOKEN}" \
      --get \
      --data-urlencode "plugin=${plugin}" \
      --data-urlencode 'status=all' \
      --data-urlencode 'limit=1000' \
      "$releases_url")"; then
      die 'falha ao consultar as releases existentes'
    fi
  fi
  if ! version="$(printf '%s' "$release_list" | python3 "$root/scripts/resolve-release-version.py" "$version")"; then
    die 'resposta de releases inválida ou versão fora do SemVer'
  fi
fi

artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/pom-release-artifact.XXXXXX")"
trap 'rm -rf "$artifact_dir"; cleanup' EXIT
cargo_args=(build --release --locked)
[[ -n "$core_features" ]] && cargo_args+=(--features "$core_features")
cargo_args+=(-p node --target "$target")
(
  cd "$worktree"
  # Releases must not inherit the build machine's GPU. An empty CUDAARCHS is
  # treated as unset by CMake, which lets the vendored ggml CMakeLists choose
  # its broad architecture list. Remove the explicit CMake override too,
  # since vendor/llama-cpp-sys-2 forwards CMAKE_* variables to that cache.
  unset CMAKE_CUDA_ARCHITECTURES
  CUDAARCHS='' cargo "${cargo_args[@]}"
) >/dev/null
built_artifact="$worktree/target/$target/release/pom$extension"
artifact="$artifact_dir/pom$extension"
[[ -f "$built_artifact" ]] || die "binário pom não encontrado: $built_artifact"
cp "$built_artifact" "$artifact"
[[ -f "$artifact" ]] || die "artefato não encontrado: $artifact"

if command -v shasum >/dev/null 2>&1; then
  artifact_sha256="$(shasum -a 256 "$artifact" | awk '{print $1}')"
else
  artifact_sha256="$(sha256sum "$artifact" | awk '{print $1}')"
fi
artifact_size="$(wc -c < "$artifact" | tr -d '[:space:]')"
release_channel_suffix=""
[[ "$channel" == "stable" ]] || release_channel_suffix="-${channel}"
release_id="${plugin}-${version}${release_channel_suffix}-${os}-${arch}"

if [[ "$dry_run" == true ]]; then
  printf 'release_id=%s\nstatus=dry-run\nsha256=%s\nsize=%s\nartifact=%s\n' \
    "$release_id" "$artifact_sha256" "$artifact_size" "$artifact"
  exit 0
fi

upload_url="${POM_RELEASE_API%/}"
[[ "$upload_url" == */internal/releases/upload ]] || upload_url="${upload_url}/internal/releases/upload"
curl --fail --silent --show-error \
  -H "Authorization: Bearer ${POM_RELEASE_TOKEN}" \
  -F "plugin=${plugin}" \
  -F "version=${version}" \
  -F "os=${os}" \
  -F "arch=${arch}" \
  -F "plugin_abi=1" \
  -F "min_host_version=${min_host_version}" \
  -F "channel=${channel}" \
  -F "feature_set=${feature_set}" \
  -F "reason=${reason}" \
  -F "artifact=@${artifact}" \
  "$upload_url" >/dev/null
printf 'release_id=%s\nstatus=published\nsha256=%s\nsize=%s\n' \
  "$release_id" "$artifact_sha256" "$artifact_size"
