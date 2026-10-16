#!/usr/bin/env bash
# Shared helpers of the session migration test. Sourced by the steps; never run alone.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
[ -f "$HERE/config.local.env" ] && source "$HERE/config.local.env" || source "$HERE/config.env"
RUN_DIR="${RUN_DIR:-$HERE/runs/latest}"
mkdir -p "$RUN_DIR"
STATE="$RUN_DIR/state.env"

log() {  # log <step> <message>: one line on the terminal and in the run's test.log
  local line
  line="$(date '+%H:%M:%S') [$1] $2"
  echo "$line"
  echo "$line" >> "$RUN_DIR/test.log"
}

need() {  # need <variable>: fails when a setting is empty
  if [ -z "${!1:-}" ]; then
    log "config" "missing setting $1 (edit config.local.env)"
    exit 2
  fi
}

save_state() {  # save_state NAME VALUE: keeps a value for the next steps
  local name="$1" value="$2"
  touch "$STATE"
  grep -v "^$name=" "$STATE" > "$STATE.tmp" || true
  printf '%s=%q\n' "$name" "$value" >> "$STATE.tmp"
  mv "$STATE.tmp" "$STATE"
}

load_state() {
  [ -f "$STATE" ] || { log "config" "no state yet: run the earlier steps first"; exit 2; }
  # shellcheck source=/dev/null
  source "$STATE"
}

log_size() {  # log_size PATH: the byte offset of a log now, so a step reads only what it writes
  local path="$1"
  case "$path" in
    ssh:*) local rest="${path#ssh:}"; ssh "${rest%%:*}" "wc -c < ${rest#*:}" 2>/dev/null || echo 0 ;;
    "") echo 0 ;;
    *) wc -c < "$path" 2>/dev/null || echo 0 ;;
  esac
}

log_since() {  # log_since PATH OFFSET: the lines a log got after OFFSET
  local path="$1" offset="$2"
  case "$path" in
    ssh:*) local rest="${path#ssh:}"; ssh "${rest%%:*}" "tail -c +$((offset + 1)) ${rest#*:}" 2>/dev/null || true ;;
    "") ;;
    *) tail -c +$((offset + 1)) "$path" 2>/dev/null || true ;;
  esac
}

chat() {  # chat HEAD_NODE_ID JSON_MESSAGES: one blocking turn pinned to a head; prints the answer JSON
  local gateway="$1" head="$2" messages="$3" body
  body="$(jq -n --arg model "$MODEL" --arg session "$SESSION_KEY" --arg head "$head" --argjson messages "$messages" \
    '{model: $model, messages: $messages, session: $session, inference_head: $head, max_tokens: 64, stream: false}')"
  curl -sS --max-time 900 "$gateway/v1/chat/completions" \
    -H "authorization: Bearer $API_KEY" -H "content-type: application/json" -d "$body"
}

check() {  # check STEP NAME CONDITION_RESULT(0|1) DETAIL: records one verdict line
  local step="$1" name="$2" ok="$3" detail="$4"
  if [ "$ok" = "0" ]; then
    log "$step" "PASS $name - $detail"
  else
    log "$step" "FAIL $name - $detail"
    return 1
  fi
}
