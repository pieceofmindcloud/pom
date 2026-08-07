# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build --release          # Output: ./target/release/rdma-p2p
cargo build                    # Debug build

# List models (built-in profiles + Ollama models if running)
./target/release/rdma-p2p --list-models

# Run a single node (built-in profile)
./target/release/rdma-p2p --model gemma3 --memory 10GB

# Run with an Ollama model (auto-resolves profile from Ollama API)
./target/release/rdma-p2p --model llama3:8b --memory 10GB

# Multi-node test on same machine (different ports)
./target/release/rdma-p2p --model gemma3 --memory 4GB --port 42777
./target/release/rdma-p2p --model gemma3 --memory 4GB --port 42778 --bootstrap 127.0.0.1:42777

# Gateway node (HTTP API + Ollama integration)
./target/release/rdma-p2p --model llama3:8b --memory 10GB --gateway --gateway-port 8080
# Then: curl -s localhost:8080/generate -d '{"prompt":"Hello"}' | jq
#       curl -s localhost:8080/peers | jq
#       curl -s localhost:8080/health | jq

# ── Distributed pipeline inference ───────────────────────────────────────────

# 1. Split a full GGUF model into per-node slices
python split_model.py \
    --model ~/.ollama/models/blobs/sha256-<hash> \
    --start-layer 0 --end-layer 15 \
    --output split_0_15.gguf --is-first

python split_model.py \
    --model ~/.ollama/models/blobs/sha256-<hash> \
    --start-layer 16 --end-layer 31 \
    --output split_16_31.gguf --is-last

# 2. Start the inference server on each node
#    (run alongside the rdma-p2p binary)
python inference_server.py \
    --model split_0_15.gguf \
    --start-layer 0 --end-layer 15 --port 8181

python inference_server.py \
    --model split_16_31.gguf \
    --start-layer 16 --end-layer 31 --port 8181

# 3. Start rdma-p2p nodes, advertising the inference port
./target/release/rdma-p2p --model llama3:8b --memory 8GB \
    --port 42777 --inference-port 8181

./target/release/rdma-p2p --model llama3:8b --memory 8GB \
    --port 42778 --bootstrap 127.0.0.1:42777 --inference-port 8181

# 4. Start the gateway (any node can be the gateway)
./target/release/rdma-p2p --model llama3:8b --memory 8GB \
    --port 42779 --bootstrap 127.0.0.1:42777 \
    --gateway --gateway-port 8080

# 5. Send a request — gateway auto-plans the pipeline
curl -s localhost:8080/api/generate -d '{"prompt":"Explain RDMA"}' | jq

# Run tests
cargo test

# Debug logging
RUST_LOG=debug ./target/release/rdma-p2p --model gemma3 --memory 4GB
```

## Architecture

Single-binary Rust application for decentralized LLM layer coordination with Ollama integration and an optional HTTP gateway.

**Module responsibilities:**
- `main.rs` — CLI parsing, startup orchestration, model resolution (built-in → Ollama fallback), gateway startup, main refresh loop
- `model.rs` — `ModelProfile` for 8 built-in LLMs + `from_ollama_model()` for auto-profiling; gap-filling layer assignment algorithm
- `network.rs` — UDP P2P: peer discovery (broadcast + bootstrap), gossip protocol (5s), peer lifecycle (30s timeout). `PeerInfo` carries `ollama_port` and `inference_port`
- `identity.rs` — Ed25519 keypair; node ID derived from SHA256 of pubkey
- `display.rs` — Terminal UI: banner, layer map grid, peer table, coverage percentage
- `ollama.rs` — `OllamaClient` wrapping Ollama's REST API (`/api/tags`, `/api/generate`)
- `pipeline.rs` — `LoadTracker` (RAII per-peer counters), `PipelinePlan` / `PipelineStep`, `plan_pipeline()` greedy planner
- `gateway.rs` — `axum` HTTP server: `POST /api/generate` (pipeline → local Ollama → peer Ollama), `GET /peers`, `GET /health`
- `inference_server.py` — FastAPI server per peer node; handles `POST /v1/infer` for partial forward passes (hidden-state threading)
- `split_model.py` — Pure-Python GGUF splitter; extracts layer range, remaps names, rewrites a valid GGUF slice

**Data flow:**
1. Identity generated → model resolved (built-in profile or queried from Ollama) → UDP socket bound
2. Bootstrap discovery (2s wait) → peer HashMap populated
3. `calculate_assignment()` reads covered layers from peers → finds best contiguous gap → assigns layer range
4. Two background tasks: receive loop (continuous) + gossip loop (5s re-announce, dead peer eviction)
5. If `--gateway`: axum server starts in a spawned task, routes inference to local Ollama first then peers
6. Main loop re-renders TUI every N seconds

**Concurrency:** Tokio async runtime; shared state via `Arc<RwLock<>>` for peer HashMap and assignment.

**Network protocol:** JSON-tagged enums over UDP port 42777 (default). Types: `Announce`, `PeerRequest`, `PeerList`, `Heartbeat`, `LocalDiscover`. `Announce` now carries `ollama_port: Option<u16>`.

**Multi-instance local testing:** LAN broadcast goes to `255.255.255.255:42777` — only nodes on port 42777 receive it. For multiple instances on the same machine, use `--bootstrap 127.0.0.1:<first_port>`. Peer discovery converges quickly because `PeerRequest` now triggers an immediate `Announce` reply (not just a `PeerList`).

**Layer assignment algorithm** (`model.rs` — `calculate_assignment` + `find_best_start`): subtracts memory overhead, computes max layers that fit, scans all possible start positions to maximize uncovered layers in the allocated range.

**Ollama model resolution:** `--model` checks built-in profiles first; if not found, queries `--ollama-url` (`/api/tags`) and calls `ModelProfile::from_ollama_model(name, size_bytes)` to estimate layer count from file size.

**Gateway routing:** `POST /api/generate` three-tier fallback:
1. **Distributed pipeline** — `plan_pipeline()` builds an ordered `PipelinePlan` from peers advertising `inference_port`; `execute_pipeline()` chains HTTP calls threading `hidden_state` tensors (base64-encoded float32); load tracked via RAII `LoadGuard`
2. **Local Ollama** — direct call to local Ollama instance
3. **Peer Ollama** — iterates alive peers with `ollama_url()` and forwards the request

**Pipeline planner** (`pipeline.rs`): greedy sweep from layer 0 to total_layers; at each position picks the peer scoring `layers_count * 100 - current_load * 10`; returns `None` (falls back) if any gap exists.

**`--inference-port`**: when set, the node advertises itself as a pipeline participant. Run `inference_server.py` on the same port alongside the rdma-p2p binary.

**`inference_server.py`**: FastAPI + llama-cpp-python. First peer tokenizes → runs eval → extracts hidden state via `llama_get_embeddings`. Middle peers inject via `llama_batch.embd` → run decode → extract. Last peer samples tokens → returns text.

**`split_model.py`**: pure stdlib GGUF reader/writer. Selects `blk.<N>.*` tensors for the assigned range, plus `token_embd.weight` (first), `output_norm/output.weight` (last). Remaps names `blk.(start+i).*` → `blk.i.*`, updates `llama.block_count` KV, writes new GGUF with recalculated offsets.

---

## Subindo o cluster de dois peers (MacBook + ksia)

Procedimento verificado em 2026-08-07 com os shards `models/qwen/qwen_0_15.gguf`
(peer 1) e `models/qwen/qwen_16_31.gguf` (peer 2).

### Como as peças se encaixam

- **Um processo por máquina.** O binário Rust sobe o `inference_server.py`
  sozinho quando `--inference-port` é passado (ver `src/main.rs`, bloco
  "Start Inference Server"). Não se roda o Python à mão.
- Ele procura o interpretador em `./venv/bin/python3` relativo ao **cwd**;
  se não existir, cai para `python3` do sistema. Por isso os scripts fazem
  `cd "$(dirname "$0")"` e o venv tem que ficar na raiz do repo.
- **As camadas saem do nome do arquivo.** `parse_layers_from_filename()` lê os
  dois últimos campos separados por `_`: `qwen_0_15.gguf` → 0-15,
  `qwen_16_31.gguf` → 16-31. Não é preciso `--start-layer`.
- **O nome do modelo tem que bater entre os peers.** `ModelProfile::guess_from_name()`
  reduz o nome do arquivo a um rótulo (qualquer nome contendo `qwen` vira
  `qwen35`). O planejador (`pipeline.rs`) só encadeia peers com `model` idêntico -
  shards com rótulos diferentes nunca formam pipeline.
- **Portas:** UDP P2P `--port` (42777), libp2p TCP = UDP + 10000 (52777),
  gateway HTTP `--gateway-port` (8080, servido por *todos* os nós),
  inference server `--inference-port` (8181). Como as máquinas são distintas,
  ambas usam os mesmos números.
- **Contexto por sessão:** `_MAX_SESSIONS = 8` no `inference_server.py` divide
  o `--n-ctx` em 8 slots. `--n-ctx 8192` → 1024 tokens por requisição.

### Dependências reais

**MacBook (Apple Silicon / Metal)**

```bash
python3 -m venv venv
./venv/bin/pip install --upgrade pip wheel setuptools cmake ninja
./venv/bin/pip install fastapi uvicorn pydantic numpy
CMAKE_ARGS="-DGGML_METAL=on -DGGML_ACCELERATE=on" \
  ./venv/bin/pip install llama-cpp-python --no-cache-dir
cargo build --release
```

**ksia (Ubuntu 24.04 + RTX 5060 Ti, Blackwell sm_120)**

O `nvidia-cuda-toolkit` do apt do Ubuntu é 12.0 e **não compila para sm_120**.
Use o repositório oficial da NVIDIA e um toolkit >= 12.8:

```bash
curl -fsSLO https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb
dpkg -i cuda-keyring_1.1-1_all.deb && apt-get update
apt-get install -y cuda-toolkit-12-9 build-essential cmake python3-venv python3-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal

cd /home/jairpereira/workspace/rdma-p2p
python3 -m venv venv
./venv/bin/pip install --upgrade pip wheel setuptools
./venv/bin/pip install fastapi uvicorn pydantic numpy
export PATH=/usr/local/cuda-12.9/bin:$PATH CUDA_HOME=/usr/local/cuda-12.9
CMAKE_ARGS="-DGGML_CUDA=on -DCMAKE_CUDA_ARCHITECTURES=120 -DCMAKE_CUDA_COMPILER=$CUDA_HOME/bin/nvcc" \
  ./venv/bin/pip install llama-cpp-python --no-cache-dir
PATH=/root/.cargo/bin:$PATH cargo build --release
```

Confira o build antes de subir o peer — sem isto `--n-gpu-layers 99` roda em CPU:

```bash
./venv/bin/python -c "import llama_cpp.llama_cpp as lc; print(lc.llama_supports_gpu_offload())"
# True + linha "Device 0: NVIDIA GeForce RTX 5060 Ti, compute capability 12.0"
```

Mantenha a **mesma versão de `llama-cpp-python` nas duas máquinas** (0.3.34 na
validação): o protocolo entre peers troca hidden states crus, e mudança de
layout entre versões corrompe silenciosamente a saída.

### Subida

```bash
# MacBook (peer 1, camadas 0-15, gateway):
./run-p1.sh

# ksia (peer 2, camadas 16-31):
ssh root@192.168.100.138 'cd /home/jairpereira/workspace/rdma-p2p && ./run-p2.sh'
# BOOTSTRAP=<ip-do-peer1>:42777 sobrescreve o endereço do peer 1.
```

O carregamento do modelo leva ~1 min; espere `/health` do inference server antes
de mandar requisição:

```bash
until curl -sf localhost:8181/health; do sleep 5; done
```

### Verificação

```bash
curl -s localhost:8080/peers | python3 -m json.tool   # 2 peers, camadas 0-15 e 16-31
ssh root@192.168.100.138 'nvidia-smi --query-compute-apps=pid,used_memory --format=csv'
./request.sh "Explique em uma frase o que e RDMA."
# → "source": "pipeline:<id-peer1>→<id-peer2>"
```

`source` começando com `pipeline:` e com **dois** hops é a única prova de que a
inferência foi conjunta. Se aparecer só um hop, ou `"source": "none"`, o
planejador não fechou a cobertura 0..total_layers.

### Pegadinhas encontradas

- **`git pull` no ksia.** Os arquivos são do usuário `jairpereira` e o acesso é
  como `root`: rode
  `git config --global --add safe.directory /home/jairpereira/workspace/rdma-p2p`.
  O repositório é privado, então o ksia usa uma *deploy key* read-only
  (`/root/.ssh/id_ed25519`) registrada no GitHub; o remoto tem que ser `git@github.com:...`.
- **Python do sistema no macOS é 3.9** e não tem numpy. Sem o venv na raiz do
  repo o inference server morre em loop com `ModuleNotFoundError: numpy` — e o
  Rust só registra isso em `inference_server_<porta>.log`, não na TUI.
- **`--memory` é dispensável** quando `--inference-port` está setado (o processo
  Python é dono da memória do modelo).
- **Shards sem metadata `rdmap2p.*`** (splits antigos, como os `qwen_*`) não
  corrigem `total_layers` via `/health`; nesses casos o valor vem do palpite de
  `guess_from_name` (32 para `qwen`), que só por sorte bate. Shards novos
  (`split_model.py`) gravam `rdmap2p.original_block_count` e são a via confiável.
- **A TUI limpa a tela a cada `--refresh` segundos.** Para ver logs, redirecione
  para arquivo (`./run-p1.sh > /tmp/p1.out 2>&1`) e leia de lá.

### Estado do cenário "peers em redes diferentes, pela internet"

Já existe em `src/discovery.rs` e `src/pkarr_node.rs`: Circuit Relay v2 (cliente
e `--serve-relay`), DCuTR para hole punching, AutoNAT, Kademlia, gossipsub e
publicação/resolução de endereço via DHT pkarr (`--pkarr-peers`, `--public-addr`,
`--relay-peers`, `--no-default-relays`). Ver os specs em `docs/superpowers/specs/`.

Falta para funcionar de fato pela internet:

1. **O endereço anunciado no gossipsub é literal `0.0.0.0:<porta>`**
   (`src/main.rs`, task de announce periódico). Na LAN não incomoda porque o
   caminho UDP reescreve `addr` com o IP de origem (`network.rs`,
   `handle_message`), mas `insert_peer()` (caminho libp2p) não reescreve nada -
   um peer que só chegue por gossipsub fica com `inference_url` = `http://0.0.0.0:8181`,
   que resolve para o *próprio* nó. Pela internet esse é o caminho principal.
2. **O tráfego de inferência não passa pelo libp2p.** O gateway fala HTTP direto
   com `http://<ip-do-peer>:<inference-port>` (`pipeline.rs`/`gateway.rs`). Relay
   e DCuTR só carregam a descoberta; sem IP roteável ou porta liberada, o hop
   HTTP falha mesmo com os peers "conectados".
3. **`--public-addr` só alimenta o pkarr**, não o `addr` do `Announce`.
4. Sem autenticação nem TLS no `/v1/infer` e no gateway - aceitável em LAN
   confiável, não na internet.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
