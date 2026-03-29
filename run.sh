#!/usr/bin/env bash
# run.sh — Start a local 2-peer + gateway dev cluster.
# Usage: ./run.sh [model] [gateway-port]
# Press Ctrl+C to stop all nodes.

set -euo pipefail

MODEL="${1:-gemma2}"
GATEWAY_PORT="${2:-8080}"
BIN="./target/release/rdma-p2p"

# ── Preflight ─────────────────────────────────────────────────────────────────
if [[ ! -x "$BIN" ]]; then
  echo "Binary not found — building..."
  cargo build --release
fi

# ── Cleanup on exit ───────────────────────────────────────────────────────────
PIDS=()

cleanup() {
  echo ""
  echo "Stopping all nodes..."
  for pid in "${PIDS[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  echo "All nodes stopped."
}

trap cleanup INT TERM EXIT

# ── Log directory ─────────────────────────────────────────────────────────────
mkdir -p logs

# ── Node 1: peer — layers 0..N (port 42777) ───────────────────────────────────
echo "Starting peer-1  (port 42777, model=$MODEL, memory=4GB)..."
"$BIN" \
  --model "$MODEL" \
  --memory 4GB \
  --port 42777 &
PIDS+=($!)

sleep 1

# ── Node 2: peer — layers N+1..end (port 42778, bootstraps to peer-1) ─────────
echo "Starting peer-2  (port 42778, model=$MODEL, memory=4GB)..."
"$BIN" \
  --model "$MODEL" \
  --memory 4GB \
  --port 42778 \
  --bootstrap "127.0.0.1:42777" &
PIDS+=($!)

sleep 1

# ── Node 3: gateway — HTTP API (port 42779, bootstraps to peer-1) ─────────────
echo "Starting gateway (port 42779, gateway-port=$GATEWAY_PORT)..."
"$BIN" \
  --model "$MODEL" \
  --memory 4GB \
  --port 42779 \
  --bootstrap "127.0.0.1:42777" \
  --gateway \
  --gateway-port "$GATEWAY_PORT" &
PIDS+=($!)

# ── Ready ──────────────────────────────────────────────────────────────────────
echo ""
echo "Cluster running:"
echo "  peer-1   → UDP 42777   (logs/peer1.log)"
echo "  peer-2   → UDP 42778   (logs/peer2.log)"
echo "  gateway  → HTTP :$GATEWAY_PORT  (logs/gateway.log)"
echo ""
echo "  curl -s localhost:$GATEWAY_PORT/health | jq"
echo "  curl -s localhost:$GATEWAY_PORT/peers  | jq"
echo "  ./request.sh \"Hello, world\""
echo ""
echo "Press Ctrl+C to stop all nodes."

# Keep the script alive (wait for all background jobs)
wait
