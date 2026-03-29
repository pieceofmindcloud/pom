#!/usr/bin/env bash
./target/release/rdma-p2p \
  --model ./models/gemma2_21_41.gguf \
  --memory 4GB \
  --port 42778 \
  --bootstrap 127.0.0.1:42777 \
  --inference-port 8182
