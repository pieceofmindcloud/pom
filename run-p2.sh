#!/usr/bin/env bash
# Peer 2 - ksia (Ubuntu 24.04 + RTX 5060 Ti / CUDA): shard camadas 16-31
#
# As camadas 16-31 saem do nome do arquivo (`..._16_31.gguf`).
# BOOTSTRAP aponta para o peer 1 (MacBook) em <ip>:<porta-udp>.
#
# Pre-requisitos e procedimento completo: ver CLAUDE.md, secao
# "Subindo o cluster de dois peers".

set -euo pipefail
cd "$(dirname "$0")"

MODEL="${MODEL:-./models/qwen/qwen_16_31.gguf}"
BOOTSTRAP="${BOOTSTRAP:-192.168.100.240:42777}"
# 8080 esta ocupada pelo code-server no ksia; o peer anuncia a porta que usa,
# entao o painel de qualquer maquina encontra este no sozinho.
GATEWAY_PORT="${GATEWAY_PORT:-8090}"

exec ./target/release/rdma-p2p \
    --model "$MODEL" \
    --port 42777 \
    --bootstrap "$BOOTSTRAP" \
    --gateway-port "$GATEWAY_PORT" \
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
