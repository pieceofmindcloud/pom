#!/usr/bin/env bash
# request.sh — Send a /api/generate request to the local dev gateway.
# Usage: ./request.sh [prompt] [gateway-port]

PROMPT="${1:-Hello.}"
GATEWAY="192.168.100.210"
PORT="${2:-8080}"

curl -s "http://$GATEWAY:$PORT/api/generate" \
  -H "Content-Type: application/json" \
  -d "{\"prompt\": $(printf '%s' "$PROMPT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}" \
  | python3 -m json.tool
