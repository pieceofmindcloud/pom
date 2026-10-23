#!/usr/bin/env bash
# First step for a new inference engine: clone the engine fork next to the
# other POM repositories, name its remotes, create the `pom` integration
# branch from the official default branch, and print what the engine is made
# of, so the assessment starts from facts.
#
#   bash engine-intake.sh <github URL of the fork> [--workspace <dir>]
#
# - origin   = the fork (where every branch is pushed)
# - upstream = the official repository (the fork's GitHub parent); read only
# - pom      = integration branch: upstream default + every feature branch
# Nothing is pushed and no existing branch is moved.
set -euo pipefail

url="" workspace=""
while (($# > 0)); do
  case "$1" in
    --workspace) workspace="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) [[ -z "$url" ]] || { echo "engine-intake: unexpected argument $1" >&2; exit 2; }; url="$1"; shift ;;
  esac
done
[[ -n "$url" ]] || { sed -n '2,13p' "$0" >&2; exit 2; }
if [[ -z "$workspace" ]]; then
  # The skill lives in <workspace>/<pom repo>/.claude/skills/pom-inference-engine.
  workspace="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../../.." && pwd -P)"
fi

slug="$(printf '%s' "$url" | sed -E 's#^(https://github.com/|git@github.com:)##; s#\.git$##; s#/$##')"
[[ "$slug" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || { echo "engine-intake: not a GitHub repository URL: $url" >&2; exit 2; }
repo="${slug#*/}"
dest="$workspace/$repo"

parent="$(gh repo view "$slug" --json parent --jq '.parent | select(. != null) | .owner.login + "/" + .name' 2>/dev/null || true)"
if [[ -z "$parent" ]]; then
  echo "engine-intake: $slug has no GitHub parent; give the official repository by adding it as 'upstream' by hand" >&2
fi

if [[ ! -d "$dest/.git" ]]; then
  git clone "git@github.com:$slug.git" "$dest"
fi
cd "$dest"
if [[ -n "$parent" ]] && ! git remote get-url upstream >/dev/null 2>&1; then
  git remote add upstream "https://github.com/$parent.git"
fi
git fetch --quiet origin
if git remote get-url upstream >/dev/null 2>&1; then
  git fetch --quiet upstream
  base_remote=upstream
else
  base_remote=origin
fi
default="$(git remote show "$base_remote" | sed -n 's/.*HEAD branch: //p')"
base="$base_remote/$default"
if git show-ref --verify --quiet refs/remotes/origin/pom; then
  git show-ref --verify --quiet refs/heads/pom || git branch --track pom origin/pom
elif ! git show-ref --verify --quiet refs/heads/pom; then
  git branch --no-track pom "$base"
fi

printf '\n== %s\n' "$dest"
printf 'origin    %s\nupstream  %s\nbase      %s (%s)\npom       %s\n' \
  "$(git remote get-url origin)" "$(git remote get-url upstream 2>/dev/null || echo none)" \
  "$base" "$(git rev-parse --short "$base")" "$(git rev-parse --short pom)"
printf '\n== languages (files)\n'
git ls-files | sed -nE 's/.*\.([A-Za-z0-9+]+)$/\1/p' | sort | uniq -c | sort -rn | head -12
printf '\n== build systems\n'
git ls-files | grep -E '(^|/)(CMakeLists\.txt|Makefile|setup\.py|pyproject\.toml|Cargo\.toml|package\.json|build\.zig|meson\.build|Package\.swift)$' | head -20
printf '\n== license\n'
git ls-files | grep -iE '^(LICENSE|COPYING)' | head -3
printf '\n== server and API surface (OpenAI-compatible routes, health)\n'
git grep -n -I -E '/v1/(chat/completions|completions|models)|"/health"|/health\b' -- ':!*.md' ':!*test*' ':!*bench*' | head -20 || true
printf '\n== model formats\n'
git grep -l -I -i -E 'gguf|safetensors|mlx' -- ':!*.md' ':!*test*' ':!*bench*' | head -10 || true
printf '\n== GPU backends\n'
git grep -l -I -E 'cuda|CUDA|Metal|MTL|hip|ROCm|vulkan|Vulkan' -- ':!*.md' | sed -E 's#/[^/]*$##' | sort | uniq -c | sort -rn | head -10 || true
printf '\n== sessions, prompt cache, speculative, layer split (feature-levels.md)\n'
git grep -l -I -i -E 'prompt.?cache|prefix.?cache|kv.?cache.*(save|restore|dump|load)|speculative|draft|mtp|layer.?split|pipeline' -- ':!*.md' | head -15 || true
printf '\nnext: assess the engine against feature-levels.md, then scaffold the plugin (SKILL.md, step 3)\n'
