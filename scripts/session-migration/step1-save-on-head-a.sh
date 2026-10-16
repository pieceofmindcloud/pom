#!/usr/bin/env bash
# Step 1: a conversation starts on head A. Its KV session is saved on A's disk when the turn ends.
# Run:  ./step1-save-on-head-a.sh
source "$(dirname "$0")/lib.sh"
STEP=step1
need API_KEY; need MODEL; need NODE_A; need GATEWAY_A; need SESSION_DIR_A
log "$STEP" "conversation $SESSION_KEY starts on head A ($NODE_A) through $GATEWAY_A"
save_state SESSION_KEY "$SESSION_KEY"

# a prompt long enough that its K/V is worth moving (about 2-3k tokens)
LONG="$(for _ in $(seq 1 400); do printf 'The quick brown fox jumps over the lazy dog. '; done)"
MESSAGES="$(jq -n --arg text "$LONG Reply with the single word OK." '[{role:"user", content:$text}]')"

offset_session="$(log_size "$SESSION_LOG_A")"
offset_node="$(log_size "$NODE_LOG_A")"
answer="$(chat "$GATEWAY_A" "$NODE_A" "$MESSAGES")"
prompt_tokens="$(jq -r '.usage.prompt_tokens // 0' <<<"$answer")"
assistant="$(jq -r '.choices[0].message.content // ""' <<<"$answer")"
check "$STEP" "turn answered" "$([ "$prompt_tokens" -gt 0 ] && echo 0 || echo 1)" "prompt $prompt_tokens tokens, answer '$assistant'"
save_state FIRST_MESSAGES "$MESSAGES"
save_state ASSISTANT_1 "$assistant"
save_state PROMPT_1_TOKENS "$prompt_tokens"

# the save reaches the disk when the engine is idle (the bridge writes it, the turn's answer does not wait)
log "$STEP" "waiting up to ${SAVE_WAIT_S}s for the session file on A's disk ..."
found=""
for _ in $(seq 1 "$SAVE_WAIT_S"); do
  if [ -f "$SESSION_DIR_A/$SESSION_KEY.ids.json" ] && [ -f "$SESSION_DIR_A/$SESSION_KEY.strsess" ]; then
    found="$SESSION_KEY.ids.json"
    break
  fi
  sleep 1
done
if [ -n "$found" ]; then
  saved_tokens="$(jq 'length' "$SESSION_DIR_A/$found")"
else
  saved_tokens=0
fi
check "$STEP" "session saved on A's disk" "$([ "$saved_tokens" -gt 0 ] && echo 0 || echo 1)" \
  "$saved_tokens tokens in $SESSION_DIR_A/${found:-<none>}"
save_state SAVED_TOKENS "$saved_tokens"

# what A's logs say about the save (the bridge's and the POM node's lines of this step)
log "$STEP" "A's bridge log for this step:"
log_since "$SESSION_LOG_A" "$offset_session" | grep -F "$SESSION_KEY" | sed 's/^/    /' | tee -a "$RUN_DIR/test.log" || true
log "$STEP" "A's node log for this step:"
log_since "$NODE_LOG_A" "$offset_node" | grep -F "$SESSION_KEY" | sed 's/^/    /' | tee -a "$RUN_DIR/test.log" || true
log "$STEP" "done. Next: ./step2-turn-on-head-b.sh"
