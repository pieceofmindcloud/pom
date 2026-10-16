#!/usr/bin/env bash
# Step 1: a conversation starts on head A. Its KV session is saved on A's disk when the turn ends.
# Run:  ./step1-save-on-head-a.sh
source "$(dirname "$0")/lib.sh"
STEP=step1
need API_KEY; need MODEL; need NODE_A; need GATEWAY_URL; need SESSION_DIR; need HEAD_A_CONTAINER
log "$STEP" "conversation $SESSION_KEY starts on head A ($NODE_A, container $HEAD_A_CONTAINER)"
save_state SESSION_KEY "$SESSION_KEY"

# a prompt above the bridge MIN_SESSION_TOKENS (8000): a shorter one is never saved (about 12k tokens)
LONG="$(for _ in $(seq 1 1200); do printf 'The quick brown fox jumps over the lazy dog. '; done)"
MESSAGES="$(jq -n --arg text "$LONG Reply with the single word OK." '[{role:"user", content:$text}]')"

mark_session="$(mark A session)"
mark_node="$(mark A node)"
answer="$(chat LOCAL "$NODE_A" "$MESSAGES")"
prompt_tokens="$(jq -r '.usage.prompt_tokens // 0' <<<"$answer")"
assistant="$(jq -r '.choices[0].message.content // ""' <<<"$answer")"
check "$STEP" "turn answered" "$([ "$prompt_tokens" -gt 0 ] && echo 0 || echo 1)" "prompt $prompt_tokens tokens, answer '$assistant'"
save_state FIRST_MESSAGES "$MESSAGES"
save_state ASSISTANT_1 "$assistant"

# the save reaches A's disk when the engine is idle (the bridge writes it, the turn's answer does not wait)
log "$STEP" "waiting up to ${SAVE_WAIT_S}s for the session file on A's disk ..."
saved_tokens=0
for _ in $(seq 1 "$SAVE_WAIT_S"); do
  if at A "test -f $SESSION_DIR/$SESSION_KEY.ids.json && test -f $SESSION_DIR/$SESSION_KEY.strsess" 2>/dev/null; then
    saved_tokens="$(at A "cat $SESSION_DIR/$SESSION_KEY.ids.json" | jq 'length')"
    break
  fi
  sleep 1
done
check "$STEP" "session saved on A's disk" "$([ "$saved_tokens" -gt 0 ] && echo 0 || echo 1)" \
  "$saved_tokens tokens in $SESSION_DIR/$SESSION_KEY.ids.json (A)" || exit 1
save_state SAVED_TOKENS "$saved_tokens"
save_state PROMPT_1_TOKENS "$prompt_tokens"

log "$STEP" "A's logs for this step:"
since A session "$mark_session" | grep -F "$SESSION_KEY" | sed 's/^/    /' | tee -a "$RUN_DIR/test.log" || true
since A node "$mark_node" | grep -F "$SESSION_KEY" | sed 's/^/    /' | tee -a "$RUN_DIR/test.log" || true
log "$STEP" "done. Next: ./step2-turn-on-head-b.sh"
