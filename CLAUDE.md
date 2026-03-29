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
