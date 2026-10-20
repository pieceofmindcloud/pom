# pom

> Architecture note: this repository is a Cargo workspace. The shipped
> `pom` binary is the `node` package in `crates/node`; public routing
> crates live in `crates/routing-contract` and `crates/routing-base`. The
> contract extraction is complete, but runtime provider selection remains on
> the existing node planner while equivalence coverage is expanded. A future
> enterprise provider is a separate private local plugin, never a dependency of
> this repository. See `docs/architecture/routing-plugin-contract.md`.

**POM - Piece Of Mind: distributed LLM inference across ordinary machines, with no central server**

Each node allocates a slice of RAM, works out automatically how many model layers fit in it, and coordinates with the other nodes over UDP P2P to cover the whole model.

---

## Installation

```bash
# Requires Rust 1.75+
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

git clone <repo>
cd pom
cargo build --release
```

The binary lands in `./target/release/pom`.

---

## Docker

### Build

```bash
./scripts/build-docker.sh
```

Every machine in the cluster builds the same `pom` executable, with CPU, CUDA and Vulkan, plus the
`pom-plugin-host`; the GGML modules pick whichever backend is available at runtime. The
frontend is an independent stage. Development builds use, by default, the libraries of the
exact release of the fork `pieceofmindcloud/llama.cpp` (CUDA 13.4 package), to avoid
recompiling the native core. The matching release must be published before the build;
`POM_LLAMA_CPP_RELEASE_TAG` can pin a specific tag. To build a local fork, including a copy
outside POM or one with uncommitted changes:

```bash
POM_LLAMA_CPP_MODE=source \
POM_LLAMA_CPP_SOURCE_DIR=/path/to/llama.cpp \
  ./scripts/build-docker.sh
```

The per-machine and per-cluster composes live in the `pom-cluster-config` repository, which
calls this script with `COMPOSE_FILE` pointing at the cluster's compose. The script reads the POM
commit on the host and passes it only to the runtime as `POM_BUILD_COMMIT`; the `.dockerignore`
cuts `.git/` out of the build context, so without it the binary stamps `desconhecido` in
`/api/ui/node`. Publishing POM binaries does not depend on this Docker mode.

### Run

```bash
# Single node, with its state (pom.json, keys, models) in a volume
docker run --rm \
    -p 8080:8080 -p 42777:42777/udp -p 8181:8181 \
    -v ./data:/data -e POM_HOME=/data -e POM_UI_DIST=/app/apps/frontend/dist \
    pom

# Node joining a bootstrap
docker run --rm \
    -p 8080:8080 -p 42777:42777/udp -p 8181:8181 \
    -v ./data:/data -e POM_HOME=/data -e POM_UI_DIST=/app/apps/frontend/dist \
    pom --bootstrap 192.168.1.10:42777
```

The web interface is available at `http://localhost:8080/` as soon as the container
comes up. On the first visit it walks you through the node's configuration; models
are deployed through it, never from the command line.

---

## Usage

```bash
# Start a node (with no pom.json, it opens the initial configuration on :8080)
./pom

# Node with a custom port and a bootstrap
./pom --port 42777 --bootstrap 192.168.1.10:42777

# Node joining a remote bootstrap (internet)
./pom --bootstrap 45.33.32.156:42777
```

Once it is configured, open `http://<node>:8080/` and deploy the model through the
interface: it slices the GGUF, spreads the layers across the nodes and loads each
slice on its peer.

---

## How it works

### 1. Discovery
On startup, the node:
1. Sends `PeerRequest` to the bootstrap nodes
2. Broadcasts locally over UDP (mDNS-style)
3. Joins the iroh gossip for peers outside the LAN (QUIC, relays, hole punching)

### 2. Deployment
Deployment is done through the web interface: it measures the memory and GPU each node
has, plans which layers each one serves, and every peer extracts its own range
from its own copy of the model. See `docs/subsystems/deployment.md`.

### 3. P2P gossip
Every 5 seconds the node:
- Announces its layers to every peer it knows
- Broadcasts locally for new nodes on the LAN
- Drops peers that have not answered for 30s
- Exchanges peer lists

---

## Workspace layout

```
Cargo.toml                     # workspace and shared dependencies
crates/
├── node/                      # package that produces the pom binary
│   ├── src/main.rs             # CLI and startup orchestration
│   ├── src/models/             # catalog, GGUF, profiles and slicing
│   ├── src/network/            # discovery, identity and P2P transport
│   ├── src/routing/            # the current pipeline planner
│   ├── src/backend/            # HTTP gateway, API and admin interface
│   └── src/utils/              # configuration and monitoring
├── routing-contract/           # serializable, versioned routing contract
├── routing-base/               # infrastructure-independent deterministic planner
├── config/                     # extraction point for public configuration schemas
└── types/                      # extraction point for public domain types
apps/frontend/                  # React/Vite source; the node reads the build from disk at runtime
```

`cargo build`, `cargo test` and `cargo run --bin pom -- <arguments>` at the
root use `crates/node` as the default workspace member. To validate every public
crate and the node, use `cargo test --workspace`.

### Message protocol (UDP JSON)

```json
// Announce - a node announces its layers
{ "type": "Announce", "node_id": "abc...", "model": "gemma3",
  "start_layer": 11, "end_layer": 21, "memory_mb": 3840 }

// PeerRequest - asks for the peer list
{ "type": "PeerRequest", "from": "abc..." }

// PeerList - answers with the peers it knows
{ "type": "PeerList", "peers": [...] }

// Heartbeat - a liveness signal
{ "type": "Heartbeat", "node_id": "abc...", "timestamp": "..." }

// LocalDiscover - broadcast LAN
{ "type": "LocalDiscover", "from": "abc...", "port": 42777 }
```

---

## Supported models

| Model           | Layers | MB/layer | Description          |
|-----------------|--------|----------|----------------------|
| gemma3          | 28     | 180      | Google Gemma 3 2B    |
| gemma3:7b       | 32     | 420      | Google Gemma 3 7B    |
| gemma3:27b      | 46     | 1100     | Google Gemma 3 27B   |
| llama3          | 32     | 350      | Meta LLaMA 3 8B      |
| llama3:70b      | 80     | 1600     | Meta LLaMA 3 70B     |
| mistral         | 32     | 320      | Mistral 7B           |
| qwen2.5         | 28     | 290      | Qwen 2.5 7B          |
| deepseek-r1     | 64     | 980      | DeepSeek R1 32B      |

---

## Example of a distributed network

```
Machine A (10GB) → gemma3 layers 0-27   (covers everything on its own)
Machine A (3GB)  → gemma3 layers 0-14
Machine B (3GB)  → gemma3 layers 15-27  (sees the gap, takes the second half)
Machine C (2GB)  → gemma3 layers 0-9    (already covered, stays as redundancy)
```

---

## Roadmap

- [ ] Automatic WireGuard mesh between peers
- [ ] Capability tokens for memory access control
- [ ] QUIC as the transport (replace raw UDP)
- [ ] Support for more models (auto-detect via the Ollama manifest)

---

## Security (next phase)

Right now nodes talk to each other without encryption (fine for a trusted LAN).

For public networks, the plan is:
1. **WireGuard mesh** - each node_id maps to a WireGuard key
2. **Capability tokens** - signed Ed25519 tokens for access to memory regions
3. **Separate channel** - control (TLS) vs data (QUIC + ChaCha20)

---

## Dependencies

```toml
tokio       = "1"        # async runtime
clap        = "4"        # CLI
serde_json  = "1"        # P2P protocol
ed25519-dalek = "2"      # cryptographic identity
uuid        = "1"        # unique IDs
sha2        = "0.10"     # public key hash
chrono      = "0.4"      # timestamps
tracing     = "0.1"      # structured logs
anyhow      = "1"        # error handling
```