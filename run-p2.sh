#!/usr/bin/env bash
# Node 2: layers 16-31, bootstrap para p1 em :42777
# NOTA: sem split_model.py, cada peer carrega o modelo completo mas anuncia
# apenas suas layers para o P2P. Para inferência distribuída real, use split_model.py.

MODEL="./models/Qwen3.5-9B-DeepSeek-V4-Flash-Q4_K_S.gguf"

./target/release/rdma-p2p \
    --model "$MODEL" \
    --memory 7000MB \
    --port 42778 \
    --bootstrap 127.0.0.1:42777 \
    --inference-port 8182 \
    --n-gpu-layers 99 \
    --n-ctx 4096 \
    --max-new-tokens 512 \
    --temperature 0.7 \
    --top-p 0.9 \
    --top-k 40 \
    --repeat-penalty 1.15 \
    --no-thinking
