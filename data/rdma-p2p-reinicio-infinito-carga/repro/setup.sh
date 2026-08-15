#!/bin/bash
# Monta uma "casa" isolada de nó, como um container teria em /data.
set -eu
H="$1"
rm -rf "$H"; mkdir -p "$H/models"
cat > "$H/rdma-p2p.json" <<JSON
{
  "role": "principal",
  "connection_key": "chave-de-teste",
  "gateway_port": 18080,
  "p2p_port": 42999,
  "inference_port": 18181,
  "bootstrap_nodes": [],
  "model_dirs": [],
  "shard_dir": "$H/models"
}
JSON
# Fatia que o nó vai tentar servir. Um GGUF só de vocabulário: passa no
# validate() (o arquivo existe) e o llama.cpp recusa carregar.
cp /Users/jairpereira/.unsloth/llama.cpp/models/ggml-vocab-qwen35.gguf "$H/models/fatia_0_0.gguf"
echo "home pronta em $H"
