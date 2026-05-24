# Pkarr Discovery Integration — Design Spec
**Date:** 2026-05-24  
**Status:** Approved

## Problem

Current peer discovery depends on either:
- UDP broadcast (LAN only)
- `--bootstrap 192.168.1.100:42777` (breaks when IP changes)

Nodes on different networks across the internet cannot find each other automatically.

## Solution

Integrate [pkarr](https://github.com/pubky/pkarr) (Public Key Addressable Resource Records) for stable, IP-independent peer discovery over the Mainline DHT (BitTorrent DHT).

Each node's `pk:zbase32...` address is derived from its Ed25519 keypair and is **stable forever** — even if the IP changes, the pkarr address stays the same.

## Architecture

```
Identity (persisted in .rdma-p2p.key)
    │
    ├── PkarrNode (src/pkarr.rs)
    │     • publishes TXT record to Mainline DHT on startup
    │     • resolves --pkarr-peers → libp2p multiaddrs
    │     • republishes every 30 min
    │
    └── Discovery (discovery.rs) — unchanged
          • receives multiaddrs from PkarrNode
          • swarm.dial(multiaddr) → gossipsub propagates peers
```

Everything else (network.rs, gateway.rs, pipeline.rs, display.rs) is untouched.

## Identity Persistence

On first run, a 32-byte Ed25519 secret key is generated and saved to `.rdma-p2p.key`:
```json
{"secret_key_hex": "a1b2c3...64hexchars"}
```
Subsequent runs load this file. The pkarr address (`pk:...`) is stable across restarts.

`.rdma-p2p.key` must be added to `.gitignore`.

## TXT Record Format

**DNS name:** `_rdmap2p`  
**Value (compact JSON, ~120 bytes):**
```json
{"a":"/ip4/203.0.113.5/tcp/52777","i":8181,"m":"qwen3.6","s":0,"e":19,"t":39}
```

| Field | Meaning |
|-------|---------|
| `a` | libp2p multiaddr (`/ip4/HOST/tcp/PORT`, where PORT = UDP port + 10000) |
| `i` | inference_port (0 = none) |
| `m` | model name |
| `s`/`e`/`t` | start_layer / end_layer / total_layers |

**Address resolution order:**
1. `--public-addr HOST:PORT` if specified
2. Auto-detected local IP via `UdpSocket::connect("8.8.8.8:80")` (no data sent)

**TTL:** 300s in DNS packet. pkarr uses the record's Ed25519 timestamp for DHT freshness.  
**Republish interval:** 30 minutes (keeps record alive; pkarr records expire after ~2h without refresh).

## New CLI Flags

```
--pkarr-peers <pk:abc...,pk:xyz...>   pkarr public keys of known peers (comma-separated)
--public-addr <HOST:PORT>             External address to publish (for NAT/port-forward scenarios)
```

Publishing always happens when `--inference-port` is set. No `--no-pkarr-publish` flag (YAGNI).

## Typical Usage (2-3 node cluster)

```bash
# Peer 1 — first run generates .rdma-p2p.key
./rdma-p2p --model qwen3.6 --inference-port 8180 ...
# Logs: 🔑 Pkarr address: pk:yqnm8dkk5ryn8dt3jxhg7k4td5...

# Peer 2 — resolves peer 1 on startup, no IP needed
./rdma-p2p --model qwen3.6 --inference-port 8182 \
    --pkarr-peers pk:yqnm8dkk5ryn8dt3jxhg7k4td5...
```

Peer 2 resolves peer 1's current IP from the DHT → dials libp2p → gossipsub propagates peers.  
If peer 1's IP changes, it republishes → peer 2 finds the new IP on next startup.

## Files Changed

| File | Change |
|------|--------|
| `src/pkarr.rs` | **New** — `PkarrNode` + `ResolvedPeer` |
| `src/identity.rs` | Add `load_or_generate(path)` + `save(path)` |
| `src/main.rs` | Add 2 CLI flags, create PkarrNode, resolve peers, spawn republish task |
| `src/discovery.rs` | Expose `dial_peer(multiaddr: &str)` method |
| `Cargo.toml` | Add `pkarr = "2"` |
| `.gitignore` | Add `.rdma-p2p.key` |

## PkarrNode Interface

```rust
pub struct PkarrNode { /* client + keypair */ }

impl PkarrNode {
    pub fn new(secret_key: [u8; 32]) -> Result<Self>;
    pub fn public_key_str(&self) -> String;           // "pk:zbase32..."
    pub async fn publish(&self, addr: &str,
                         inference_port: Option<u16>,
                         model: &str,
                         start: u32, end: u32, total: u32) -> Result<()>;
    pub async fn resolve_peers(&self, pubkeys: &[String]) -> Vec<ResolvedPeer>;
}

pub struct ResolvedPeer {
    pub multiaddr: String,
    pub inference_port: Option<u16>,
    pub model: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
}
```

## NAT Considerations

- Nodes with public IPs or port-forwarding: use `--public-addr` → fully discoverable
- Nodes behind NAT without port-forwarding: publish LAN IP → not reachable from internet, but can connect OUT to public nodes
- This is acceptable for the initial 2-3 node use case where users control their machines

## Non-Goals

- Bootstrap nodes / open network (future work if cluster grows)
- Replacing libp2p Kademlia (pkarr and Kademlia serve different purposes)
- pkarr-native identity refactor (future work)
