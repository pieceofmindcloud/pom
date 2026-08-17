#!/usr/bin/env bash
# Node 1: pom auto-starts inference_server.py internamente.
MODEL="./models/Qwen3.5-9B-DeepSeek-V4-Flash-Q4_K_S.gguf"

./target/release/pom \
    --model "$MODEL" \
    --memory 15GB \
    --port 42777 \
    --gateway \
    --gateway-port 8080 \
    --inference-port 8181 \
    --n-gpu-layers 99 \
    --n-ctx 8192 \
    --max-new-tokens 512 \
    --temperature 0.7 \
    --top-p 0.9 \
    --top-k 40 \
    --repeat-penalty 1.15 \
    --no-thinking \
    --single-peer
