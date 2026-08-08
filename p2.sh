#!/usr/bin/env bash

./target/release/rdma-p2p \
      --model "./models/qwen/qwen_16_31.gguf" \
      --port 42777 \
      --inference-port 8182 \
      --pkarr-peers pk:fxbswjpo9izgpk4hbfw878txjtjiu3sju3cf4k9t1uw18h3jqr4o \
      --n-gpu-layers 99 --n-ctx 2048 --n-batch 512 \
      --max-new-tokens 512 --temperature 0.7 --top-p 0.9 --top-k 40 \
      --repeat-penalty 1.15 --no-thinking --start-layer 16