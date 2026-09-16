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
if [[ "$extension" == dll ]]; then
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
published_output="$(
  PATH="$fake_bin:$PATH" \
  POM_RELEASE_API='https://license.test' \
  POM_RELEASE_TOKEN='test-token' \
  POM_RELEASE_CURL_LOG="$curl_log" \
  POM_RDMA_REPO="$root" \
  "$root/pom-release.sh" \
    --platform linux-x86_64 \
    --plugin enterprise \
    --plugin-crate enterprise-plugin-fixture \
    --reason 'teste de incremento'
)"
assert_contains "$published_output" 'release_id=ent-0.1.0+build.11-linux-x86_64'
assert_contains "$(<"$curl_log")" 'plugin=enterprise'
assert_contains "$(<"$curl_log")" 'status=all'
assert_contains "$(<"$curl_log")" 'limit=1000'
assert_contains "$(<"$curl_log")" 'version=0.1.0+build.11'

calls_before_manual="$(wc -l < "$curl_log" | tr -d '[:space:]')"
manual_output="$(
  PATH="$fake_bin:$PATH" \
  POM_RELEASE_API='https://license.test' \
  POM_RELEASE_TOKEN='test-token' \
  POM_RELEASE_CURL_LOG="$curl_log" \
  POM_RDMA_REPO="$root" \
  "$root/pom-release.sh" \
    --platform linux-x86_64 \
    --plugin enterprise \
    --plugin-crate enterprise-plugin-fixture \
    --version '9.9.9'
)"
assert_contains "$manual_output" 'release_id=ent-9.9.9-linux-x86_64'
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
  POM_RELEASE_API='https://license.test' \
  POM_RELEASE_CURL_LOG="$dry_log" \
  POM_RDMA_REPO="$root" \
  "$root/pom-release.sh" \
    --platform linux-x86_64 \
    --plugin enterprise \
    --plugin-crate enterprise-plugin-fixture \
    --dry-run
)"
assert_contains "$dry_run_output" 'release_id=ent-0.1.0+build.1-linux-x86_64'
[[ ! -e "$dry_log" ]]

printf 'pom-release tests: PASS\n'
