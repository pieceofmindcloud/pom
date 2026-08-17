#!/usr/bin/env bash
# request.sh — Envia uma requisicao POST /api/generate para o gateway do peer 1.
#
# Uso:
#   ./request.sh "seu prompt"                 # usa os defaults abaixo
#   ./request.sh "seu prompt" 192.168.100.138 # outro host de gateway
#   ./request.sh "seu prompt" ksia 8080       # host + porta
#   GATEWAY=127.0.0.1 PORT=8080 ./request.sh "seu prompt"
#
# Defaults: o peer 1 (MacBook) e quem roda o gateway HTTP.
# Qualquer no serve a API, entao GATEWAY pode apontar para o peer 2 tambem.

set -uo pipefail

PROMPT="${1:-Explique em uma frase o que e inferencia distribuida.}"
GATEWAY="${2:-${GATEWAY:-192.168.100.240}}"
PORT="${3:-${PORT:-8080}}"

curl -s "http://$GATEWAY:$PORT/api/generate" \
  -H "Content-Type: application/json" \
  -d "{\"prompt\": $(printf '%s' "$PROMPT" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')}" \
  | python3 -m json.tool
