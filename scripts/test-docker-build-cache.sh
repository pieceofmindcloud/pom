#!/usr/bin/env bash
#
# Proves that the image build reuses what did not change.
#
# Two tiers:
#
#   1. Structure. Reads `Dockerfile` and `.dockerignore` and checks the
#      properties the layer order depends on: no `COPY . .`, the fork above the
#      backend, the backend above the interface, the interface build last, the
#      target cache mount still keyed to the fork's commit, and the artifacts a
#      local build drops in the tree kept out of the context. No Docker needed.
#
#   2. Cache. Runs real BuildKit builds over a stub derived from `Dockerfile` -
#      same instructions, same order, same ARGs, same cache mounts, with the
#      heavy `FROM`s and `RUN` bodies replaced - against a synthetic context,
#      and asserts which layer groups re-executed in six scenarios:
#
#        nothing changed                     -> nothing rebuilds
#        only local build artifacts appear   -> nothing rebuilds
#        apps/frontend changed               -> interface only
#        + the backend changed               -> interface and backend
#        + the llama.cpp fork changed        -> all three
#        the fork's commit changed           -> backend (new target cache id)
#
#      This tier needs a working Docker daemon and is skipped without one; the
#      structure tier still fails the run on its own.
#
# The stub is what makes tier 2 cheap enough to run: the real builder image is
# CUDA + rustup + npm and the real `RUN`s are a 4-variant LTO build, so proving
# the cache graph by building it for real is not something anyone would run.
# What the stub keeps is exactly what decides the graph - the instruction order
# and the COPY sources.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
dockerfile="$repo_root/Dockerfile"
dockerignore="$repo_root/.dockerignore"
stub_base_image=${POM_CACHE_TEST_IMAGE:-busybox:1.36}

failures=0
note() { printf '  %s\n' "$*"; }
pass() { printf '  ok   %s\n' "$*"; }
fail() { printf '  FAIL %s\n' "$*"; failures=$((failures + 1)); }

# ── Dockerfile parsing ────────────────────────────────────────────────────────
# One logical instruction per line, continuations joined, comments dropped, in
# file order. `<stage>\t<keyword>\t<rest>`; the stage is the `AS <name>` of the
# FROM that opened it, or its index when it has none.
parse_instructions() {
    awk '
        function flush() {
            if (buf == "") return
            gsub(/[ \t]+/, " ", buf)
            sub(/^ /, "", buf)
            kw = buf
            sub(/ .*$/, "", kw)
            rest = buf
            sub(/^[^ ]+ ?/, "", rest)
            if (toupper(kw) == "FROM") {
                stage = "stage" (++nstage)
                if (match(tolower(rest), / as [a-z0-9_.-]+$/)) {
                    stage = substr(rest, RSTART + 4)
                }
            }
            printf "%s\t%s\t%s\n", stage, toupper(kw), rest
            buf = ""
        }
        /^[ \t]*#/ { next }
        /^[ \t]*$/ { next }
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

builder_stage_name() {
    parse_instructions "$dockerfile" | awk -F'\t' '$2 == "FROM" { print $1; exit }'
}

builder=$(builder_stage_name)
instructions=$(parse_instructions "$dockerfile" | awk -F'\t' -v s="$builder" '$1 == s { print $2 "\t" $3 }')

# Index (1-based) of the first instruction whose "<KEYWORD> <rest>" matches the
# extended regex; 0 when there is none.
index_of() {
    printf '%s\n' "$instructions" | awk -F'\t' -v re="$1" '
        { if (($1 " " $2) ~ re) { print NR; found = 1; exit } }
        END { if (!found) print 0 }'
}

last_index_of() {
    printf '%s\n' "$instructions" | awk -F'\t' -v re="$1" '
        { if (($1 " " $2) ~ re) hit = NR }
        END { print hit + 0 }'
}

# ── Tier 1: structure ─────────────────────────────────────────────────────────
printf '1. Dockerfile structure (builder stage: %s)\n' "$builder"

whole_context=$(printf '%s\n' "$instructions" | awk -F'\t' '
    $1 == "COPY" {
        line = $2
        sub(/^(--[^ ]+ )+/, "", line)
        n = split(line, tok, " ")
        for (i = 1; i < n; i++) if (tok[i] == "." || tok[i] == "./") { print; exit }
    }')
if [ -n "$whole_context" ]; then
    fail "the builder stage copies the whole context (COPY $whole_context)"
    note "     one unrelated file anywhere in the tree then re-runs every step below it"
else
    pass "no COPY of the whole build context"
fi

i_npm_ci=$(index_of '^RUN .*npm ci')
i_llama=$(index_of '^COPY .*llama\.cpp/')
i_crates=$(index_of '^COPY .*(^| )crates/')
i_cargo=$(index_of '^RUN .*cargo build')
i_ui_src=$(index_of '^COPY apps/frontend/ ')
i_ui_build=$(index_of '^RUN .*npm run build')
i_last_run=$(last_index_of '^RUN ')
i_commit_arg=$(index_of '^ARG POM_BUILD_COMMIT')

ordered() { # ordered <label> <earlier-idx> <later-idx> <earlier-name> <later-name>
    if [ "$2" -gt 0 ] && [ "$3" -gt 0 ] && [ "$2" -lt "$3" ]; then
        pass "$1"
    else
        fail "$1 ($4 at $2, $5 at $3)"
    fi
}

ordered "node_modules install sits above the backend" "$i_npm_ci" "$i_cargo" "npm ci" "cargo build"
ordered "the llama.cpp fork is copied before the backend sources" "$i_llama" "$i_crates" "llama.cpp" "crates"
ordered "the backend sources are copied before the Rust build" "$i_crates" "$i_cargo" "crates" "cargo build"
ordered "the interface sources are copied after the Rust build" "$i_cargo" "$i_ui_src" "cargo build" "apps/frontend"
ordered "the interface is built after its sources are copied" "$i_ui_src" "$i_ui_build" "apps/frontend" "npm run build"

if [ "$i_ui_build" -gt 0 ] && [ "$i_ui_build" -eq "$i_last_run" ]; then
    pass "the interface build is the last RUN of the builder stage"
else
    fail "the interface build is not the last RUN of the builder stage (it is #$i_ui_build, last is #$i_last_run)"
fi

if [ "$i_commit_arg" -gt 0 ] && [ "$i_crates" -gt 0 ] && [ "$i_llama" -gt 0 ] \
   && [ "$i_commit_arg" -gt "$i_crates" ] && [ "$i_commit_arg" -gt "$i_llama" ]; then
    pass "ARG POM_BUILD_COMMIT is declared below the fork and backend COPYs"
    note "     an in-scope ARG is part of the cache key of every RUN under it,"
    note "     referenced or not, so it has to be declared as late as possible"
else
    fail "ARG POM_BUILD_COMMIT (#$i_commit_arg) must come after the llama.cpp (#$i_llama) and crates (#$i_crates) COPYs"
fi

if printf '%s\n' "$instructions" | grep -q 'id=pom-target-\${LLAMA_CPP_COMMIT}'; then
    pass "the /build/target cache mount is still keyed to \${LLAMA_CPP_COMMIT}"
else
    fail "the /build/target cache mount lost its \${LLAMA_CPP_COMMIT} id - a fork bump would relink a stale libllama.a"
fi

printf '\n2. .dockerignore keeps local build artifacts out of the context\n'
for pattern in '\*\*/\*\.log' '\*\*/\*\.tsbuildinfo' 'apps/frontend/src/docs/' 'apps/frontend/src/brand/' 'apps/frontend/public/brand/' 'llama\.cpp/\.git/' '\*\*/node_modules/'; do
    if grep -qE "^${pattern}$" "$dockerignore"; then
        pass "$(printf '%s' "$pattern" | sed 's/\\//g')"
    else
        fail "$(printf '%s' "$pattern" | sed 's/\\//g') is not excluded"
    fi
done

# ── Tier 2: real BuildKit cache behaviour ─────────────────────────────────────
printf '\n3. BuildKit cache behaviour\n'

if ! command -v docker >/dev/null 2>&1 || ! docker version --format '{{.Server.Version}}' >/dev/null 2>&1; then
    note "skipped: no reachable Docker daemon (run this on a build host to exercise it)"
    printf '\n'
    if [ "$failures" -eq 0 ]; then
        printf 'docker build cache test passed (structure only)\n'
        exit 0
    fi
    printf '%s check(s) failed\n' "$failures"
    exit 1
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/pom-docker-cache.XXXXXX")
trap 'rm -rf "$work"' EXIT
ctx="$work/ctx"
stub="$work/Dockerfile.stub"
mkdir -p "$ctx"

# The stub: same instructions and order, cheap base image, marker bodies. Only
# the builder stage is kept, since that is where the cache graph lives.
{
    printf '# syntax=docker/dockerfile:1\n'
    parse_instructions "$dockerfile" | awk -F'\t' -v s="$builder" -v img="$stub_base_image" '
        function tag_for(body) {
            if (body ~ /apt-get/)               return "apt"
            if (body ~ /rustup/)                return "rustup"
            if (body ~ /npm ci/)                return "npm-ci"
            if (body ~ /npm run build/)         return "ui-build"
            if (body ~ /cargo build/)           return "cargo"
            return "run" (++other)
        }
        $1 != s { next }
        $2 == "FROM" { printf "FROM %s AS %s\n", img, s; next }
        $2 == "RUN" {
            body = $3
            flags = ""
            while (match(body, /^--[^ ]+ /)) {
                flags = flags substr(body, RSTART, RLENGTH)
                body = substr(body, RSTART + RLENGTH)
            }
            printf "RUN %secho POM-STEP:%s\n", flags, tag_for(body)
            next
        }
        { printf "%s %s\n", $2, $3 }
    '
} > "$stub"

cp "$dockerignore" "$ctx/.dockerignore"

# Every path the builder stage copies has to exist in the synthetic context.
printf '%s\n' "$instructions" | awk -F'\t' '
    $1 == "COPY" {
        line = $2
        if (line ~ /--from=/) next
        sub(/^(--[^ ]+ )+/, "", line)
        n = split(line, tok, " ")
        for (i = 1; i < n; i++) print tok[i]
    }' | sort -u | while read -r src; do
    case "$src" in
        .|./) continue ;;
        */) mkdir -p "$ctx/$src" && printf 'stub\n' > "$ctx/${src}.stub" ;;
        *)  mkdir -p "$ctx/$(dirname "$src")" && printf 'stub\n' > "$ctx/$src" ;;
    esac
done

# The three components, each under the directory its own layer copies. The
# contents carry a per-run id: the daemon's build cache outlives this script,
# and a fixed "v1"/"v2" pair would be answered from the previous run's layers,
# which reads as "nothing rebuilt" no matter what the Dockerfile says.
run_id="$(date +%s)-$$"
mkdir -p "$ctx/llama.cpp/ggml/src" "$ctx/crates/node/src" "$ctx/apps/frontend/src"
seed() { printf '%s %s\n' "$2" "$run_id" > "$ctx/$1"; }
seed llama.cpp/ggml/src/probe.c 'v1'
seed crates/node/src/probe.rs 'v1'
seed apps/frontend/src/probe.tsx 'v1'

build_commit=abc1234
llama_commit=def5678

# Prints the layer groups whose steps did not all come back CACHED, sorted.
# `--progress=plain` names every step (`#7 [builder 5/14] COPY crates/ crates/`)
# and marks the reused ones (`#7 CACHED`), which is the only report of what the
# cache actually did.
rebuilt_groups() {
    docker build --progress=plain --target "$builder" \
        --build-arg "POM_BUILD_COMMIT=$build_commit" \
        --build-arg "LLAMA_CPP_COMMIT=$llama_commit" \
        -f "$stub" -t pom-cache-test:probe "$ctx" 2>&1 \
    | awk -v stage="$builder" -v debug="${POM_CACHE_TEST_DEBUG:-}" '
        # A stubbed RUN carries its own tag; a COPY is placed by what it copies.
        function group(instr) {
            if (instr ~ /POM-STEP:cargo/)                              return "backend"
            if (instr ~ /POM-STEP:ui-build/)                           return "ui"
            if (instr ~ /POM-STEP:npm-ci/)                             return "npm-deps"
            if (instr ~ /POM-STEP:/)                                   return "base"
            if (instr ~ /apps\/frontend\/package/)                     return "npm-deps"
            if (instr ~ /llama\.cpp\// || instr ~ /^COPY vendor\//)    return "llama"
            if (instr ~ /^COPY apps\/frontend\/ / || instr ~ /^COPY docs\// \
                || instr ~ /^COPY AGENTS\.md/ || instr ~ /scripts\/AGENTS\.md/) return "ui"
            if (instr ~ /^COPY crates\// || instr ~ /^COPY Cargo\./ \
                || instr ~ /^COPY \.cargo\// || instr ~ /^COPY assets\//) return "backend"
            return "base"
        }
        match($0, /^#[0-9]+ \[/ ) {
            id = substr($1, 2)
            if ($0 !~ ("\\[" stage " ")) next
            instr = $0
            sub(/^#[0-9]+ \[[^]]*\] /, "", instr)
            # The image-resolve step is not a cacheable layer and never reports
            # CACHED; counting it would make every scenario look dirty.
            if (instr ~ /^FROM /) next
            step[id] = instr
            next
        }
        /^#[0-9]+ CACHED$/ { cached[substr($1, 2)] = 1; next }
        END {
            for (id in step) {
                if (debug) printf "      #%s %-8s %s %s\n", id, group(step[id]), \
                    (id in cached ? "CACHED" : "RAN   "), step[id] > "/dev/stderr"
                if (!(id in cached)) dirty[group(step[id])] = 1
            }
            n = 0
            for (g in dirty) out[n++] = g
            for (i = 0; i < n; i++)
                for (j = i + 1; j < n; j++)
                    if (out[j] < out[i]) { t = out[i]; out[i] = out[j]; out[j] = t }
            line = ""
            for (i = 0; i < n; i++) line = line (line == "" ? "" : " ") out[i]
            print line
        }'
}

scenario() { # scenario <name> <expected groups>
    local name=$1 expected=$2 got
    got=$(rebuilt_groups)
    if [ "$got" = "$expected" ]; then
        pass "$name -> rebuilt: ${got:-<nothing>}"
    else
        fail "$name -> rebuilt: ${got:-<nothing>} (expected: ${expected:-<nothing>})"
    fi
}

if ! docker build --progress=plain --target "$builder" \
    --build-arg "POM_BUILD_COMMIT=$build_commit" \
    --build-arg "LLAMA_CPP_COMMIT=$llama_commit" \
    -f "$stub" -t pom-cache-test:probe "$ctx" >"$work/cold.log" 2>&1; then
    fail "the derived stub does not build - see $work/cold.log"
    tail -n 5 "$work/cold.log" || true
    exit 1
fi

scenario "nothing changed" ""

# The reproduced defect: a node started from the checkout writes a log next to
# its crate, `npm run build` leaves a tsbuildinfo and a generated src/docs, and
# a fetch in the fork rewrites its .git. None of it is an input of any build.
mkdir -p "$ctx/apps/frontend/src/docs" "$ctx/llama.cpp/.git"
seed crates/node/infer_server_4242.log 'runtime log'
seed apps/frontend/tsconfig.app.tsbuildinfo '{"stale":true}'
seed apps/frontend/src/docs/AGENTS.md '# generated'
seed llama.cpp/.git/HEAD 'ref: refs/heads/pom-tensor-parallel'
scenario "only local build artifacts appeared" ""

seed apps/frontend/src/probe.tsx 'v2'
scenario "apps/frontend changed" "ui"

seed crates/node/src/probe.rs 'v2'
scenario "apps/frontend and the backend changed" "backend ui"

seed llama.cpp/ggml/src/probe.c 'v2'
scenario "apps/frontend, the backend and the fork changed" "backend llama ui"

llama_commit=0bad1dea
scenario "the fork's commit changed (new target cache id)" "backend ui"

docker rmi -f pom-cache-test:probe >/dev/null 2>&1 || true

printf '\n'
if [ "$failures" -eq 0 ]; then
    printf 'docker build cache test passed\n'
    exit 0
fi
printf '%s check(s) failed\n' "$failures"
exit 1
