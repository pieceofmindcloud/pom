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
- `network.rs` — UDP P2P: peer discovery (broadcast + bootstrap), gossip protocol (5s), peer lifecycle (30s timeout). `PeerInfo` carries `ollama_port` so gateway nodes are routable
- `identity.rs` — Ed25519 keypair; node ID derived from SHA256 of pubkey
- `display.rs` — Terminal UI: banner, layer map grid, peer table, coverage percentage
- `ollama.rs` — `OllamaClient` wrapping Ollama's REST API (`/api/tags`, `/api/generate`)
- `gateway.rs` — `axum` HTTP server exposing `POST /generate`, `GET /peers`, `GET /health`

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

**Gateway routing:** `POST /generate` tries local Ollama first; on failure iterates alive peers with `ollama_port` set, constructs `http://<peer_ip>:<ollama_port>` and forwards the request.
