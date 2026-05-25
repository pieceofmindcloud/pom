# DCuTR NAT Traversal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add libp2p Circuit Relay v2 + DCuTR + AutoNAT to rdma-p2p so two nodes behind different NAT routers connect without port forwarding.

**Architecture:** Nodes dial public IPFS relay nodes on startup, reserve circuit slots, and exchange gossipsub messages via relay. When two peers are both on the same relay, DCuTR initiates TCP hole punching; on success the relay is discarded and inference traffic flows directly. On failure, relay remains as low-bandwidth fallback.

**Tech Stack:** Rust/Tokio, libp2p 0.56, pkarr 6, relay/dcutr/autonat libp2p feature flags.

---

## File Map

| File | Change |
|------|--------|
| `Cargo.toml` | Add `"relay"`, `"dcutr"`, `"autonat"` to libp2p features |
| `src/pkarr_node.rs` | Add `r: Option<bool>` to `TxtPayload`, `is_relay: bool` to `ResolvedPeer`, `is_relay: bool` param to `publish()` |
| `src/discovery.rs` | Full refactor: new imports, `DiscBehaviour` + 4 fields, `SwarmBuilder` chain, `Discovery` struct + 2 fields, `new()` signature, `run()` destructuring, `handle_event()` relay/dcutr/autonat arms |
| `src/main.rs` | New `--relay-peers`, `--serve-relay`, `--no-default-relays` flags; relay peer resolution; updated `Discovery::new()` call; updated `pkarr_node::publish()` calls |

---

### Task 1: Cargo.toml — add relay/dcutr/autonat features

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add features to libp2p dependency**

In `Cargo.toml`, replace the libp2p features list:

```toml
libp2p = { version = "0.56", features = [
    "tokio", "tcp", "dns", "noise", "yamux",
    "identify", "kad", "mdns", "gossipsub", "macros",
    "relay", "dcutr", "autonat",
] }
```

- [ ] **Step 2: Fetch and verify**

Run: `cargo check 2>&1 | tail -5`

Expected: `Finished` or only errors in `src/` (not in libp2p itself). This downloads relay/dcutr/autonat crates into cargo cache.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "feat(deps): add relay, dcutr, autonat to libp2p features"
```

---

### Task 2: pkarr_node.rs — relay capability field

**Files:**
- Modify: `src/pkarr_node.rs`
- Modify: `src/main.rs` (update two `publish()` call sites to pass `false`)

- [ ] **Step 1: Write failing test**

Add to the `#[cfg(test)]` block in `src/pkarr_node.rs`:

```rust
#[test]
fn roundtrip_relay_field() {
    let with_relay = TxtPayload {
        a: "1.2.3.4:42777".to_string(),
        i: None,
        m: "llama3:8b".to_string(),
        s: 0,
        e: 15,
        t: 32,
        r: Some(true),
    };
    let json = serde_json::to_string(&with_relay).expect("serialize");
    assert!(json.contains("\"r\":true"), "relay flag should be in JSON: {}", json);
    let decoded: TxtPayload = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded.r, Some(true));

    let no_relay = TxtPayload {
        a: "1.2.3.4:42777".to_string(),
        i: None,
        m: "llama3:8b".to_string(),
        s: 0,
        e: 15,
        t: 32,
        r: None,
    };
    let json2 = serde_json::to_string(&no_relay).expect("serialize");
    assert!(!json2.contains("\"r\""), "relay flag should be absent: {}", json2);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test roundtrip_relay_field 2>&1 | tail -20`

Expected: compile error — `TxtPayload` has no field `r`.

- [ ] **Step 3: Add `r` field to `TxtPayload`**

In `src/pkarr_node.rs`, replace the `TxtPayload` struct:

```rust
#[derive(Debug, Serialize, Deserialize)]
struct TxtPayload {
    /// Multiaddr / address string (e.g. "192.168.1.5:42777")
    a: String,
    /// Optional inference server port
    #[serde(skip_serializing_if = "Option::is_none")]
    i: Option<u16>,
    /// Model name
    m: String,
    /// Start layer (inclusive)
    s: u32,
    /// End layer (inclusive)
    e: u32,
    /// Total layers in the full model
    t: u32,
    /// Relay capability (true if this node accepts relay connections)
    #[serde(skip_serializing_if = "Option::is_none")]
    r: Option<bool>,
}
```

- [ ] **Step 4: Add `is_relay` to `ResolvedPeer`**

Replace the `ResolvedPeer` struct:

```rust
#[derive(Debug, Clone)]
pub struct ResolvedPeer {
    pub multiaddr: String,
    pub inference_port: Option<u16>,
    pub model: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    pub is_relay: bool,
}
```

- [ ] **Step 5: Update `parse_signed_packet` to populate `is_relay`**

Replace the `return Some(ResolvedPeer { ... })` block inside `parse_signed_packet`:

```rust
return Some(ResolvedPeer {
    multiaddr: payload.a,
    inference_port: payload.i,
    model: payload.m,
    start_layer: payload.s,
    end_layer: payload.e,
    total_layers: payload.t,
    is_relay: payload.r.unwrap_or(false),
});
```

- [ ] **Step 6: Add `is_relay` param to `publish()`**

Replace the `pub async fn publish(` signature and body:

```rust
pub async fn publish(
    &self,
    addr: &str,
    inference_port: Option<u16>,
    model: &str,
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    is_relay: bool,
) -> Result<()> {
    let payload = TxtPayload {
        a: addr.to_string(),
        i: inference_port,
        m: model.to_string(),
        s: start_layer,
        e: end_layer,
        t: total_layers,
        r: if is_relay { Some(true) } else { None },
    };

    let json = serde_json::to_string(&payload)?;

    let sp = SignedPacket::builder()
        .txt(
            "_rdmap2p".try_into()?,
            json.as_str().try_into()?,
            300,
        )
        .sign(&self.keypair)?;

    self.client.publish(&sp, None).await?;

    info!("Published pkarr record: {}", json);

    Ok(())
}
```

- [ ] **Step 7: Fix existing tests — add `r: None` to TxtPayload literals**

In the `roundtrip_txt_payload` test, add `r: None` to the `TxtPayload { ... }` literal.  
In the `roundtrip_no_inference_port` test, add `r: None` to the `TxtPayload { ... }` literal.

- [ ] **Step 8: Fix main.rs publish call sites — pass `false` for now**

In `src/main.rs`, update the two `publish` calls to include the new `is_relay` argument:

Line ~711:
```rust
if let Err(e) = pn2.publish(&addr2, inf_port, &mn2, start_l, end_l, total_l, false).await {
```

Line ~730:
```rust
if let Err(e) = pn.publish(&addr, inf_port, &model_name_pk, start_l, end_l, total_l, false).await {
```

- [ ] **Step 9: Run tests**

Run: `cargo test -p rdma-p2p pkarr_node 2>&1 | tail -15`

Expected: all 4 tests pass (roundtrip_txt_payload, roundtrip_no_inference_port, detect_local_ip_returns_something, roundtrip_relay_field).

- [ ] **Step 10: Commit**

```bash
git add src/pkarr_node.rs src/main.rs
git commit -m "feat(pkarr): add relay capability field r to TxtPayload and ResolvedPeer"
```

---

### Task 3: discovery.rs — full relay/DCuTR/AutoNAT refactor

**Files:**
- Modify: `src/discovery.rs`

This task rewrites the entire file. Read the current file, then replace with the implementation below.

- [ ] **Step 1: Write the complete new `src/discovery.rs`**

Replace the entire file contents with:

```rust
use crate::network::{Network, PeerInfo, P2PMessage};
use anyhow::Result;
use futures::StreamExt;
use libp2p::{
    autonat, dcutr, gossipsub, identify, kad, mdns, noise, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Swarm,
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const GOSSIP_TOPIC: &str = "rdma-p2p/announce/1.0.0";

const DEFAULT_RELAY_NODES: &[&str] = &[
    "/ip4/147.75.87.27/tcp/4001/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN",
    "/ip4/147.75.195.153/tcp/4001/p2p/QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa",
    "/ip4/104.131.131.82/tcp/4001/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ",
];

/// libp2p TCP port is always UDP port + 10 000.
fn udp_to_libp2p_multiaddr(addr: &str) -> Option<Multiaddr> {
    let (host, port_str) = addr.rsplit_once(':')?;
    let udp_port: u16 = port_str.parse().ok()?;
    let tcp_port = udp_port.checked_add(10000)?;
    format!("/ip4/{}/tcp/{}", host, tcp_port).parse().ok()
}

#[derive(NetworkBehaviour)]
struct DiscBehaviour {
    gossipsub:    gossipsub::Behaviour,
    kademlia:     kad::Behaviour<kad::store::MemoryStore>,
    mdns:         mdns::tokio::Behaviour,
    identify:     identify::Behaviour,
    relay:        relay::client::Behaviour,
    dcutr:        dcutr::Behaviour,
    autonat:      autonat::Behaviour,
    relay_server: relay::Behaviour,
}

pub struct Discovery {
    pub announce_tx:  mpsc::Sender<Vec<u8>>,
    pub dial_tx:      mpsc::Sender<Multiaddr>,
    swarm:            Swarm<DiscBehaviour>,
    topic:            gossipsub::IdentTopic,
    network:          Arc<Network>,
    announce_rx:      mpsc::Receiver<Vec<u8>>,
    dial_rx:          mpsc::Receiver<Multiaddr>,
    relay_peer_addrs: HashSet<Multiaddr>,
    connected_relays: HashSet<PeerId>,
}

impl Discovery {
    pub fn new(
        udp_port: u16,
        bootstrap_addrs: &[String],
        relay_addrs: &[String],
        serve_relay: bool,
        use_default_relays: bool,
        network: Arc<Network>,
    ) -> Result<Self> {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let libp2p_port = udp_port.saturating_add(10000);
        let topic = gossipsub::IdentTopic::new(GOSSIP_TOPIC);

        let mut relay_peer_addrs: HashSet<Multiaddr> = HashSet::new();
        if use_default_relays {
            for addr_str in DEFAULT_RELAY_NODES {
                if let Ok(ma) = addr_str.parse::<Multiaddr>() {
                    relay_peer_addrs.insert(ma);
                }
            }
        }
        for addr_str in relay_addrs {
            if let Ok(ma) = addr_str.parse::<Multiaddr>() {
                relay_peer_addrs.insert(ma);
            }
        }

        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_dns()?
            .with_relay_client(noise::Config::new, yamux::Config::default)?
            .with_behaviour(|key, relay_client| {
                let gossipsub_cfg = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(1))
                    .validation_mode(gossipsub::ValidationMode::Anonymous)
                    .build()
                    .expect("gossipsub config");
                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Anonymous,
                    gossipsub_cfg,
                )
                .expect("gossipsub init");

                let peer_id = key.public().to_peer_id();
                let mut kademlia = kad::Behaviour::new(
                    peer_id,
                    kad::store::MemoryStore::new(peer_id),
                );
                kademlia.set_mode(Some(kad::Mode::Server));

                let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)
                    .expect("mdns init");

                let identify = identify::Behaviour::new(identify::Config::new(
                    "/rdma-p2p/1.0.0".to_string(),
                    key.public(),
                ));

                let relay_server = relay::Behaviour::new(
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
                        relay::Config { max_reservations: 0, ..Default::default() }
                    },
                );

                DiscBehaviour {
                    gossipsub,
                    kademlia,
                    mdns,
                    identify,
                    relay: relay_client,
                    dcutr: dcutr::Behaviour::new(peer_id),
                    autonat: autonat::Behaviour::new(peer_id, autonat::Config::default()),
                    relay_server,
                }
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        let listen_addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{}", libp2p_port).parse()?;
        swarm.listen_on(listen_addr)?;

        swarm.behaviour_mut().gossipsub.subscribe(&topic).ok();

        for addr_str in bootstrap_addrs {
            if let Some(ma) = udp_to_libp2p_multiaddr(addr_str) {
                info!("libp2p: dialling bootstrap {}", ma);
                if let Err(e) = swarm.dial(ma) {
                    warn!("libp2p dial {}: {}", addr_str, e);
                }
            }
        }

        for ma in &relay_peer_addrs {
            info!("libp2p: dialling relay {}", ma);
            swarm.dial(ma.clone()).ok();
        }

        let (announce_tx, announce_rx) = mpsc::channel(64);
        let (dial_tx, dial_rx) = mpsc::channel(64);
        Ok(Discovery {
            announce_tx,
            dial_tx,
            swarm,
            topic,
            network,
            announce_rx,
            dial_rx,
            relay_peer_addrs,
            connected_relays: HashSet::new(),
        })
    }

    pub async fn run(self) {
        let Discovery {
            announce_tx: _,
            dial_tx: _,
            mut swarm,
            topic,
            network,
            mut announce_rx,
            mut dial_rx,
            relay_peer_addrs,
            mut connected_relays,
        } = self;
        loop {
            tokio::select! {
                event = swarm.next() => {
                    let Some(ev) = event else { break };
                    handle_event(&mut swarm, &network, ev, &relay_peer_addrs, &mut connected_relays).await;
                }
                msg = announce_rx.recv() => {
                    let Some(data) = msg else { break };
                    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), data) {
                        debug!("gossipsub publish: {:?}", e);
                    }
                }
                ma = dial_rx.recv() => {
                    let Some(addr) = ma else { break };
                    info!("pkarr: dialling {}", addr);
                    if let Err(e) = swarm.dial(addr) {
                        warn!("pkarr dial: {}", e);
                    }
                }
            }
        }
    }
}

async fn handle_event(
    swarm: &mut Swarm<DiscBehaviour>,
    network: &Network,
    event: SwarmEvent<DiscBehaviourEvent>,
    relay_peer_addrs: &HashSet<Multiaddr>,
    connected_relays: &mut HashSet<PeerId>,
) {
    match event {
        // ── Gossipsub: peer announced itself ────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Gossipsub(gossipsub::Event::Message {
            message, ..
        })) => {
            if let Ok(P2PMessage::Announce {
                node_id,
                pubkey,
                addr,
                model,
                start_layer,
                end_layer,
                total_layers,
                memory_mb,
                timestamp,
                ollama_port,
                inference_port,
            }) = serde_json::from_slice(&message.data)
            {
                network
                    .insert_peer(PeerInfo {
                        node_id,
                        pubkey,
                        addr,
                        model,
                        start_layer,
                        end_layer,
                        total_layers,
                        memory_mb,
                        last_seen: timestamp,
                        ollama_port,
                        inference_port,
                    })
                    .await;
            }
        }

        // ── mDNS: LAN peer found ─────────────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
            for (peer_id, addr) in list {
                info!("🔍 mDNS: {} at {}", peer_id, addr);
                swarm.behaviour_mut().kademlia.add_address(&peer_id, addr.clone());
                if let Err(e) = swarm.dial(addr) {
                    debug!("mDNS dial: {}", e);
                }
            }
        }

        // ── mDNS: LAN peer expired ────────────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
            for (peer_id, addr) in list {
                swarm.behaviour_mut().kademlia.remove_address(&peer_id, &addr);
            }
        }

        // ── Identify: learn remote's addresses; listen on relay circuit ───────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            for addr in info.listen_addrs {
                swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
            }
            if connected_relays.contains(&peer_id) {
                let relay_circuit: Multiaddr =
                    format!("/p2p/{}/p2p-circuit", peer_id).parse().unwrap();
                swarm.listen_on(relay_circuit).ok();
            }
        }

        SwarmEvent::NewListenAddr { address, .. } => {
            info!("🌐 libp2p listening on {}", address);
        }

        SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
            if relay_peer_addrs.contains(endpoint.get_remote_address()) {
                connected_relays.insert(peer_id);
            }
            debug!("libp2p: connected to {}", peer_id);
        }

        SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
            debug!("libp2p: disconnected from {} ({:?})", peer_id, cause);
        }

        // ── Relay reservation accepted ───────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Relay(
            relay::client::Event::ReservationReqAccepted { relay_peer_id, .. },
        )) => {
            info!("🔗 Relay slot reserved on {}", relay_peer_id);
        }

        // ── Relay reservation failed ─────────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Relay(
            relay::client::Event::ReservationReqFailed { relay_peer_id, .. },
        )) => {
            warn!("Relay reservation failed on {}", relay_peer_id);
        }

        // ── DCuTR: hole punch succeeded ──────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Dcutr(
            dcutr::Event::DirectConnectionUpgradeSucceeded { remote_peer_id },
        )) => {
            info!("⚡ Direct connection to {} (hole punch succeeded)", remote_peer_id);
        }

        // ── DCuTR: hole punch failed ─────────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Dcutr(
            dcutr::Event::DirectConnectionUpgradeFailed { remote_peer_id, error, .. },
        )) => {
            warn!("DCuTR failed for {} ({:?}) — relay fallback active", remote_peer_id, error);
        }

        // ── AutoNAT: NAT type detected ───────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Autonat(
            autonat::Event::StatusChanged { new, .. },
        )) => {
            info!("NAT status: {:?}", new);
        }

        _ => {}
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check 2>&1 | grep -E "^error" | head -20`

Expected: no output (no errors).

If there are errors related to relay::Config field names, check libp2p 0.56 relay source:
```bash
find ~/.cargo/registry/src -path "*/libp2p-relay-*/src/behaviour/mod.rs" | head -1 | xargs grep "pub max_"
```
Adjust field names to match.

- [ ] **Step 3: Commit**

```bash
git add src/discovery.rs
git commit -m "feat(discovery): add Circuit Relay v2, DCuTR, AutoNAT for NAT traversal"
```

---

### Task 4: main.rs — new CLI flags + relay peer resolution

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add three new CLI flags to `Args`**

After the `public_addr` field (currently the last field in `Args`), add:

```rust
/// Pkarr keys of relay nodes to use for NAT traversal (comma-separated, format: pk:zbase32...).
#[arg(long, default_value = "")]
relay_peers: String,

/// Run a relay server that accepts circuit connections from other peers (requires public IP).
#[arg(long)]
serve_relay: bool,

/// Disable the built-in public IPFS relay nodes.
#[arg(long)]
no_default_relays: bool,
```

- [ ] **Step 2: Parse relay_peer_keys (after pkarr_peer_keys parsing block)**

After the `let pkarr_peer_keys: Vec<String> = ...` block (around line 338), add:

```rust
let relay_peer_keys: Vec<String> = args
    .relay_peers
    .split(',')
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .collect();
```

- [ ] **Step 3: Resolve relay peers via pkarr before Discovery::new**

After the `bootstrap_nodes` block and before the Ollama port block (around line 340), add:

```rust
// ── Resolve relay peers via pkarr ────────────────────────────────────────
let resolved_relay_addrs: Vec<String> = if let Some(ref pn) = pkarr_node {
    if !relay_peer_keys.is_empty() {
        let peers = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            pn.resolve_peers(&relay_peer_keys),
        )
        .await
        .unwrap_or_default();
        peers
            .into_iter()
            .filter(|p| p.is_relay)
            .map(|p| p.multiaddr)
            .collect()
    } else {
        vec![]
    }
} else {
    vec![]
};
```

- [ ] **Step 4: Update `Discovery::new()` call**

Find the `Discovery::new(args.port, &bootstrap_nodes, net.clone())` call and replace it:

```rust
let (disc_announce_tx, disc_dial_tx) = match Discovery::new(
    args.port,
    &bootstrap_nodes,
    &resolved_relay_addrs,
    args.serve_relay,
    !args.no_default_relays,
    net.clone(),
) {
    Ok(disc) => {
        let ann_tx = disc.announce_tx.clone();
        let dial_tx = disc.dial_tx.clone();
        tokio::spawn(async move { disc.run().await });
        (Some(ann_tx), Some(dial_tx))
    }
    Err(e) => {
        warn!("libp2p discovery disabled ({}). UDP-only mode.", e);
        (None, None)
    }
};
```

- [ ] **Step 5: Capture `serve_relay` for pkarr publish closures**

Before the pkarr publish block (around line 690 — `if let Some(pn) = pkarr_node {`), add:

```rust
let serve_relay = args.serve_relay;
```

- [ ] **Step 6: Update the two pkarr publish calls to pass `serve_relay`**

In the initial-publish spawn (around line 711):
```rust
if let Err(e) = pn2.publish(&addr2, inf_port, &mn2, start_l, end_l, total_l, serve_relay).await {
```

In the 30-min republish loop (around line 730):
```rust
if let Err(e) = pn.publish(&addr, inf_port, &model_name_pk, start_l, end_l, total_l, serve_relay).await {
```

- [ ] **Step 7: Build**

Run: `cargo build --release 2>&1 | grep -E "^error" | head -20`

Expected: no errors, binary produced at `./target/release/rdma-p2p`.

- [ ] **Step 8: Smoke test**

```bash
pkill -f rdma-p2p || true
RUST_LOG=info ./target/release/rdma-p2p --model gemma3 --memory 4GB --port 42777 2>&1 &
sleep 5 && jobs
```

Expected in logs:
- `🌐 libp2p listening on /ip4/0.0.0.0/tcp/52777`
- `libp2p: dialling relay /ip4/147.75.87.27/...` (default relay dials)
- Eventually: `🔗 Relay slot reserved on Qm...` or relay connection attempt logs

Kill the test process:
```bash
pkill -f rdma-p2p
```

- [ ] **Step 9: Commit**

```bash
git add src/main.rs
git commit -m "feat(main): add --relay-peers, --serve-relay, --no-default-relays flags for NAT traversal"
```

---

### Task 5: Integration test — two peers behind NAT

**This task is manual verification steps, no code changes.**

Prerequisites:
- Peer 1 on network A (e.g. home WiFi), Peer 2 on network B (e.g. mobile hotspot)
- Both have the new binary built

- [ ] **Step 1: Start Peer 1**

```bash
# On machine 1 (network A)
RUST_LOG=info ./target/release/rdma-p2p \
    --model gemma3 --memory 4GB --port 42777
```

Note the pkarr address logged: `🔑 Pkarr address: pk:<KEY1>`.

- [ ] **Step 2: Start Peer 2 with Peer 1's pkarr key**

```bash
# On machine 2 (network B)
RUST_LOG=info ./target/release/rdma-p2p \
    --model gemma3 --memory 4GB --port 42777 \
    --pkarr-peers pk:<KEY1>
```

- [ ] **Step 3: Verify relay connection**

Within 30 seconds, both peers should log:

```
🔗 Relay slot reserved on Qm...
```

This means both peers are reachable via the relay.

- [ ] **Step 4: Verify DCuTR hole punch (if NAT is compatible)**

If both NATs are "full cone" or "port restricted cone", look for:

```
⚡ Direct connection to <peer_id> (hole punch succeeded)
```

If NATs are symmetric or hole punch fails:
```
DCuTR failed for <peer_id> (…) — relay fallback active
```

Both outcomes are expected. In either case, gossipsub messages should flow (Peer 2 receives Peer 1's layer announcement).

- [ ] **Step 5: Verify peer table shows Peer 1 on Peer 2**

After ~10 seconds, Peer 2's TUI table should show Peer 1 with correct model and layer info.

- [ ] **Step 6: Commit verification note (optional)**

```bash
git tag -a "dcutr-tested" -m "DCuTR NAT traversal verified across separate networks"
```

---

## Self-Review

**Spec coverage:**
- ✅ DEFAULT_RELAY_NODES hardcoded in discovery.rs
- ✅ --relay-peers, --serve-relay, --no-default-relays flags
- ✅ DiscBehaviour: relay, dcutr, autonat, relay_server fields
- ✅ SwarmBuilder: .with_relay_client() between .with_dns() and .with_behaviour()
- ✅ relay_peer_addrs + connected_relays in Discovery struct
- ✅ ConnectionEstablished → populate connected_relays
- ✅ Identify → listen_on relay circuit
- ✅ Relay reservation accepted/failed events
- ✅ DCuTR succeeded/failed events
- ✅ AutoNAT StatusChanged event
- ✅ relay::Config with max_circuit_bytes=128KB when serve_relay=true
- ✅ relay::Config { max_reservations: 0 } when serve_relay=false
- ✅ TxtPayload r field + ResolvedPeer is_relay
- ✅ is_relay filter when building resolved_relay_addrs

**No placeholders present.**

**Type consistency:** `relay_peer_addrs: HashSet<Multiaddr>` set in `new()`, read in `handle_event()` via `&HashSet<Multiaddr>`. `connected_relays: HashSet<PeerId>` mutated in `ConnectionEstablished`, read in `Identify`. Consistent throughout.
