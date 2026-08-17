#!/usr/bin/env bash
# Node 2: layers 16-31, bootstrap para p1 em :42777
# NOTA: sem split_model.py, cada peer carrega o modelo completo mas anuncia
# apenas suas layers para o P2P. Para inferência distribuída real, use split_model.py.

MODEL="./models/qwen/qwen_16_31.gguf"

./target/release/pom \
    --model "$MODEL" \
    --port 42777 \
    --bootstrap 2804:d55:5579:1000:3939:e9e9:a832:bc6f:42777 \
    --inference-port 8182 \
    --n-gpu-layers 99 \
    --pkarr-peers pk:fxbswjpo9izgpk4hbfw878txjtjiu3sju3cf4k9t1uw18h3jqr4o \
    --n-ctx 2048 \
    --n-batch 512 \
    --max-new-tokens 512 \
    --temperature 0.7 \
    --top-p 0.9 \
    --top-k 40 \
    --repeat-penalty 1.15 \
    --no-thinking
