#!/usr/bin/env bash
# Node 1: layers 0-15 + gateway HTTP :8080
# NOTA: sem split_model.py, cada peer carrega o modelo completo mas anuncia
# apenas suas layers para o P2P. Para inferência distribuída real, use split_model.py.

MODEL="./models/Qwen3.5-9B-DeepSeek-V4-Flash-Q4_K_S.gguf"
MODEL="./models/qwen/qwen_0_15.gguf"

./target/release/rdma-p2p \
    --model "$MODEL" \
    --memory 1000MB \
    --port 42777 \
    --gateway \
    --gateway-port 8080 \
    --inference-port 8180 \
    --n-gpu-layers 99 \
    --n-ctx 4096 \
    --max-new-tokens 512 \
    --temperature 0.7 \
    --top-p 0.9 \
    --top-k 40 \
    --repeat-penalty 1.15 \
    --no-thinking
