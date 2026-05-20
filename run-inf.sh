#!/usr/bin/env bash
# Inference server for Qwen3.5-9B (single-peer full model).
# GPU: --n-gpu-layers 99 offloads all 32 layers to VRAM (RTX 5060 Ti 16GB).
#      With all weights on GPU, the full 16GB VRAM is available for the
#      SSM state KV cache, giving 16000 / ~24MB per pos ≈ 650+ tokens context.
# --no-thinking disables Qwen3.5 reasoning block to save context budget.

MODEL="${1:-./models/Qwen3.5-9B-DeepSeek-V4-Flash-Q4_K_S.gguf}"
PORT="${2:-8181}"

source venv/bin/activate 2>/dev/null || true

python3 inference_server.py \
    --model "$MODEL" \
    --start-layer 0 \
    --end-layer 31 \
    --port "$PORT" \
    --n-gpu-layers 99 \
    --n-ctx 8192 \
    --n-batch 512 \
    --max-new-tokens 512 \
    --temperature 0.7 \
    --top-p 0.9 \
    --top-k 40 \
    --repeat-penalty 1.15 \
    --repeat-last-n 64 \
    --single-peer \
    --no-thinking
