#!/usr/bin/env bash
# Shared helpers of the session migration test. Sourced by the steps; never run alone.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=/dev/null
source "$HERE/config.env"
[ -f "$HERE/config.local.env" ] && source "$HERE/config.local.env"
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

# Where a head runs: HEAD_<A|B>_SSH is the ssh command that reaches its host (empty: this machine) and
# HEAD_<A|B>_CONTAINER the container the POM node and its engine run in (empty: the host itself).
at() {  # at LOC COMMAND: runs a shell command where the head's files and engine are (container when set)
  local loc="$1"; shift
  local ssh_var="HEAD_${loc}_SSH" cont_var="HEAD_${loc}_CONTAINER"
  local ssh_cmd="${!ssh_var:-}" cont="${!cont_var:-}"
  if [ -n "$cont" ]; then
    if [ -n "$ssh_cmd" ]; then
      $ssh_cmd "docker exec -i $cont sh -c $(printf %q "$*")"
    else
      docker exec -i "$cont" sh -c "$*"
    fi
  else
    if [ -n "$ssh_cmd" ]; then
      $ssh_cmd "sh -c $(printf %q "$*")"
    else
      sh -c "$*"
    fi
  fi
}

at_host() {  # at_host LOC COMMAND: runs on the head's host, outside its container (for docker logs)
  local loc="$1"; shift
  local ssh_var="HEAD_${loc}_SSH" ssh_cmd
  ssh_cmd="${!ssh_var:-}"
  if [ -n "$ssh_cmd" ]; then $ssh_cmd "sh -c $(printf %q "$*")"; else sh -c "$*"; fi
}

# The two logs a step reads, per head: the POM node's log (its docker logs: the Rust session pull lines) and the
# plugin's Strata server log inside the container (the bridge's restore lines). A mark is a line count for the node's
# log and a byte count for the Strata log; `since` prints what came after a mark.
cont_of() { local v="HEAD_${1}_CONTAINER"; echo "${!v:-}"; }

mark() {  # mark LOC SOURCE(node|session): the position of the log now
  local loc="$1" src="$2"
  if [ "$src" = node ]; then
    at_host "$loc" "docker logs $(cont_of "$loc") 2>&1 | wc -l" | tr -d ' '
  else
    at "$loc" "wc -c < $SESSION_LOG_PATH" | tr -d ' '
  fi
}

since() {  # since LOC SOURCE MARK: the log lines written after the mark
  local loc="$1" src="$2" pos="$3"
  if [ "$src" = node ]; then
    at_host "$loc" "docker logs $(cont_of "$loc") 2>&1 | tail -n +$((pos + 1))"
  else
    at "$loc" "tail -c +$((pos + 1)) $SESSION_LOG_PATH"
  fi
}

chat() {  # chat WHERE HEAD_NODE_ID JSON_MESSAGES: WHERE is the place the request leaves from (A, B or LOCAL): one blocking turn pinned to a head; prints the answer JSON
  local loc="$1" head="$2" messages="$3" body
  body="$(jq -n --arg model "$MODEL" --arg session "$SESSION_KEY" --arg head "$head" --argjson messages "$messages" \
    '{model: $model, messages: $messages, session: $session, inference_head: $head, max_tokens: 64, stream: false, thinking_enabled: false}')"
  printf '%s' "$body" | at "$loc" "curl -sS --max-time 900 $GATEWAY_URL/v1/chat/completions -H 'authorization: Bearer $API_KEY' -H 'content-type: application/json' -d @-"
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
