#!/usr/bin/env bash
# Peer 1 - MacBook (Apple Silicon / Metal): shard camadas 0-15 + gateway HTTP :8080
#
# As camadas 0-15 saem do nome do arquivo (`..._0_15.gguf`), e o binario Rust
# sobe o inference_server.py sozinho usando ./venv/bin/python3 na --inference-port.
#
# Pre-requisitos e procedimento completo: ver CLAUDE.md, secao
# "Subindo o cluster de dois peers".

set -euo pipefail
cd "$(dirname "$0")"

MODEL="${MODEL:-./models/qwen/qwen_0_15.gguf}"

exec ./target/release/rdma-p2p \
    --model "$MODEL" \
    --port 42777 \
    --gateway \
    --gateway-port 8080 \
    --inference-port 8181 \
    --n-gpu-layers 99 \
    --n-ctx "${N_CTX:-8192}" \
    --n-batch "${N_BATCH:-512}" \
    --max-new-tokens "${MAX_NEW_TOKENS:-256}" \
    --temperature 0.7 \
    --top-p 0.9 \
    --top-k 40 \
    --repeat-penalty 1.15 \
    --no-thinking
