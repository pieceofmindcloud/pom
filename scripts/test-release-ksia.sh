#!/usr/bin/env bash
# Checks scripts/release-ksia.sh without building: every variant in --dry-run
# must resolve the right commands, flags and package name.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
script="$root/scripts/release-ksia.sh"
fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }
has() { [[ "$1" == *"$2"* ]] || fail "expected '$2' in:\n$1"; }
lacks() { [[ "$1" != *"$2"* ]] || fail "did not expect '$2'"; }

export POM_RELEASE_CACHE="$(mktemp -d "${TMPDIR:-/tmp}/pom-release-ksia.XXXXXX")"
trap 'rm -rf "$POM_RELEASE_CACHE"' EXIT

out="$("$script" --variant linux-cpu --version v1.2.3 --dry-run)"
has "$out" "+ cargo build --release --locked -p node -p pom-plugin-host"
has "$out" "release/pom-plugin-host "
lacks "$out" "--features cuda"
has "$out" "package=$root/dist-release-ksia/pom-v1.2.3-x86_64-unknown-linux-gnu-cpu.tar.gz"
has "$out" "+ npm run build"

out="$("$script" --variant linux-cuda --version v1.2.3 --dry-run)"
has "$out" "+ cargo build --release --locked -p node -p pom-plugin-host --features cuda"
has "$out" "package=$root/dist-release-ksia/pom-v1.2.3-x86_64-unknown-linux-gnu-cuda.tar.gz"
# The full architecture list comes from an empty CUDAARCHS in the child env.
grep -q "export CUDAARCHS=''" "$script" || fail "CUDAARCHS must be emptied for the CUDA variant"
grep -q "unset CMAKE_CUDA_ARCHITECTURES" "$script" || fail "CMAKE_CUDA_ARCHITECTURES must be unset"

out="$(POM_RELEASE_SKIP_UI=1 "$script" --variant windows-cpu --version v1.2.3 --dry-run)"
has "$out" "+ cargo xwin build --release --locked -p node -p pom-plugin-host --target x86_64-pc-windows-msvc"
has "$out" "release/pom-plugin-host.exe "
has "$out" "+ rustup target add x86_64-pc-windows-msvc"
has "$out" "clang-cl"
has "$out" "lld-link"
has "$out" "package=$root/dist-release-ksia/pom-v1.2.3-x86_64-pc-windows-msvc.zip"
lacks "$out" "+ npm run build"

out="$(POM_RELEASE_SKIP_UI=1 "$script" --variant windows-cpu --version v1.2.3 --debug-symbols --dry-run 2>&1)"
has "$out" "target-windows-cpu-debug"
has "$out" "pom.pdb"
"$script" --variant linux-cpu --version v1.2.3 --debug-symbols --dry-run >/dev/null 2>&1 && fail "--debug-symbols is windows-cpu only"

"$script" --variant linux-cpu --version 1.2.3 --dry-run >/dev/null 2>&1 && fail "a version without v must fail"
"$script" --variant macos --version v1.2.3 --dry-run >/dev/null 2>&1 && fail "an unknown variant must fail"

workflow="$root/.github/workflows/release-ksia.yml"
grep -E '^\s*-?\s*uses:\s*actions/(upload-artifact|download-artifact|cache)' "$workflow" && fail "the ksia workflow must not store anything on GitHub"
grep -E '^\s*cache:\s*npm' "$workflow" && fail "setup-node cache lives on GitHub"
[[ "$(grep -c 'runs-on: \[self-hosted, Linux, X64, ksia\]' "$workflow")" == 3 ]] || fail "every job must run on the ksia runner"

printf 'test-release-ksia: ok\n'
