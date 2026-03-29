#!/usr/bin/env bash
# request.sh — Send a /api/generate request to the local dev gateway.
# Usage: ./request.sh [prompt] [gateway-port]

PROMPT="${1:-Tell me a fun fact about distributed systems.}"
PORT="${2:-8080}"

curl -s "http://localhost:$PORT/api/generate" \
  -H "Content-Type: application/json" \
  -d "{\"prompt\": $(printf '%s' "$PROMPT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}" \
  | python3 -m json.tool
