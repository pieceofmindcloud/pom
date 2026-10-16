#!/usr/bin/env bash
# Step 3: reads the outcome of steps 1 and 2 and says whether the session moved from A to B through the disk, or
# whether B read the prompt again. Exits 0 only when the session was migrated and reused.
# Run:  ./step3-check.sh
source "$(dirname "$0")/lib.sh"
STEP=step3
need NODE_A; need NODE_B
load_state
failures=0

# 1. the session existed on A's disk after step 1
check "$STEP" "saved on A" "$([ "${SAVED_TOKENS:-0}" -gt 0 ] && echo 0 || echo 1)" "$SAVED_TOKENS tokens saved in step 1" || failures=$((failures+1))

# 2. B fetched it from A (the pull decided to fetch, or the test forced it)
fetched="$(grep -F "fetched from a peer" "$RUN_DIR/step2-logs.txt" | grep -c "$NODE_A" || true)"
check "$STEP" "B fetched the session from A" "$([ "$fetched" -gt 0 ] && echo 0 || echo 1)" \
  "$([ "$fetched" -gt 0 ] && echo "node log: fetched from $NODE_A" || echo "no 'fetched from a peer' line naming $NODE_A; see step2-logs.txt")" \
  || failures=$((failures+1))

# 3. B restored the saved start from its own disk into a slot
restored="$(grep -c "restored from disk" "$RUN_DIR/step2-logs.txt" || true)"
check "$STEP" "B restored it from its disk" "$([ "$restored" -gt 0 ] && echo 0 || echo 1)" \
  "$([ "$restored" -gt 0 ] && echo "bridge log: restored from disk" || echo "no 'restored from disk' line in step2-logs.txt")" \
  || failures=$((failures+1))

# 4. the engine did not read those tokens again (they came back as reused)
threshold=$(( ${SAVED_TOKENS:-0} * 9 / 10 ))
check "$STEP" "the saved tokens were reused" "$([ "${CACHED_TOKENS:-0}" -ge "$threshold" ] && [ "$threshold" -gt 0 ] && echo 0 || echo 1)" \
  "$CACHED_TOKENS reused, at least $threshold expected (90% of $SAVED_TOKENS saved)" || failures=$((failures+1))

echo
if [ "$failures" -eq 0 ]; then
  log "$STEP" "RESULT: MIGRATED - the session went from A's disk to B and the engine did not read it again"
  exit 0
fi
if [ "${CACHED_TOKENS:-0}" -lt 1 ]; then
  log "$STEP" "RESULT: RE-READ - B read the prompt again; the session did not reach its slot"
else
  log "$STEP" "RESULT: PARTIAL - $failures check(s) failed; the lines are in $RUN_DIR/step2-logs.txt"
fi
exit 1
