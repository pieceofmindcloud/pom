#!/usr/bin/env bash
./target/release/rdma-p2p \
  --model ./models/gemma2_0_20.gguf \
  --memory 4GB \
  --port 42777 \
  --gateway \
  --gateway-port 8080 \
  --inference-port 8181
