#!/usr/bin/env bash
# run.sh — Generate and optionally start local cluster scripts.
# Usage: ./run.sh

set -euo pipefail

BIN="./target/release/pom"

# ── Preflight ─────────────────────────────────────────────────────────────────
echo "Building pom..."
cargo build --release

# ── Create run-p1.sh (Gateway + Peer 1) ───────────────────────────────────────
echo "Creating run-p1.sh (Gateway + Layers 0-20)..."
cat <<EOF > run-p1.sh
#!/usr/bin/env bash
$BIN \\
  --model ./models/gemma2_0_20.gguf \\
  --memory 4GB \\
  --port 42777 \\
  --gateway \\
  --gateway-port 8080 \\
  --inference-port 8181
EOF
chmod +x run-p1.sh

# ── Create run-p2.sh (Peer 2) ─────────────────────────────────────────────────
echo "Creating run-p2.sh (Layers 21-41)..."
cat <<EOF > run-p2.sh
#!/usr/bin/env bash
$BIN \\
  --model ./models/gemma2_21_41.gguf \\
  --memory 4GB \\
  --port 42778 \\
  --bootstrap 127.0.0.1:42777 \\
  --inference-port 8182
EOF
chmod +x run-p2.sh

echo ""
echo "Scripts created:"
echo "  ./run-p1.sh  (Gateway, Layers 0-20, Port 42777, Infer 8181)"
echo "  ./run-p2.sh  (Peer, Layers 21-41, Port 42778, Infer 8182)"
echo ""
echo "You can now run them in separate terminals."
