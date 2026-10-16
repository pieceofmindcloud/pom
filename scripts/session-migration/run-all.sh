#!/usr/bin/env bash
# Runs the three steps in order and stops at the first failure. Every step appends to runs/<run>/test.log.
# Run:  ./run-all.sh
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/lib.sh"
log "run" "session migration test: A=$NODE_A ($GATEWAY_A) B=$NODE_B ($GATEWAY_B) session=$SESSION_KEY"
for step in step1-save-on-head-a.sh step2-turn-on-head-b.sh step3-check.sh; do
  log "run" "== $step =="
  "$HERE/$step"
done
