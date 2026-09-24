#!/usr/bin/env bash
# Prove the Dockerfile's cache graph structurally and, when Docker is available,
# with cheap BuildKit builds derived from the real instructions.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
dockerfile="$repo_root/Dockerfile"
dockerignore="$repo_root/.dockerignore"
stub_base_image=${POM_CACHE_TEST_IMAGE:-busybox:1.36}

failures=0
note() { printf '  %s\n' "$*"; }
pass() { printf '  ok   %s\n' "$*"; }
fail() { printf '  FAIL %s\n' "$*"; failures=$((failures + 1)); }

# Parse one logical Dockerfile instruction per line as <stage>\t<keyword>\t<rest>.
parse_instructions() {
    awk '
        function flush() {
            if (buf == "") return
            gsub(/[ \t]+/, " ", buf)
            sub(/^ /, "", buf)
            kw = buf
            sub(/ .*/, "", kw)
            rest = buf
            sub(/^[^ ]+ ?/, "", rest)
            if (toupper(kw) == "FROM") {
                stage = "stage" (++nstage)
                if (match(tolower(rest), / as [a-z0-9_.-]+$/))
                    stage = substr(rest, RSTART + 4)
            }
            printf "%s\t%s\t%s\n", stage, toupper(kw), rest
            buf = ""
        }
        /^[ \t]*#/ || /^[ \t]*$/ { next }
        {
            line = $0
            if (line ~ /\\[ \t]*$/) {
                sub(/\\[ \t]*$/, "", line)
                buf = buf " " line
                next
            }
            buf = buf " " line
            flush()
        }
        END { flush() }
    ' "$1"
}

all_instructions=$(parse_instructions "$dockerfile")
base=$(printf '%s\n' "$all_instructions" | awk -F '\t' '$2 == "FROM" && $3 ~ / AS build-base$/ { print $1; exit }')
builder=builder
frontend=frontend
runtime_base=$(printf '%s\n' "$all_instructions" | awk -F '\t' '$2 == "FROM" && $3 ~ / AS runtime-base$/ { print $1; exit }')
runtime=$(printf '%s\n' "$all_instructions" | awk -F '\t' '$2 == "FROM" && $3 ~ / AS runtime$/ { print $1; exit }')
in_stage() { printf '%s\n' "$all_instructions" | awk -F '\t' -v s="$1" '$1 == s { print $2 "\t" $3 }'; }
base_instructions=$(in_stage "$base")
builder_instructions=$(in_stage "$builder")
frontend_instructions=$(in_stage "$frontend")
runtime_base_instructions=$(in_stage "$runtime_base")
runtime_instructions=$(in_stage "$runtime")

index_in() {
    printf '%s\n' "$1" | awk -F '\t' -v re="$2" '{ if (($1 " " $2) ~ re) { print NR; found=1; exit } } END { if (!found) print 0 }'
}
ordered() {
    if [ "$2" -gt 0 ] && [ "$3" -gt 0 ] && [ "$2" -lt "$3" ]; then pass "$1"; else fail "$1 ($2 before $3 required)"; fi
}

printf '1. Dockerfile structure (shared: %s, Rust: %s, frontend: %s, runtime: %s)\n' "$base" "$builder" "$frontend" "$runtime"
whole_context=$(printf '%s\n' "$all_instructions" | awk -F '\t' '$2 == "COPY" && $3 !~ /--from=/ { line=$3; sub(/^(--[^ ]+ )+/, "", line); n=split(line,tok," "); for(i=1;i<n;i++) if(tok[i]=="." || tok[i]=="./") {print; exit} }')
if [ -n "$whole_context" ]; then fail "a stage copies the whole context (COPY $whole_context)"; else pass "no COPY of the whole build context"; fi

ordered "npm dependencies are isolated in the frontend stage" \
    "$(index_in "$frontend_instructions" '^COPY .*apps/frontend/package')" \
    "$(index_in "$frontend_instructions" '^RUN .*npm ci')"
ordered "rustup is cached before backend source copies" \
    "$(index_in "$builder_instructions" '^RUN .*sh.rustup.rs')" \
    "$(index_in "$builder_instructions" '^COPY --from=llama-fork')"
ordered "the llama.cpp fork precedes backend sources" \
    "$(index_in "$builder_instructions" '^COPY .*llama\.cpp/')" \
    "$(index_in "$builder_instructions" '^COPY .*crates/')"
ordered "backend sources precede Cargo" \
    "$(index_in "$builder_instructions" '^COPY .*crates/')" \
    "$(index_in "$builder_instructions" '^RUN .*cargo build')"
ordered "frontend sources precede the frontend build" \
    "$(index_in "$frontend_instructions" '^COPY apps/frontend/ ')" \
    "$(index_in "$frontend_instructions" '^RUN .*npm run build')"
if printf '%s\n' "$frontend_instructions" | tr '\t' ' ' | grep -Fq 'COPY --parents ./crates/**/AGENTS.md ./'; then
    pass "frontend receives crate docs without Rust source trees"
else
    fail "frontend stage must copy only crates/**/AGENTS.md for generated docs"
fi

if printf '%s\n' "$all_instructions" | awk -F '\t' -v b="$builder" -v f="$frontend" '$1==b || $1==f { if($2=="ARG" && $3 ~ /^POM_BUILD_COMMIT/) found=1 } END { exit !found }'; then
    fail "POM_BUILD_COMMIT must not be in either build stage"
else
    pass "POM_BUILD_COMMIT is excluded from Cargo and frontend cache keys"
fi
if printf '%s\n' "$runtime_base_instructions" | grep -q '^ARG[[:space:]]POM_BUILD_COMMIT' \
   && printf '%s\n' "$runtime_base_instructions" | grep -q '^ENV[[:space:]]POM_BUILD_COMMIT=\${POM_BUILD_COMMIT}'; then
    pass "runtime base exports the host commit stamp"
else
    fail "runtime base must export POM_BUILD_COMMIT from its build arg"
fi

for arg in LLAMA_CPP_CACHE_KEY POM_CUDA_ARCH_CACHE_KEY POM_CUDA_ARCHITECTURES POM_LLAMA_CPP_MODE POM_LLAMA_CPP_RELEASE_TAG POM_LLAMA_CPP_RELEASE_REPOSITORY RUST_TOOLCHAIN; do
    if printf '%s\n' "$builder_instructions" | grep -q "^ARG[[:space:]]$arg="; then
        pass "$arg is scoped to the Rust builder"
    else
        fail "$arg is missing from the Rust builder"
    fi
done
if printf '%s\n' "$builder_instructions" | grep -q 'id=pom-target-\${LLAMA_CPP_CACHE_KEY}-\${CUDA_VERSION}-\${POM_CUDA_ARCH_CACHE_KEY}-\${RUST_TOOLCHAIN}'; then
    pass "native target cache is isolated by fork, CUDA, architecture and Rust toolchain"
else
    fail "native target cache key is missing a native build input"
fi
if printf '%s\n' "$builder_instructions" | grep -Fq 'cargo build --release --features cuda,vulkan' \
   && ! printf '%s\n' "$builder_instructions" | grep -Fq 'cargo build --release --features vulkan' \
   && ! printf '%s\n' "$builder_instructions" | grep -Fq 'cargo build --release --no-default-features' \
   && printf '%s\n' "$builder_instructions" | grep -Fq 'libggml-cpu-*.so* /build/llama-runtime/cuda-vulkan/' \
   && printf '%s\n' "$builder_instructions" | grep -Fq 'libggml-vulkan*.so* /build/llama-runtime/cuda-vulkan/'; then
    pass "one POM build combines CUDA+Vulkan with CPU/Vulkan/CUDA runtime modules"
else
    fail "the universal POM must compile once and package all runtime backend modules together"
fi
for target in cuda-vulkan plugin-host; do
    if printf '%s\n' "$builder_instructions" | grep -Fq "CARGO_TARGET_DIR=/build/target/$target"; then pass "Cargo target is present for $target"; else fail "Cargo target missing for $target"; fi
done
if [ "$(index_in "$frontend_instructions" '^RUN .*npm run build')" -eq "$(printf '%s\n' "$frontend_instructions" | awk -F '\t' '$1=="RUN" {n=NR} END {print n+0}')" ]; then
    pass "the frontend build is the last RUN in its independent stage"
else
    fail "frontend npm build should be the last RUN in its stage"
fi
runtime_last_instruction=$(printf '%s\n' "$runtime_instructions" | tail -n 1 | tr '\t' ' ')
if [ "$runtime_last_instruction" = 'COPY --from=frontend /build/apps/frontend/dist /app/apps/frontend/dist' ]; then
    pass "the frontend bundle is the final runtime image layer"
else
    fail "the final runtime instruction must copy only the frontend bundle"
fi

printf '\n2. .dockerignore keeps local build artifacts and secrets out of the context\n'
for pattern in '\*\*/target/' '\*\*/\.git/' '\*\*/\*\.log' '\*\*/\*\.tsbuildinfo' '\*\*/\*\.gguf' '\*\*/\*\.part' '\*\*/\*\.key' '\*\*/\*\.pem' '\*\*/\.env' '\*\*/\.env\.\*' 'tests/' 'apps/frontend/src/docs/' 'apps/frontend/src/brand/' 'apps/frontend/public/brand/' 'llama\.cpp/' '\*\*/node_modules/'; do
    if grep -qE "^${pattern}$" "$dockerignore"; then pass "$(printf '%s' "$pattern" | sed 's/\\//g')"; else fail "$pattern is not excluded"; fi
done
if grep -qE '^\*\*/tests/?$' "$dockerignore"; then
    fail "do not exclude nested tests; Cargo needs crates/node/tests/support/fake_plugin_host.rs"
else
    pass "nested Cargo test-support target sources remain available"
fi

printf '\n3. BuildKit cache behaviour\n'
if ! command -v docker >/dev/null 2>&1 || ! docker version --format '{{.Server.Version}}' >/dev/null 2>&1; then
    note "skipped: no reachable Docker daemon"
    if [ "$failures" -eq 0 ]; then printf '\ndocker build cache test passed (structure only)\n'; exit 0; fi
    printf '%s structural check(s) failed\n' "$failures"; exit 1
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/pom-docker-cache.XXXXXX")
trap 'rm -rf "$work"' EXIT
ctx="$work/ctx"
stub="$work/Dockerfile.stub"
mkdir -p "$ctx"

# Keep the real base, Rust and frontend stages and order; replace only external
# FROM images and RUN bodies. This exercises BuildKit's actual layer/cache rules.
{
    printf '# syntax=docker/dockerfile:1\n'
    printf '%s\n' "$all_instructions" | awk -F '\t' -v base="$base" -v b="$builder" -v f="$frontend" -v img="$stub_base_image" '
        function tag_for(body) {
            if (body ~ /npm ci/) return "npm-ci"
            if (body ~ /npm run build/) return "ui-build"
            if (body ~ /cargo build/) return "cargo"
            if (body ~ /rustup/) return "rustup"
            if (body ~ /apt-get/) return "apt"
            return "run" (++other)
        }
        $1 != base && $1 != b && $1 != f { next }
        $2 == "FROM" {
            split($3, parts, " ")
            if (parts[1] == base || parts[1] == b || parts[1] == f)
                printf "FROM %s AS %s\n", parts[1], $1
            else
                printf "FROM %s AS %s\n", img, $1
            next
        }
        $2 == "RUN" {
            body=$3; flags=""
            while (match(body, /^--[^ ]+ /)) { flags=flags substr(body,RSTART,RLENGTH); body=substr(body,RSTART+RLENGTH) }
            suffix=(body ~ /rustup/ ? " ${RUST_TOOLCHAIN}" : "")
            printf "RUN %secho POM-STEP:%s%s\n", flags, tag_for(body), suffix
            next
        }
        { printf "%s %s\n", $2, $3 }
    '
} > "$stub"
cp "$dockerignore" "$ctx/.dockerignore"

# Materialize every source used by the three retained stages.
printf '%s\n' "$all_instructions" | awk -F '\t' -v base="$base" -v b="$builder" -v f="$frontend" '$1==base || $1==b || $1==f { if($2=="COPY" && $3 !~ /--from=/) { line=$3; sub(/^(--[^ ]+ )+/,"",line); n=split(line,tok," "); for(i=1;i<n;i++) print tok[i] } }' | sort -u | while read -r src; do
    case "$src" in
        .|./) continue ;;
        *crates/**/AGENTS.md) continue ;;
        */) mkdir -p "$ctx/$src" && printf 'stub\n' > "$ctx/${src}.stub" ;;
        *) mkdir -p "$ctx/$(dirname "$src")" && printf 'stub\n' > "$ctx/$src" ;;
    esac
done

run_id="$(date +%s)-$$"
mkdir -p "$ctx/llama-fork/ggml/src" "$ctx/crates/node/src" "$ctx/apps/frontend/src"
printf 'crate docs\n' > "$ctx/crates/AGENTS.md"
printf 'node docs\n' > "$ctx/crates/node/AGENTS.md"
seed() {
    mkdir -p "$ctx/$(dirname "$1")"
    printf '%s %s\n' "$2" "$run_id" > "$ctx/$1"
}
seed llama-fork/ggml/src/probe.c v1
seed crates/node/src/probe.rs v1
seed apps/frontend/src/probe.tsx v1
seed apps/frontend/package-lock.json deps-v1

build_commit=abc1234
llama_cache_key=def5678-clean
cuda_version=13.2.1
cuda_architectures='86;120'
cuda_arch_key=86-120
llama_mode=release
llama_release_tag=sample-release-tag
llama_release_repository=pereirajair/llama.cpp
cuda_runtime_flavor=runtime
rust_toolchain=stable

build_target_groups() {
    local target=$1
    docker build --progress=plain --target "$target" \
        --build-context "llama-fork=$ctx/llama-fork" \
        --build-arg "POM_BUILD_COMMIT=$build_commit" \
        --build-arg "LLAMA_CPP_CACHE_KEY=$llama_cache_key" \
        --build-arg "CUDA_VERSION=$cuda_version" \
        --build-arg "POM_CUDA_ARCHITECTURES=$cuda_architectures" \
        --build-arg "POM_CUDA_ARCH_CACHE_KEY=$cuda_arch_key" \
        --build-arg "POM_LLAMA_CPP_MODE=$llama_mode" \
        --build-arg "POM_LLAMA_CPP_RELEASE_TAG=$llama_release_tag" \
        --build-arg "POM_LLAMA_CPP_RELEASE_REPOSITORY=$llama_release_repository" \
        --build-arg "CUDA_RUNTIME_FLAVOR=$cuda_runtime_flavor" \
        --build-arg "RUST_TOOLCHAIN=$rust_toolchain" \
        -f "$stub" -t pom-cache-test:probe "$ctx" 2>&1 \
    | awk -v base="$base" -v builder="$builder" -v frontend="$frontend" -v debug="${POM_CACHE_TEST_DEBUG:-}" '
        function group(stage,instr) {
            if (instr ~ /POM-STEP:cargo/) return "backend"
            if (instr ~ /POM-STEP:ui-build/) return "ui"
            if (instr ~ /POM-STEP:npm-ci/ || instr ~ /apps\/frontend\/package/) return "npm-deps"
            if (instr ~ /POM-STEP:rustup/) return "toolchain"
            if (instr ~ /POM-STEP:apt/) return "base"
            if (instr ~ /llama\.cpp\// || instr ~ /^COPY vendor\//) return "llama"
            if (stage == "frontend") return "ui"
            if (instr ~ /^COPY crates\// || instr ~ /^COPY Cargo\./ || instr ~ /^COPY \.cargo\// || instr ~ /^COPY assets\//) return "backend"
            return "base"
        }
        /^#[0-9]+ \[[^]]+\] / {
            id=substr($1,2); label=$0
            sub(/^#[0-9]+ \[/,"",label); sub(/\].*/,"",label)
            split(label,fields," "); stage=fields[1]
            if (stage != base && stage != builder && stage != frontend) next
            instr=$0; sub(/^#[0-9]+ \[[^]]+\] /,"",instr)
            if (instr ~ /^FROM /) next
            step[id]=instr; step_stage[id]=stage; next
        }
        /^#[0-9]+ CACHED$/ { cached[substr($1,2)]=1; next }
        END {
            for(id in step) {
                g=group(step_stage[id],step[id])
                if(debug) printf "      %-8s %s %s\n",g,(id in cached?"CACHED":"RAN   "),step[id] > "/dev/stderr"
                if(!(id in cached)) dirty[g]=1
            }
            for(g in dirty) printf "%s\n",g
        }'
}

rebuilt_groups() {
    local result=""
    for target in "$builder" "$frontend"; do
        local groups
        groups=$(build_target_groups "$target")
        result="$result $groups"
    done
    printf '%s\n' "$result" | tr ' ' '\n' | sed '/^$/d' | sort -u | paste -sd ' ' -
}
scenario() {
    local name=$1 expected=$2 got
    got=$(rebuilt_groups)
    if [ "$got" = "$expected" ]; then pass "$name -> rebuilt: ${got:-<nothing>}"; else fail "$name -> rebuilt: ${got:-<nothing>} (expected: ${expected:-<nothing>})"; fi
}

if ! build_target_groups "$builder" >"$work/cold-builder.log" || ! build_target_groups "$frontend" >"$work/cold-frontend.log"; then
    fail "the derived BuildKit stub does not build"
    tail -n 8 "$work/cold-builder.log" "$work/cold-frontend.log" 2>/dev/null || true
    exit 1
fi
scenario "nothing changed" ""
build_commit=def4567
scenario "only the app commit stamp changed" ""
build_commit=abc1234

mkdir -p "$ctx/apps/frontend/src/docs" "$ctx/llama.cpp/.git"
seed crates/node/infer_server_4242.log 'runtime log'
seed apps/frontend/tsconfig.app.tsbuildinfo '{"stale":true}'
seed apps/frontend/src/docs/AGENTS.md '# generated'
seed llama.cpp/.git/HEAD 'ref: refs/heads/pom-tensor-parallel'
seed crates/node/local.gguf 'not a build input'
seed crates/node/identity.key 'not a build input'
seed crates/node/.env.local 'not a build input'
scenario "only local build artifacts and credentials appeared" ""
seed apps/frontend/package-lock.json deps-v2
scenario "frontend dependencies changed" "npm-deps ui"
seed apps/frontend/package-lock.json deps-v1

seed apps/frontend/src/probe.tsx v2
scenario "apps/frontend changed" "ui"
seed crates/node/src/probe.rs v2
scenario "backend Rust source changed while UI is cached" "backend"
seed llama-fork/ggml/src/probe.c v2
scenario "fork changed while frontend sources stay unchanged" "backend llama"

llama_cache_key=0bad1dea-clean
scenario "fork cache identity changed" "backend"
llama_cache_key=def5678-dirty-contents
scenario "fork source identity changed" "backend"
llama_cache_key=def5678-clean
cuda_arch_key=86
scenario "CUDA architecture cache key changed" "backend"
cuda_arch_key=86-120
rust_toolchain=1.90.0
# The toolchain ARG and rustup layer precede source COPYs so source edits reuse
# rustup; changing the toolchain therefore also refreshes those later COPYs.
scenario "Rust toolchain changed" "backend llama toolchain"
docker rmi -f pom-cache-test:probe >/dev/null 2>&1 || true
printf '\n'
if [ "$failures" -eq 0 ]; then printf 'docker build cache test passed\n'; exit 0; fi
printf '%s check(s) failed\n' "$failures"
exit 1
