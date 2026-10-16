#!/usr/bin/env bash
# Step 2: the conversation's next turn is pinned to head B. B does not hold the session in its slot: it must fetch A's
# session (only if B runs the same layer split) and restore it from its own disk, or read the prompt again.
# Run:  ./step2-turn-on-head-b.sh   (after step 1)
source "$(dirname "$0")/lib.sh"
STEP=step2
need API_KEY; need MODEL; need NODE_B; need GATEWAY_URL; need HEAD_B_CONTAINER
load_state
log "$STEP" "turn 2 of $SESSION_KEY goes to head B ($NODE_B, container $HEAD_B_CONTAINER) through $GATEWAY_URL"

MESSAGES="$(jq -n --argjson first "$FIRST_MESSAGES" --arg answer "$ASSISTANT_1" \
  '$first + [{role:"assistant", content:$answer}, {role:"user", content:"Reply with the single word DONE."}]')"

mark_session="$(mark B session)"
mark_node="$(mark B node)"
answer="$(chat LOCAL "$NODE_B" "$MESSAGES")"
prompt_tokens="$(jq -r '.usage.prompt_tokens // 0' <<<"$answer")"
cached="$(jq -r '.usage.prompt_tokens_details.cached_tokens // 0' <<<"$answer")"
log "$STEP" "turn answered: prompt $prompt_tokens tokens, $cached of them reused (not read again by the engine)"
save_state PROMPT_2_TOKENS "$prompt_tokens"
save_state CACHED_TOKENS "$cached"

{
  echo "== B's node log (session pull) =="
  since B node "$mark_node" | grep -F "$SESSION_KEY" | grep -F "session pull" || echo "(no session pull line for this conversation)"
  echo "== B's Strata log (restore) =="
  since B session "$mark_session" | grep -F "$SESSION_KEY" | grep -E "restored from disk|read again|not restored|none of" || echo "(no restore line for this conversation)"
} | tee "$RUN_DIR/step2-logs.txt" | sed 's/^/    /' | tee -a "$RUN_DIR/test.log"
log "$STEP" "done. Next: ./step3-check.sh"
