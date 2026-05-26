#!/usr/bin/env bash
# request.sh — Send a /api/generate request to the local dev gateway.
# Usage: ./request.sh [prompt] [gateway-port]

PROMPT="
<system-reminder>
As you answer the user's questions, you can use the following context:
## important-instruction-reminders
Do what has been asked; nothing more, nothing less.
NEVER create files unless they're absolutely necessary for achieving your goal.
ALWAYS prefer editing an existing file to creating a new one.
NEVER proactively create documentation files (*.md) or README files. Only create documentation files if explicitly requested by the User.

</system-reminder>

$1
"
GATEWAY="127.0.0.1"
PORT="${2:-8080}"

curl -s "http://$GATEWAY:$PORT/api/generate" \
  -H "Content-Type: application/json" \
  -d "{\"prompt\": $(printf '%s' "$PROMPT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}" \
  | python3 -m json.tool
