# Pkarr Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add pkarr-based peer discovery so rdma-p2p nodes find each other over the internet via Mainline DHT without IP-based bootstrap.

**Architecture:** A new `pkarr_node` module wraps the pkarr 6.0.0 client. On startup, the node loads (or generates) a stable Ed25519 keypair from `.rdma-p2p.key`, publishes its address as a TXT DNS record under `_rdmap2p` to the DHT, resolves any `--pkarr-peers` keys to multiaddrs, and dials them via the existing libp2p `Discovery` channel. A background task republishes every 30 minutes to keep the record alive.

**Tech Stack:** Rust/Tokio, pkarr 6.0.0 (crate already in Cargo.toml), libp2p 0.56, serde_json

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `src/pkarr_node.rs` | **Create** | `PkarrNode` struct, publish/resolve, `ResolvedPeer` |
| `src/discovery.rs` | **Modify** | Expose `dial_tx: mpsc::Sender<Multiaddr>` + handle in `run()` |
| `src/main.rs` | **Modify** | CLI flags, PkarrNode startup, peer resolution, republish task |
| `.gitignore` | **Modify** | Add `.rdma-p2p.key` |

---

## Task 1: Create `src/pkarr_node.rs`

**Files:**
- Create: `src/pkarr_node.rs`

- [ ] **Step 1: Write the failing test**

Add to end of `src/pkarr_node.rs` (file doesn't exist yet — write the whole file with the test module):

```rust
use anyhow::Result;
use pkarr::{Client, Keypair, PublicKey, SignedPacket};
use serde::{Deserialize, Serialize};
use std::net::UdpSocket;
use std::path::Path;
use tracing::{info, warn};

pub struct PkarrNode {
    client: Client,
    keypair: Keypair,
}

#[derive(Debug, Clone)]
pub struct ResolvedPeer {
    pub multiaddr: String,
    pub inference_port: Option<u16>,
    pub model: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
}

#[derive(Serialize, Deserialize)]
struct TxtPayload {
    a: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    i: Option<u16>,
    m: String,
    s: u32,
    e: u32,
    t: u32,
}

impl PkarrNode {
    pub fn load_or_create(key_path: &Path) -> Result<Self> {
        let keypair = match Keypair::from_secret_key_file(key_path) {
            Ok(kp) => {
                info!("Loaded pkarr identity from {}", key_path.display());
                kp
            }
            Err(_) => {
                let kp = Keypair::random();
                kp.write_secret_key_file(key_path)?;
                info!("Generated new pkarr identity → {}", key_path.display());
                kp
            }
        };
        let client = Client::builder().build()?;
        Ok(PkarrNode { client, keypair })
    }

    pub fn public_key_uri(&self) -> String {
        self.keypair.to_uri_string()
    }

    pub async fn publish(
        &self,
        addr: &str,
        inference_port: Option<u16>,
        model: &str,
        start_layer: u32,
        end_layer: u32,
        total_layers: u32,
    ) -> Result<()> {
        let payload = TxtPayload {
            a: addr.to_string(),
            i: inference_port,
            m: model.to_string(),
            s: start_layer,
            e: end_layer,
            t: total_layers,
        };
        let json = serde_json::to_string(&payload)?;
        let sp = SignedPacket::builder()
            .txt("_rdmap2p".try_into()?, json.as_str().try_into()?, 300)
            .sign(&self.keypair)?;
        self.client.publish(&sp, None).await?;
        info!("Published pkarr record: {}", json);
        Ok(())
    }

    pub async fn resolve_peers(&self, pubkeys: &[String]) -> Vec<ResolvedPeer> {
        let mut peers = Vec::new();
        for key_str in pubkeys {
            let pk: PublicKey = match key_str.as_str().try_into() {
                Ok(pk) => pk,
                Err(e) => {
                    warn!("Invalid pkarr key '{}': {}", key_str, e);
                    continue;
                }
            };
            match self.client.resolve(&pk).await {
                Some(sp) => {
                    if let Some(peer) = parse_signed_packet(&sp) {
                        info!("Resolved pkarr peer {}: {}", key_str, peer.multiaddr);
                        peers.push(peer);
                    } else {
                        warn!("No _rdmap2p TXT record for {}", key_str);
                    }
                }
                None => warn!("Could not resolve pkarr key {}", key_str),
            }
        }
        peers
    }
}

fn parse_signed_packet(sp: &SignedPacket) -> Option<ResolvedPeer> {
    for rr in sp.resource_records("_rdmap2p") {
        if let pkarr::dns::rdata::RData::TXT(txt) = &rr.rdata {
            if let Ok(s) = String::try_from(txt.clone()) {
                if let Ok(p) = serde_json::from_str::<TxtPayload>(&s) {
                    return Some(ResolvedPeer {
                        multiaddr: p.a,
                        inference_port: p.i,
                        model: p.m,
                        start_layer: p.s,
                        end_layer: p.e,
                        total_layers: p.t,
                    });
                }
            }
        }
    }
    None
}

/// Detect outbound IP via a no-data UDP trick (connect does not send packets).
pub fn detect_local_ip() -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_txt_payload() {
        let p = TxtPayload {
            a: "/ip4/1.2.3.4/tcp/52777".to_string(),
            i: Some(8181),
            m: "qwen3.6".to_string(),
            s: 0,
            e: 19,
            t: 39,
        };
        let json = serde_json::to_string(&p).unwrap();
        let p2: TxtPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p.a, p2.a);
        assert_eq!(p.i, p2.i);
        assert_eq!(p.s, p2.s);
        assert_eq!(p.t, p2.t);
    }

    #[test]
    fn roundtrip_no_inference_port() {
        let p = TxtPayload {
            a: "/ip4/10.0.0.1/tcp/52777".to_string(),
            i: None,
            m: "gemma3".to_string(),
            s: 5,
            e: 15,
            t: 28,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("\"i\""), "i field should be omitted when None");
        let p2: TxtPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(p2.i, None);
    }

    #[test]
    fn detect_local_ip_returns_something() {
        // This test requires network access (no packets sent, just binding).
        // It will fail in fully offline environments — that's expected.
        let ip = detect_local_ip();
        assert!(ip.is_some(), "should detect local IP");
        let ip = ip.unwrap();
        assert!(!ip.is_empty());
        assert!(ip.contains('.'), "should be IPv4");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd /home/pereirajair/.openclaw/workspace/rdma-p2p
cargo test pkarr_node 2>&1 | head -30
```

Expected: compilation error — `pkarr_node` module not in `main.rs` yet, or compile errors in the new file. That's fine — this step just confirms the test skeleton is wrong before we wire it up.

- [ ] **Step 3: Add `mod pkarr_node;` to `main.rs`**

In `src/main.rs` add after the existing `mod` declarations (line ~9):

```rust
mod pkarr_node;
```

After the `mod pipeline;` line.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test pkarr_node 2>&1
```

Expected output (3 tests):
```
test pkarr_node::tests::roundtrip_txt_payload ... ok
test pkarr_node::tests::roundtrip_no_inference_port ... ok
test pkarr_node::tests::detect_local_ip_returns_something ... ok
```

- [ ] **Step 5: Commit**

```bash
git add src/pkarr_node.rs src/main.rs
git commit -m "feat: add pkarr_node module with PkarrNode publish/resolve"
```

---

## Task 2: Add `dial_tx` channel to `src/discovery.rs`

**Files:**
- Modify: `src/discovery.rs`

- [ ] **Step 1: Add `dial_tx` and `dial_rx` fields to `Discovery` struct**

In `src/discovery.rs`, change the `Discovery` struct (around line 33):

```rust
pub struct Discovery {
    /// Send serialised `P2PMessage::Announce` bytes here to publish them via gossipsub.
    pub announce_tx: mpsc::Sender<Vec<u8>>,
    /// Send a multiaddr here to have the swarm dial it.
    pub dial_tx: mpsc::Sender<libp2p::Multiaddr>,
    swarm: Swarm<DiscBehaviour>,
    topic: gossipsub::IdentTopic,
    network: Arc<Network>,
    announce_rx: mpsc::Receiver<Vec<u8>>,
    dial_rx: mpsc::Receiver<libp2p::Multiaddr>,
}
```

- [ ] **Step 2: Create the dial channel in `Discovery::new()`**

In `Discovery::new()`, at the end (around line 106), replace:

```rust
        let (announce_tx, announce_rx) = mpsc::channel(64);
        Ok(Discovery { announce_tx, swarm, topic, network, announce_rx })
```

with:

```rust
        let (announce_tx, announce_rx) = mpsc::channel(64);
        let (dial_tx, dial_rx) = mpsc::channel(64);
        Ok(Discovery { announce_tx, dial_tx, swarm, topic, network, announce_rx, dial_rx })
```

- [ ] **Step 3: Handle `dial_rx` in `Discovery::run()`**

In `run()`, change the destructuring and the select loop (lines ~110-126):

```rust
    pub async fn run(self) {
        let Discovery { announce_tx: _, dial_tx: _, mut swarm, topic, network, mut announce_rx, mut dial_rx } = self;
        loop {
            tokio::select! {
                event = swarm.next() => {
                    let Some(ev) = event else { break };
                    handle_event(&mut swarm, &network, ev).await;
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
```

- [ ] **Step 4: Verify it compiles**

```bash
cargo build 2>&1 | grep -E "^error"
```

Expected: no output (clean compile).

- [ ] **Step 5: Commit**

```bash
git add src/discovery.rs
git commit -m "feat(discovery): expose dial_tx channel for pkarr-resolved peer dialling"
```

---

## Task 3: Wire pkarr into `src/main.rs`

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Add CLI flags to `Args`**

In the `Args` struct (after the `single_peer` field, around line 129), add:

```rust
    /// Pkarr public keys of known peers to dial on startup (comma-separated, format: pk:zbase32...).
    #[arg(long, default_value = "")]
    pkarr_peers: String,

    /// External address to publish to pkarr DHT (HOST:PORT). Auto-detected if not set.
    /// PORT here is the UDP port; libp2p TCP port will be UDP+10000.
    #[arg(long, default_value = "")]
    public_addr: String,
```

- [ ] **Step 2: Add the pkarr startup block after identity generation**

In `main()`, find the comment `// ── Generate identity ──` (around line 287). After the three existing identity lines:

```rust
    let identity = identity::Identity::generate();
    let node_id = identity.node_id.0.clone();
    let pubkey = identity.public_key_hex();
```

Add:

```rust
    // ── Pkarr identity (stable across restarts) ──────────────────────────────
    let key_path = std::path::Path::new(".rdma-p2p.key");
    let pkarr_node = match pkarr_node::PkarrNode::load_or_create(key_path) {
        Ok(n) => {
            info!("🔑 Pkarr address: {}", n.public_key_uri());
            Some(n)
        }
        Err(e) => {
            warn!("pkarr init failed (discovery disabled): {}", e);
            None
        }
    };
```

- [ ] **Step 3: Compute the publishable libp2p multiaddr**

After the pkarr_node block (still in `main()`, before the bootstrap nodes section), add:

```rust
    // ── Compute pkarr multiaddr to publish ───────────────────────────────────
    let pkarr_multiaddr = {
        let tcp_port = args.port.saturating_add(10000);
        if !args.public_addr.is_empty() {
            // User-supplied HOST:PORT (PORT is UDP port, we derive TCP)
            if let Some((host, _)) = args.public_addr.split_once(':') {
                format!("/ip4/{}/tcp/{}", host, tcp_port)
            } else {
                format!("/ip4/{}/tcp/{}", args.public_addr, tcp_port)
            }
        } else {
            let ip = pkarr_node::detect_local_ip().unwrap_or_else(|| "0.0.0.0".to_string());
            format!("/ip4/{}/tcp/{}", ip, tcp_port)
        }
    };
```

- [ ] **Step 4: Parse `--pkarr-peers` into a Vec**

Right after the `pkarr_multiaddr` block, add:

```rust
    let pkarr_peer_keys: Vec<String> = args
        .pkarr_peers
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
```

- [ ] **Step 5: Resolve pkarr peers and dial them after Discovery starts**

Find the section that starts libp2p discovery (around line 583):

```rust
    let disc_announce_tx = match Discovery::new(args.port, &bootstrap_nodes, net.clone()) {
```

Change it to capture `dial_tx` as well:

```rust
    let (disc_announce_tx, disc_dial_tx) = match Discovery::new(args.port, &bootstrap_nodes, net.clone()) {
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

Then, after the gossipsub publish task (after the `if let Some(tx) = disc_announce_tx { ... }` block, still before the display loop), add:

```rust
    // ── Resolve pkarr peers and dial on startup ──────────────────────────────
    if let (Some(ref pn), Some(ref dtx)) = (&pkarr_node, disc_dial_tx.as_ref()) {
        if !pkarr_peer_keys.is_empty() {
            let peers = pn.resolve_peers(&pkarr_peer_keys).await;
            for peer in &peers {
                if let Ok(ma) = peer.multiaddr.parse::<libp2p::Multiaddr>() {
                    let _ = dtx.send(ma).await;
                }
            }
        }
    }

    // ── Publish our pkarr record and schedule 30-min republish ──────────────
    if let Some(pn) = pkarr_node {
        let pn = std::sync::Arc::new(pn);
        let addr = pkarr_multiaddr.clone();
        let inf_port = args.inference_port;
        let model_name_pk = profile.name.clone();
        let asgn_pk = assignment_shared.clone();

        // Initial publish (best-effort — don't block startup)
        {
            let pn2 = pn.clone();
            let addr2 = addr.clone();
            let mn2 = model_name_pk.clone();
            let asgn2 = asgn_pk.clone();
            tokio::spawn(async move {
                let a = asgn2.read().await;
                if let Some(ref asgn) = *a {
                    if let Err(e) = pn2.publish(&addr2, inf_port, &mn2,
                            asgn.start_layer, asgn.end_layer, asgn.total_layers).await {
                        warn!("pkarr initial publish failed: {}", e);
                    }
                }
            });
        }

        // Republish every 30 minutes
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
            ticker.tick().await; // first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                let a = asgn_pk.read().await;
                if let Some(ref asgn) = *a {
                    if let Err(e) = pn.publish(&addr, inf_port, &model_name_pk,
                            asgn.start_layer, asgn.end_layer, asgn.total_layers).await {
                        warn!("pkarr republish failed: {}", e);
                    }
                }
            }
        });
    }
```

- [ ] **Step 6: Add `use libp2p::Multiaddr;` if not already in scope**

At the top of `main.rs`, inside the `use` block, check if `libp2p::Multiaddr` is used directly. It's referenced in the pkarr dial block — add it to the existing imports after `use discovery::Discovery;`:

```rust
use libp2p::Multiaddr;
```

- [ ] **Step 7: Verify full build**

```bash
cargo build --release 2>&1 | grep -E "^error"
```

Expected: no output (clean compile).

- [ ] **Step 8: Commit**

```bash
git add src/main.rs
git commit -m "feat(main): add --pkarr-peers/--public-addr, wire PkarrNode publish and dial"
```

---

## Task 4: Update `.gitignore`

**Files:**
- Modify: `.gitignore`

- [ ] **Step 1: Add `.rdma-p2p.key` to `.gitignore`**

Append to `.gitignore`:

```
.rdma-p2p.key
```

Current `.gitignore` content:
```
target*
logs*
models*
venv*
__pycache__*
*.log
```

New content:
```
target*
logs*
models*
venv*
__pycache__*
*.log
.rdma-p2p.key
```

- [ ] **Step 2: Commit**

```bash
git add .gitignore
git commit -m "chore: add .rdma-p2p.key to .gitignore"
```

---

## Task 5: Manual Integration Test

- [ ] **Step 1: Build release binary**

```bash
cargo build --release 2>&1 | tail -5
```

Expected: `Finished release [optimized] target(s) in ...`

- [ ] **Step 2: Run node 1 and check pkarr address logs**

```bash
./target/release/rdma-p2p --model gemma3 --memory 4GB --port 42777 --inference-port 8180 2>&1 | grep -E "Pkarr|pkarr" | head -5
```

Expected log line:
```
🔑 Pkarr address: pk:yqnm8dkk...
```

Copy the `pk:...` address.

- [ ] **Step 3: Run node 2 with node 1's pkarr address**

On a second terminal (or second machine), substituting the actual `pk:...` from step 2:

```bash
./target/release/rdma-p2p --model gemma3 --memory 4GB --port 42778 \
    --pkarr-peers pk:REPLACE_WITH_NODE1_KEY 2>&1 | grep -E "Resolved|dialling|pkarr"
```

Expected log lines:
```
Resolved pkarr peer pk:...: /ip4/HOST/tcp/52777
pkarr: dialling /ip4/HOST/tcp/52777
```

- [ ] **Step 4: Verify gossipsub propagation**

After ~10s, node 2 should show node 1 in its peers table. Check:

```bash
curl -s localhost:8081/peers | jq .
```

Expected: JSON array containing node 1's entry.

- [ ] **Step 5: Verify `.rdma-p2p.key` is created and not tracked by git**

```bash
ls -la .rdma-p2p.key
git status .rdma-p2p.key
```

Expected:
- File exists with permissions `600`
- `git status` shows it as `ignored`

---

## Notes

- **NAT**: Nodes behind NAT without port-forwarding will publish their LAN IP. They can still connect OUT to nodes with public IPs, but won't be reachable from the internet. Use `--public-addr <external-ip>:<udp-port>` on nodes with a forwarded port.
- **pkarr record expiry**: pkarr records expire after ~2 hours without a refresh. The 30-minute republish task keeps them alive indefinitely.
- **First publish delay**: The initial publish happens asynchronously so it doesn't block the rest of startup. If you need to immediately share your `pk:...` with a peer, wait ~5s for the publish to propagate.
- **Module naming**: The module is `pkarr_node` (not `pkarr`) to avoid shadowing the `pkarr` crate name in the Rust namespace.
