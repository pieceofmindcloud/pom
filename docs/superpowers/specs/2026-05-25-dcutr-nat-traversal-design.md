# DCuTR NAT Traversal — Design Spec
**Date:** 2026-05-25  
**Status:** Approved

## Problem

Two rdma-p2p nodes on different networks (different home WiFi routers) cannot connect to each other because both are behind NAT without port forwarding access. The pkarr integration publishes LAN IPs that are unreachable from outside the local network.

## Solution

Integrate libp2p Circuit Relay v2 + DCuTR (Direct Connection Upgrade Through Relay) + AutoNAT into the existing libp2p stack in `discovery.rs`. Nodes connect to a relay node as a rendezvous point, then attempt simultaneous TCP hole punching (DCuTR) to establish a direct connection. The relay is only used for introduction and fallback — all actual inference traffic flows directly between peers.

## Connection Flow

```
Startup
  ├── Dial relay nodes (public IPFS + --relay-peers)
  ├── Identify protocol → learn relay's PeerId
  ├── listen_on /p2p/<relay_id>/p2p-circuit
  └── ReservationReqAccepted → node reachable via relay

Peer discovery
  ├── Peer1 and Peer2 both connected to same relay
  ├── Gossipsub flows over relay circuit
  └── DCuTR detects pair connected via relay → initiates hole punch
        ├── Success → direct TCP connection (relay discarded)
        └── Failure → stays on relay as fallback (low-bandwidth only)
```

## Architecture

```
identity + pkarr (pkarr_node.rs)
    │
    ├── Discovery (discovery.rs)
    │     DiscBehaviour:
    │       • gossipsub    — peer announce propagation (unchanged)
    │       • kademlia     — DHT routing (unchanged)
    │       • mdns         — LAN discovery (unchanged)
    │       • identify     — learn peer addresses + relay PeerIds
    │       • relay client — circuit relay v2 transport client
    │       • dcutr        — hole punching upgrade
    │       • autonat      — NAT status detection
    │       • relay server — optional, --serve-relay only
    │
    └── Relay nodes
          • DEFAULT_RELAY_NODES (hardcoded public IPFS, used by default)
          • --relay-peers pk:...  (own-network nodes resolved via pkarr)
```

## Node Roles

| Scenario | Relay Client | Relay Server | DCuTR |
|----------|-------------|--------------|-------|
| Behind NAT (default) | ✓ | — | ✓ |
| Public IP + `--serve-relay` | ✓ | ✓ | ✓ |
| IPFS public nodes (external) | — | ✓ | — |

## Files Changed

| File | Change |
|------|--------|
| `Cargo.toml` | Add `"relay"`, `"dcutr"`, `"autonat"` to libp2p features |
| `src/discovery.rs` | Refactor DiscBehaviour, SwarmBuilder chain, event handler, startup relay dial |
| `src/main.rs` | Add `--relay-peers`, `--serve-relay`, `--no-default-relays` flags; resolve relay peers via pkarr; pass to Discovery::new() |
| `src/pkarr_node.rs` | Add `r: bool` field to TxtPayload for relay capability announcement |

## Default Relay Nodes

Public libp2p/IPFS relay nodes (circuit relay v2, stable infrastructure):

```rust
const DEFAULT_RELAY_NODES: &[&str] = &[
    "/ip4/147.75.87.27/tcp/4001/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN",
    "/ip4/147.75.195.153/tcp/4001/p2p/QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa",
    "/ip4/104.131.131.82/tcp/4001/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ",
];
```

Traffic through these relays is Noise-encrypted end-to-end. Relay nodes cannot read content.

## `discovery.rs` — Structural Changes

### DiscBehaviour

```rust
#[derive(NetworkBehaviour)]
struct DiscBehaviour {
    gossipsub:    gossipsub::Behaviour,
    kademlia:     kad::Behaviour<kad::store::MemoryStore>,
    mdns:         mdns::tokio::Behaviour,
    identify:     identify::Behaviour,
    relay:        relay::client::Behaviour,   // new
    dcutr:        dcutr::Behaviour,           // new
    autonat:      autonat::Behaviour,         // new
    relay_server: relay::Behaviour,           // new (noop unless --serve-relay)
}
```

### SwarmBuilder Chain

`.with_relay_client()` must be inserted between `.with_dns()` and `.with_behaviour()`. The behaviour closure receives `relay_client` as a second argument:

```rust
SwarmBuilder::with_existing_identity(keypair)
    .with_tokio()
    .with_tcp(tcp::Config::default().nodelay(true), noise::Config::new, yamux::Config::default)?
    .with_dns()?
    .with_relay_client(noise::Config::new, yamux::Config::default)?   // new
    .with_behaviour(|key, relay_client| {                             // relay_client passed in
        let peer_id = key.public().to_peer_id();
        DiscBehaviour {
            gossipsub:    ...,
            kademlia:     ...,
            mdns:         ...,
            identify:     ...,
            relay:        relay_client,
            dcutr:        dcutr::Behaviour::new(peer_id),
            autonat:      autonat::Behaviour::new(peer_id, autonat::Config::default()),
            relay_server: relay::Behaviour::new(
                peer_id,
                if serve_relay {
                    relay::Config {
                        max_reservations: 128,
                        max_reservations_per_peer: 4,
                        reservation_duration: Duration::from_secs(3600),
                        max_circuit_duration: Duration::from_secs(120),
                        max_circuit_bytes: 1 << 17,
                        ..Default::default()
                    }
                } else {
                    // Server disabled: zero reservations accepted
                    relay::Config { max_reservations: 0, ..Default::default() }
                },
            ),
        }
    })?
    .with_swarm_config(...)
    .build()
```

### Discovery::new() Signature

```rust
pub fn new(
    udp_port: u16,
    bootstrap_addrs: &[String],
    relay_addrs: &[String],       // resolved multiaddrs from --relay-peers pkarr keys
    serve_relay: bool,            // --serve-relay flag
    use_default_relays: bool,     // !--no-default-relays
    network: Arc<Network>,
) -> Result<Self>
```

### Discovery Struct — New Field

```rust
pub struct Discovery {
    pub announce_tx:   mpsc::Sender<Vec<u8>>,
    pub dial_tx:       mpsc::Sender<Multiaddr>,
    // ... existing fields ...
    relay_peer_addrs:  HashSet<Multiaddr>,  // multiaddrs we dialled as relay nodes
    connected_relays:  HashSet<PeerId>,     // PeerIds confirmed as connected relays
}
```

`relay_peer_addrs` is populated in `new()` from `DEFAULT_RELAY_NODES` + `relay_addrs`. In `run()`, `ConnectionEstablished` checks if the endpoint address is in this set to populate `connected_relays`.

### Startup Relay Dialling (in new())

After the swarm is built and listening, dial all relay nodes:

```rust
// Default public relays
if use_default_relays {
    for addr_str in DEFAULT_RELAY_NODES {
        if let Ok(ma) = addr_str.parse::<Multiaddr>() {
            swarm.dial(ma).ok();
        }
    }
}
// User-supplied relay peers (already resolved to multiaddrs by main.rs)
for addr_str in relay_addrs {
    if let Ok(ma) = addr_str.parse::<Multiaddr>() {
        swarm.dial(ma).ok();
    }
}
```

### Relay Circuit Listening (in run() — Identify handler)

When Identify fires for a connected peer that is a relay node, listen on its circuit address:

```rust
SwarmEvent::Behaviour(DiscBehaviourEvent::Identify(identify::Event::Received {
    peer_id, info, ..
})) => {
    for addr in &info.listen_addrs {
        swarm.behaviour_mut().kademlia.add_address(&peer_id, addr.clone());
    }
    // If this peer is a relay we dialled, reserve a circuit slot
    if connected_relays.contains(&peer_id) {
        let relay_circuit: Multiaddr = format!("/p2p/{}/p2p-circuit", peer_id).parse().unwrap();
        swarm.listen_on(relay_circuit).ok();
    }
}
```

### New Events in run()

```rust
// Relay reservation accepted → we are now reachable via this relay
DiscBehaviourEvent::Relay(relay::client::Event::ReservationReqAccepted {
    relay_peer_id, ..
}) => {
    info!("🔗 Relay slot reserved on {}", relay_peer_id);
}

// Relay reservation failed → try next relay
DiscBehaviourEvent::Relay(relay::client::Event::ReservationReqFailed {
    relay_peer_id, ..
}) => {
    warn!("Relay reservation failed on {}", relay_peer_id);
}

// Hole punch succeeded → direct connection active
DiscBehaviourEvent::Dcutr(dcutr::Event::DirectConnectionUpgradeSucceeded {
    remote_peer_id,
}) => {
    info!("⚡ Direct connection to {} (hole punch succeeded)", remote_peer_id);
}

// Hole punch failed → relay fallback active
DiscBehaviourEvent::Dcutr(dcutr::Event::DirectConnectionUpgradeFailed {
    remote_peer_id, error, ..
}) => {
    warn!("DCuTR failed for {} ({:?}) — relay fallback active", remote_peer_id, error);
}

// AutoNAT status update
DiscBehaviourEvent::Autonat(autonat::Event::StatusChanged { new, .. }) => {
    info!("NAT status: {:?}", new);
}
```

### ConnectionEstablished — Track Relay Connections

```rust
SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
    // If we dialled this peer as a relay node, mark it
    if dialled_relay_addrs.contains(&endpoint.get_remote_address()) {
        connected_relays.insert(peer_id);
    }
    debug!("libp2p: connected to {}", peer_id);
}
```

## `--serve-relay` Limits

When `--serve-relay` is set, the relay server uses conservative bandwidth limits so the node is not abused as a public relay for unrelated traffic:

```rust
relay::Config {
    max_reservations: 128,
    max_reservations_per_peer: 4,
    reservation_duration: Duration::from_secs(3600),   // 1 hour
    max_circuit_duration: Duration::from_secs(120),    // 2 min per circuit
    max_circuit_bytes: 1 << 17,                        // 128 KB per circuit
}
```

The low `max_circuit_bytes` is intentional. After hole punching, inference traffic (hidden states) flows directly between peers — it does not go through the relay. The relay only handles the introduction handshake and fallback for peers that cannot hole-punch.

## `pkarr_node.rs` — Relay Capability Announcement

Nodes running with `--serve-relay` add `"r":true` to their TxtPayload:

```json
{"a":"/ip4/1.2.3.4/tcp/52777","i":8181,"m":"qwen3.6","s":0,"e":19,"t":39,"r":true}
```

`ResolvedPeer` gains a `is_relay: bool` field. In `main.rs`, when resolving `--relay-peers`, nodes with `is_relay: true` are passed as `relay_addrs` to `Discovery::new()`.

## New CLI Flags

```
--relay-peers <pk:abc,...>    Pkarr keys of nodes to use as relay (resolves via DHT)
--serve-relay                 Accept relay connections from other peers (requires public IP)
--no-default-relays           Disable hardcoded public IPFS relay nodes
```

## Typical Usage

```bash
# Peer 1 — behind NAT, connects via public IPFS relays automatically
./rdma-p2p --model qwen3.6 --inference-port 8180 ...
# Logs: 🔗 Relay slot reserved on QmNnooDu7...
#       ⚡ Direct connection to <peer2> (hole punch succeeded)

# Peer 2 — behind different NAT
./rdma-p2p --model qwen3.6 --inference-port 8182 \
    --pkarr-peers pk:fxbswjpo...

# Optional: own relay node (if you have a VPS)
./rdma-p2p --model qwen3.6 --serve-relay ...
# Other peers: --relay-peers pk:<vps_key>
```

## Security

- **Traffic privacy:** All connections use Noise protocol encryption. Relay nodes cannot read content.
- **Relay server abuse:** Mitigated by `max_circuit_bytes = 128KB` and `max_circuit_duration = 120s`. Inference hidden states never flow through the relay.
- **IP exposure:** Direct peers learn your external IP after hole punch — inherent in any P2P system.
- **Gossipsub:** Topic remains open (pre-existing). DCuTR does not change the gossipsub security model.
- **`--serve-relay` opt-in:** Relay server is never activated by default, even on nodes with public IPs.

## Non-Goals

- NAT traversal for UDP (only TCP hole punching via DCuTR)
- Replacing pkarr discovery (pkarr and relay serve different purposes)
- Automatic relay server election (always manual via `--serve-relay`)
- QUIC transport (future work)
