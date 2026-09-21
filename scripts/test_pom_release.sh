#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/pom-release-test.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

assert_contains() {
  local haystack="$1"
  local needle="$2"
  if [[ "$haystack" != *"$needle"* ]]; then
    printf 'expected output to contain %s, got:\n%s\n' "$needle" "$haystack" >&2
    exit 1
  fi
}

first_version="$(printf '%s' '{"releases":[]}' | python3 "$root/scripts/resolve-release-version.py" '0.1.0')"
[[ "$first_version" == '0.1.0+build.1' ]]

next_version="$(printf '%s' '{"releases":[{"version":"0.1.0+build.7"},{"version":"0.1.0+build.10"},{"version":"0.2.0+build.3"}]}' | python3 "$root/scripts/resolve-release-version.py" '0.1.0')"
[[ "$next_version" == '0.2.0+build.4' ]]

fake_bin="$tmp_dir/bin"
mkdir -p "$fake_bin"
cat > "$fake_bin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
set -euo pipefail

target=""
crate=""
profile="debug"
original_args="$*"
while (($# > 0)); do
  case "$1" in
    --target) target="$2"; shift 2 ;;
    -p) crate="$2"; shift 2 ;;
    --release) profile="release"; shift ;;
    *) shift ;;
  esac
done
[[ -n "$target" && -n "$crate" ]]
extension="so"
[[ "$target" == *windows* ]] && extension="dll"
[[ "$target" == *darwin* ]] && extension="dylib"
artifact_dir="target/$target/$profile"
mkdir -p "$artifact_dir"
artifact_name="$(printf '%s' "$crate" | tr '-' '_')"
printf '%s\n' "$original_args" >> "${POM_RELEASE_CARGO_LOG:?}"
if [[ -n "${POM_RELEASE_CARGO_ENV_LOG:-}" ]]; then
  printf 'CUDAARCHS=%s CMAKE_CUDA_ARCHITECTURES=%s\n' \
    "${CUDAARCHS-__unset__}" "${CMAKE_CUDA_ARCHITECTURES-__unset__}" \
    >> "$POM_RELEASE_CARGO_ENV_LOG"
fi
if [[ "$crate" == node ]]; then
  core_artifact="$artifact_dir/pom"
  [[ "$extension" == dll ]] && core_artifact+=".exe"
  printf 'core fixture' > "$core_artifact"
elif [[ "$extension" == dll ]]; then
  printf 'fixture' > "$artifact_dir/$artifact_name.dll"
else
  printf 'fixture' > "$artifact_dir/lib$artifact_name.$extension"
fi
FAKE_CARGO
chmod +x "$fake_bin/cargo"

cat > "$fake_bin/curl" <<'FAKE_CURL'
#!/usr/bin/env bash
set -euo pipefail

last_arg=""
for arg in "$@"; do
  last_arg="$arg"
done
printf '%s\n' "$*" >> "$POM_RELEASE_CURL_LOG"
case "$last_arg" in
  */internal/releases)
    printf '%s' '{"releases":[{"version":"0.1.0+build.7"},{"version":"0.1.0+build.10"}]}'
    ;;
  */internal/releases/upload)
    ;;
  *)
    printf 'unexpected curl URL: %s\n' "$last_arg" >&2
    exit 1
    ;;
esac
FAKE_CURL
chmod +x "$fake_bin/curl"

curl_log="$tmp_dir/curl.log"
cargo_log="$tmp_dir/cargo.log"
cargo_env_log="$tmp_dir/cargo-env.log"
published_output="$(
  PATH="$fake_bin:$PATH" \
  CUDAARCHS='120' \
  CMAKE_CUDA_ARCHITECTURES='120' \
  POM_RELEASE_API='https://license.test' \
  POM_RELEASE_TOKEN='test-token' \
  POM_RELEASE_CURL_LOG="$curl_log" \
  POM_RELEASE_CARGO_LOG="$cargo_log" \
  POM_RDMA_REPO="$root" \
  "$root/pom-release.sh" \
    --platform linux-x86_64 \
    --reason 'teste de incremento'
)"
assert_contains "$published_output" 'release_id=pom_core-0.1.0+build.11-linux-x86_64'
curl_contents="$(cat "$curl_log")"
assert_contains "$curl_contents" 'plugin=pom_core'
assert_contains "$curl_contents" 'status=all'
assert_contains "$curl_contents" 'limit=1000'
assert_contains "$curl_contents" 'version=0.1.0+build.11'

calls_before_manual="$(wc -l < "$curl_log" | tr -d '[:space:]')"
manual_output="$(
  PATH="$fake_bin:$PATH" \
  CUDAARCHS='120' \
  CMAKE_CUDA_ARCHITECTURES='120' \
  POM_RELEASE_API='https://license.test' \
  POM_RELEASE_TOKEN='test-token' \
  POM_RELEASE_CURL_LOG="$curl_log" \
  POM_RELEASE_CARGO_LOG="$cargo_log" \
  POM_RDMA_REPO="$root" \
  "$root/pom-release.sh" \
    --platform linux-x86_64 \
    --version '9.9.9'
)"
assert_contains "$manual_output" 'release_id=pom_core-9.9.9-linux-x86_64'
manual_calls="$(tail -n +"$((calls_before_manual + 1))" "$curl_log")"
if [[ "$manual_calls" == *'/internal/releases?'* ||
      "$manual_calls" == *' /internal/releases '* ||
      "$manual_calls" == *'--data-urlencode plugin='* ]]; then
  printf '%s\n' 'manual --version unexpectedly queried existing releases' >&2
  exit 1
fi

dry_log="$tmp_dir/dry-run-curl.log"
dry_run_output="$(
  PATH="$fake_bin:$PATH" \
  CUDAARCHS='120' \
  CMAKE_CUDA_ARCHITECTURES='120' \
  POM_RELEASE_API='https://license.test' \
  POM_RELEASE_CURL_LOG="$dry_log" \
  POM_RELEASE_CARGO_LOG="$cargo_log" \
  POM_RDMA_REPO="$root" \
  "$root/pom-release.sh" \
    --platform linux-x86_64 \
    --dry-run
)"
assert_contains "$dry_run_output" 'release_id=pom_core-0.1.0+build.1-linux-x86_64'
[[ ! -e "$dry_log" ]]

for platform in linux-x86_64 macos-aarch64 windows-x86_64; do
  core_dry_run_output="$(
    PATH="$fake_bin:$PATH" \
    CUDAARCHS='120' \
    CMAKE_CUDA_ARCHITECTURES='120' \
    POM_RELEASE_API='https://license.test' \
    POM_RELEASE_CARGO_LOG="$cargo_log" \
    POM_RELEASE_CARGO_ENV_LOG="$cargo_env_log" \
    POM_RDMA_REPO="$root" \
    "$root/pom-release.sh" \
      --platform "$platform" \
      --dry-run
  )"
  case "$platform" in
    linux-x86_64)
      assert_contains "$core_dry_run_output" 'release_id=pom_core-0.1.0+build.1-linux-x86_64'
      assert_contains "$core_dry_run_output" '/pom'
      assert_contains "$(tail -n 1 "$cargo_log")" '--features cuda,vulkan -p node --target x86_64-unknown-linux-gnu'
      ;;
    macos-aarch64)
      assert_contains "$core_dry_run_output" 'release_id=pom_core-0.1.0+build.1-macos-aarch64'
      assert_contains "$core_dry_run_output" '/pom'
      assert_contains "$(tail -n 1 "$cargo_log")" '--features metal -p node --target aarch64-apple-darwin'
      ;;
    windows-x86_64)
      assert_contains "$core_dry_run_output" 'release_id=pom_core-0.1.0+build.1-windows-x86_64'
      assert_contains "$core_dry_run_output" '/pom.exe'
      assert_contains "$(tail -n 1 "$cargo_log")" 'build --release --locked -p node --target x86_64-pc-windows-msvc'
      ;;
  esac
  assert_contains "$(tail -n 1 "$cargo_env_log")" 'CUDAARCHS='
  [[ "$(tail -n 1 "$cargo_env_log")" == 'CUDAARCHS= CMAKE_CUDA_ARCHITECTURES=__unset__' ]]
done

override_output="$(
  PATH="$fake_bin:$PATH" \
  CUDAARCHS='120' \
  CMAKE_CUDA_ARCHITECTURES='120' \
  POM_RELEASE_API='https://license.test' \
  POM_RELEASE_CARGO_LOG="$cargo_log" \
  POM_RELEASE_CARGO_ENV_LOG="$cargo_env_log" \
  POM_RDMA_REPO="$root" \
  "$root/pom-release.sh" \
    --platform linux-x86_64 \
    --features metal \
    --dry-run
)"
assert_contains "$override_output" 'release_id=pom_core-0.1.0+build.1-linux-x86_64'
assert_contains "$(tail -n 1 "$cargo_log")" '--features metal -p node --target x86_64-unknown-linux-gnu'
[[ "$(tail -n 1 "$cargo_env_log")" == 'CUDAARCHS= CMAKE_CUDA_ARCHITECTURES=__unset__' ]]

core_published_output="$(
  PATH="$fake_bin:$PATH" \
  CUDAARCHS='120' \
  CMAKE_CUDA_ARCHITECTURES='120' \
  POM_RELEASE_API='https://license.test' \
  POM_RELEASE_TOKEN='test-token' \
  POM_RELEASE_CURL_LOG="$curl_log" \
  POM_RELEASE_CARGO_LOG="$cargo_log" \
  POM_RELEASE_CARGO_ENV_LOG="$cargo_env_log" \
  POM_RDMA_REPO="$root" \
  "$root/pom-release.sh" \
    --platform windows-x86_64 \
    --reason 'teste de pom_core'
)"
assert_contains "$core_published_output" 'release_id=pom_core-0.1.0+build.11-windows-x86_64'
assert_contains "$core_published_output" 'status=published'
curl_contents="$(cat "$curl_log")"
assert_contains "$curl_contents" 'plugin=pom_core'
assert_contains "$curl_contents" 'feature_set=core'
assert_contains "$curl_contents" 'plugin_abi=1'
[[ "$(tail -n 1 "$cargo_env_log")" == 'CUDAARCHS= CMAKE_CUDA_ARCHITECTURES=__unset__' ]]

printf 'pom-release tests: PASS\n'
