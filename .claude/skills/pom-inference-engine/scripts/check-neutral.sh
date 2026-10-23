#!/usr/bin/env bash
# Checks that an engine feature branch can be proposed to the official
# repository: its commits (messages, authors' trailers aside) and its diff
# against the official base name nothing of the POM, its plugins, its people
# or its machines.
#
#   bash check-neutral.sh <branch> [--base upstream/main]
#
# Extra terms (machine names, people, internal hosts) come from
# $NEUTRAL_EXTRA_TERMS (an extended regex) so they never live in this file.
set -euo pipefail

branch="${1:?usage: check-neutral.sh <branch> [--base <ref>]}"
shift
base=""
while (($# > 0)); do
  case "$1" in
    --base) base="${2:?}"; shift 2 ;;
    *) echo "check-neutral: unknown argument $1" >&2; exit 2 ;;
  esac
done
if [[ -z "$base" ]]; then
  remote="$(git remote get-url upstream >/dev/null 2>&1 && echo upstream || echo origin)"
  base="$remote/$(git remote show "$remote" | sed -n 's/.*HEAD branch: //p')"
fi

terms='(^|[^A-Za-z])(pom|POM|Pom)([^A-Za-z]|$)|pom[-_.](plugin|node|jair|stages|cluster)|piece ?of ?mind|pieceofmind|/pom/|POM_|plugin-host|freetoken|(/home|/Users|/storage)/[A-Za-z0-9_.-]+'
if [[ -n "${NEUTRAL_EXTRA_TERMS:-}" ]]; then
  terms="$terms|$NEUTRAL_EXTRA_TERMS"
fi

merge_base="$(git merge-base "$base" "$branch")"
status=0
messages="$(git log --format='%H%n%B' "$merge_base..$branch" | grep -v -E '^(Co-Authored-By|Signed-off-by|Assisted-by):' || true)"
if hits="$(printf '%s\n' "$messages" | grep -n -E "$terms")"; then
  printf 'check-neutral: commit messages name the integration:\n%s\n' "$hits" >&2
  status=1
fi
if hits="$(git diff "$merge_base" "$branch" | grep -E '^\+' | grep -v '^+++' | grep -n -E "$terms")"; then
  printf 'check-neutral: the diff names the integration:\n%s\n' "$hits" | head -40 >&2
  status=1
fi
if git diff --name-only "$merge_base" "$branch" | grep -n -E "$terms" >&2; then
  printf 'check-neutral: file names above name the integration\n' >&2
  status=1
fi
if [[ "$(git rev-list --count "$merge_base..$branch")" == 0 ]]; then
  printf 'check-neutral: %s has no commit over %s\n' "$branch" "$base" >&2
  status=1
fi
if git log --merges --format=%h "$merge_base..$branch" | grep -q .; then
  printf 'check-neutral: %s contains merge commits; rebase it on %s\n' "$branch" "$base" >&2
  status=1
fi
((status == 0)) && printf 'check-neutral: %s is neutral against %s (%s commits)\n' \
  "$branch" "$base" "$(git rev-list --count "$merge_base..$branch")"
exit "$status"
