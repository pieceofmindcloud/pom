#!/usr/bin/env bash
# Step 2: the same conversation's next turn goes to head B (the receiving gateway is B, the head is B). B does not hold
# the session in its slot. It must either fetch A's session and restore it from its disk, or read the prompt again.
# Run:  ./step2-turn-on-head-b.sh   (after step 1)
source "$(dirname "$0")/lib.sh"
STEP=step2
need API_KEY; need MODEL; need NODE_B; need GATEWAY_B; need NODE_A
load_state
log "$STEP" "turn 2 of $SESSION_KEY goes to head B ($NODE_B) through $GATEWAY_B; the session was saved on A"

MESSAGES="$(jq -n --argjson first "$FIRST_MESSAGES" --arg answer "$ASSISTANT_1" \
  '$first + [{role:"assistant", content:$answer}, {role:"user", content:"Reply with the single word DONE."}]')"

offset_session="$(log_size "$SESSION_LOG_B")"
offset_node="$(log_size "$NODE_LOG_B")"
answer="$(chat "$GATEWAY_B" "$NODE_B" "$MESSAGES")"
prompt_tokens="$(jq -r '.usage.prompt_tokens // 0' <<<"$answer")"
cached="$(jq -r '.usage.prompt_tokens_details.cached_tokens // 0' <<<"$answer")"
log "$STEP" "turn answered: prompt $prompt_tokens tokens, $cached of them reused (not read again by the engine)"
save_state PROMPT_2_TOKENS "$prompt_tokens"
save_state CACHED_TOKENS "$cached"

session_lines="$(log_since "$SESSION_LOG_B" "$offset_session" | grep -F "$SESSION_KEY" || true)"
node_lines="$(log_since "$NODE_LOG_B" "$offset_node" | grep -F "$SESSION_KEY" || true)"
{
  echo "== B's node log (session pull) =="
  grep -F "session pull" <<<"$node_lines" || echo "(no session pull line for this conversation)"
  echo "== B's bridge log (restore) =="
  echo "$session_lines" | grep -E "restored from disk|read again|not restored" || echo "(no restore line for this conversation)"
} | tee "$RUN_DIR/step2-logs.txt" | sed 's/^/    /' | tee -a "$RUN_DIR/test.log"
log "$STEP" "done. Next: ./step3-check.sh"
