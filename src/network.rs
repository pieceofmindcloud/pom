use crate::model::LayerAssignment;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

pub const DISCOVERY_PORT: u16 = 42777;
pub const GOSSIP_INTERVAL_SECS: u64 = 5;
pub const PEER_TIMEOUT_SECS: i64 = 30;

/// Message types exchanged between nodes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum P2PMessage {
    /// Node announces itself and its layer assignment
    Announce {
        node_id: String,
        pubkey: String,
        addr: String,
        model: String,
        start_layer: u32,
        end_layer: u32,
        total_layers: u32,
        memory_mb: f64,
        timestamp: DateTime<Utc>,
        /// Port where this node's Ollama is reachable (if any).
        /// Other nodes can build the URL as http://<peer_ip>:<ollama_port>.
        #[serde(default)]
        ollama_port: Option<u16>,
        #[serde(default)]
        inference_port: Option<u16>,
        /// HTTP port serving the API and web UI. `serde(default)` keeps peers
        /// running an older build parseable.
        #[serde(default)]
        gateway_port: Option<u16>,
        #[serde(default)]
        role: String,
        #[serde(default)]
        hostname: String,
        #[serde(default)]
        disk_total_mb: u64,
        #[serde(default)]
        disk_free_mb: u64,
    },
    /// Request peer list from a known node
    PeerRequest { from: String },
    /// Response with known peers
    PeerList { peers: Vec<PeerInfo> },
    /// Heartbeat to signal liveness
    Heartbeat {
        node_id: String,
        timestamp: DateTime<Utc>,
    },
    /// mDNS-style local broadcast
    LocalDiscover { from: String, port: u16 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub node_id: String,
    pub pubkey: String,
    pub addr: String,
    pub model: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    pub memory_mb: f64,
    pub last_seen: DateTime<Utc>,
    #[serde(default)]
    pub ollama_port: Option<u16>,
    /// Port of the llama-cpp-python inference server on this peer.
    /// Used by the gateway pipeline to POST hidden states.
    #[serde(default)]
    pub inference_port: Option<u16>,
    /// Port serving this peer's HTTP API and web UI, so the dashboard can pull
    /// load figures straight from each peer.
    #[serde(default)]
    pub gateway_port: Option<u16>,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub connection_verified: bool,
    #[serde(default)]
    pub disk_total_mb: u64,
    #[serde(default)]
    pub disk_free_mb: u64,
    #[serde(default)]
    pub api_keys_active: u16,
}

/// Extract the host part of an addr string for use in HTTP URLs.
/// IPv4 `"1.2.3.4:port"` → `"1.2.3.4"`, IPv6 `"[::1]:port"` → `"[::1]"`.
fn ip_host(addr: &str) -> &str {
    if addr.starts_with('[') {
        // "[::1]:port" — keep the brackets so the URL is valid
        match addr.find(']') {
            Some(i) => &addr[..=i],
            None => "127.0.0.1",
        }
    } else {
        addr.split(':').next().unwrap_or("127.0.0.1")
    }
}

impl PeerInfo {
    pub fn layer_range(&self) -> String {
        format!("{}-{}", self.start_layer, self.end_layer)
    }

    pub fn layers_count(&self) -> u32 {
        self.end_layer - self.start_layer + 1
    }

    pub fn is_alive(&self) -> bool {
        let now = Utc::now();
        let diff = now.signed_duration_since(self.last_seen);
        diff.num_seconds() < PEER_TIMEOUT_SECS
    }

    /// Full Ollama HTTP URL for this peer (None if no ollama_port advertised).
    pub fn ollama_url(&self) -> Option<String> {
        self.ollama_port.map(|port| {
            format!("http://{}:{}", ip_host(&self.addr), port)
        })
    }

    /// Full inference server URL for pipeline calls (None if not running).
    pub fn inference_url(&self) -> Option<String> {
        self.inference_port.map(|port| {
            format!("http://{}:{}", ip_host(&self.addr), port)
        })
    }

    /// Base URL of this peer's HTTP API / web UI.
    pub fn gateway_url(&self) -> Option<String> {
        self.gateway_port
            .map(|port| format!("http://{}:{}", ip_host(&self.addr), port))
    }
}

pub struct Network {
    pub node_id: String,
    pub pubkey: String,
    pub bind_addr: SocketAddr,
    pub peers: Arc<RwLock<HashMap<String, PeerInfo>>>,
    pub bootstrap_nodes: Vec<String>,
    /// Port of the local Ollama instance, advertised in P2P messages.
    pub ollama_port: Option<u16>,
    /// Port of the local llama-cpp-python inference server.
    pub inference_port: Option<u16>,
    /// Port of this node's HTTP API / web UI.
    pub gateway_port: Option<u16>,
    /// Advertised in every `Announce` so peers can name this machine without a
    /// second HTTP round-trip.
    pub role: String,
    pub hostname: String,
    socket: Arc<UdpSocket>,
}

impl Network {
    pub async fn new(
        node_id: String,
        pubkey: String,
        port: u16,
        bootstrap_nodes: Vec<String>,
        ollama_port: Option<u16>,
        inference_port: Option<u16>,
        gateway_port: Option<u16>,
    ) -> Result<Self> {
        // Bind dual-stack IPv6 so peers on IPv6 (including link-local fe80::) can be reached.
        // Falls back to IPv4-only if the OS has IPv6 disabled.
        let (socket, bind_addr) = {
            let v6: SocketAddr = format!("[::]:{}",  port).parse()?;
            match UdpSocket::bind(v6).await {
                Ok(s) => {
                    let _ = s.set_broadcast(true); // no-op on pure IPv6, ignored
                    info!("UDP socket bound to [::]:{} (dual-stack)", port);
                    (s, v6)
                }
                Err(e) => {
                    info!("{}",e);
                    let v4: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
                    let s = UdpSocket::bind(v4).await?;
                    s.set_broadcast(true)?;
                    info!("UDP socket bound to 0.0.0.0:{} (IPv4)", port);
                    (s, v4)
                }
            }
        };

        Ok(Network {
            node_id,
            pubkey,
            bind_addr,
            peers: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_nodes,
            ollama_port,
            inference_port,
            gateway_port,
            role: String::new(),
            hostname: String::new(),
            socket: Arc::new(socket),
        })
    }

    /// Label this node carries in its announces.
    pub fn with_identity(mut self, role: String, hostname: String) -> Self {
        self.role = role;
        self.hostname = hostname;
        self
    }

    /// Put a destination in the address family of our socket.
    ///
    /// The socket is bound dual-stack (`[::]`), and macOS refuses a `sockaddr_in`
    /// destination on an `AF_INET6` socket with EINVAL — every send to an IPv4
    /// peer failed, silently killing discovery in one direction. Linux happens to
    /// accept it, which is why the breakage looked one-sided. Mapping the address
    /// to `::ffff:a.b.c.d` works on both, for unicast and for broadcast.
    fn normalize(&self, addr: SocketAddr) -> SocketAddr {
        match (self.bind_addr, addr) {
            (SocketAddr::V6(_), SocketAddr::V4(v4)) => {
                SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port())
            }
            _ => addr,
        }
    }

    async fn send_bytes(&self, data: &[u8], addr: &str) -> std::io::Result<()> {
        let resolved = tokio::net::lookup_host(addr)
            .await?
            .next()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no address"))?;
        self.socket.send_to(data, self.normalize(resolved)).await?;
        Ok(())
    }

    /// Send a message to a specific address
    pub async fn send_to(&self, msg: &P2PMessage, addr: &str) -> Result<()> {
        let data = serde_json::to_vec(msg)?;
        match self.send_bytes(&data, addr).await {
            Ok(_) => debug!("Sent {:?} to {}", std::mem::discriminant(msg), addr),
            Err(e) => warn!("Failed to send to {}: {}", addr, e),
        }
        Ok(())
    }

    /// Broadcast to local network (only nodes on DISCOVERY_PORT receive this)
    pub async fn broadcast_local(&self, msg: &P2PMessage) -> Result<()> {
        let data = serde_json::to_vec(msg)?;
        let broadcast_addr = format!("255.255.255.255:{}", DISCOVERY_PORT);
        if let Err(e) = self.send_bytes(&data, &broadcast_addr).await {
            debug!("LAN broadcast failed: {e}");
        }
        Ok(())
    }

    /// Build this node's `Announce`.
    ///
    /// A node configured through the web UI has no `LayerAssignment` until a
    /// model is bound to it. It must still announce itself, otherwise it is
    /// invisible to the rest of the cluster — it just announces zero layers.
    fn self_announce(&self, assignment: Option<&LayerAssignment>) -> P2PMessage {
        P2PMessage::Announce {
            node_id: self.node_id.clone(),
            pubkey: self.pubkey.clone(),
            addr: format!("0.0.0.0:{}", self.bind_addr.port()),
            model: assignment.map(|a| a.model.clone()).unwrap_or_default(),
            start_layer: assignment.map(|a| a.start_layer).unwrap_or(0),
            end_layer: assignment.map(|a| a.end_layer).unwrap_or(0),
            total_layers: assignment.map(|a| a.total_layers).unwrap_or(0),
            memory_mb: assignment.map(|a| a.memory_used_mb).unwrap_or(0.0),
            timestamp: Utc::now(),
            ollama_port: self.ollama_port,
            inference_port: self.inference_port,
            gateway_port: self.gateway_port,
            role: self.role.clone(),
            hostname: self.hostname.clone(),
            disk_total_mb: 0,
            disk_free_mb: 0,
        }
    }

    /// Announce this node's existence and layer assignment to the network
    pub async fn announce(&self, assignment: Option<&LayerAssignment>) -> Result<()> {
        let msg = self.self_announce(assignment);

        // Announce to bootstrap nodes
        for bootstrap in &self.bootstrap_nodes {
            self.send_to(&msg, bootstrap).await?;
        }

        // Announce to known peers
        let peers = self.peers.read().await;
        for peer in peers.values() {
            if peer.is_alive() {
                self.send_to(&msg, &peer.addr).await?;
            }
        }

        // Broadcast locally
        self.broadcast_local(&msg).await?;

        Ok(())
    }

    /// Request peer list from bootstrap nodes
    pub async fn bootstrap_discovery(&self) -> Result<()> {
        let msg = P2PMessage::PeerRequest {
            from: self.node_id.clone(),
        };
        for bootstrap in &self.bootstrap_nodes {
            info!("Contacting bootstrap node: {}", bootstrap);
            self.send_to(&msg, bootstrap).await?;
        }
        Ok(())
    }

    /// Get all covered layers in the network (by other peers)
    pub async fn get_covered_layers(&self, model: &str) -> Vec<u32> {
        let peers = self.peers.read().await;
        let mut covered = Vec::new();
        for peer in peers.values() {
            if peer.is_alive() && peer.model == model {
                for layer in peer.start_layer..=peer.end_layer {
                    if !covered.contains(&layer) {
                        covered.push(layer);
                    }
                }
            }
        }
        covered.sort();
        covered
    }

    /// Start background tasks: gossip, heartbeat, receive loop
    pub async fn start(
        self: Arc<Self>,
        assignment: Arc<RwLock<Option<LayerAssignment>>>,
    ) -> Result<()> {
        // Receive loop
        let net_recv = self.clone();
        let assignment_recv = assignment.clone();
        tokio::spawn(async move {
            net_recv.receive_loop(assignment_recv).await;
        });

        // Gossip / heartbeat loop
        let net_gossip = self.clone();
        let assignment_gossip = assignment.clone();
        tokio::spawn(async move {
            let mut ticker = interval(Duration::from_secs(GOSSIP_INTERVAL_SECS));
            loop {
                ticker.tick().await;
                // Announce unconditionally: a node without a model still has to
                // be discoverable, otherwise a freshly configured cluster is
                // made of nodes that only listen and never speak.
                let asgn = assignment_gossip.read().await.clone();
                let _ = net_gossip.announce(asgn.as_ref()).await;
                net_gossip.cleanup_dead_peers().await;
            }
        });

        // Reach out to the configured bootstrap nodes right away. This belongs
        // here, not in the caller: every start path needs it, and the
        // gateway-only path used to skip it.
        self.bootstrap_discovery().await?;

        Ok(())
    }

    async fn receive_loop(&self, assignment: Arc<RwLock<Option<LayerAssignment>>>) {
        let mut buf = vec![0u8; 65535];
        loop {
            match self.socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let data = &buf[..len];
                    if let Ok(msg) = serde_json::from_slice::<P2PMessage>(data) {
                        self.handle_message(msg, src, &assignment).await;
                    }
                }
                Err(e) => warn!("UDP recv error: {}", e),
            }
        }
    }

    async fn handle_message(
        &self,
        msg: P2PMessage,
        src: SocketAddr,
        assignment: &Arc<RwLock<Option<LayerAssignment>>>,
    ) {
        match msg {
            P2PMessage::Announce {
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
                gateway_port,
                role,
                hostname,
                disk_total_mb,
                disk_free_mb,
            } => {
                if node_id == self.node_id {
                    return; // ignore ourselves
                }

                let peer_addr = if addr.starts_with("0.0.0.0") {
                    // to_canonical() converts IPv4-mapped IPv6 (::ffff:x.x.x.x) back to IPv4
                    match src.ip().to_canonical() {
                        IpAddr::V4(v4) => format!("{}:{}", v4, src.port()),
                        IpAddr::V6(v6) => format!("[{}]:{}", v6, src.port()),
                    }
                } else {
                    addr.clone()
                };

                let peer = PeerInfo {
                    node_id: node_id.clone(),
                    pubkey,
                    addr: peer_addr,
                    model: model.clone(),
                    start_layer,
                    end_layer,
                    total_layers,
                    memory_mb,
                    last_seen: timestamp,
                    ollama_port,
                    inference_port,
                    gateway_port,
                    role,
                    hostname,
                    connection_verified: false,
                    disk_total_mb,
                    disk_free_mb,
                    api_keys_active: 0,
                };

                let is_new = {
                    let peers = self.peers.read().await;
                    !peers.contains_key(&node_id)
                };

                if is_new {
                    let ollama_tag = ollama_port
                        .map(|p| format!(" ollama:{}", p))
                        .unwrap_or_default();
                    info!(
                        "🆕 New peer: {} | model={} layers={}-{}{}",
                        &node_id[..12],
                        model,
                        start_layer,
                        end_layer,
                        ollama_tag,
                    );
                } else {
                    debug!("Peer update: {} layers={}-{}", &node_id[..12], start_layer, end_layer);
                }

                let mut peers = self.peers.write().await;
                peers.insert(node_id, peer);
            }

            P2PMessage::PeerRequest { from } => {
                if from == self.node_id {
                    return;
                }

                // Immediately send our own Announce so the requester learns about
                // us without waiting for the next gossip cycle. This happens even
                // with no assignment — see `self_announce`.
                {
                    let asgn = assignment.read().await.clone();
                    let announce = self.self_announce(asgn.as_ref());
                    let _ = self.send_to(&announce, &src.to_string()).await;
                }

                // Also send the list of peers we know about
                let peer_list: Vec<PeerInfo> = {
                    let peers = self.peers.read().await;
                    peers.values().filter(|p| p.is_alive()).cloned().collect()
                };
                let reply = P2PMessage::PeerList { peers: peer_list };
                let _ = self.send_to(&reply, &src.to_string()).await;
            }

            P2PMessage::PeerList { peers } => {
                let mut known = self.peers.write().await;
                for peer in peers {
                    if peer.node_id != self.node_id && !known.contains_key(&peer.node_id) {
                        info!(
                            "📋 Learned peer from gossip: {} layers={}-{}",
                            &peer.node_id[..12],
                            peer.start_layer,
                            peer.end_layer
                        );
                        known.insert(peer.node_id.clone(), peer);
                    }
                }
            }

            P2PMessage::Heartbeat { node_id, timestamp } => {
                let mut peers = self.peers.write().await;
                if let Some(peer) = peers.get_mut(&node_id) {
                    peer.last_seen = timestamp;
                }
            }

            P2PMessage::LocalDiscover { from, port } => {
                if from != self.node_id {
                    let reply_addr = match src.ip() {
                        IpAddr::V4(v4) => format!("{}:{}", v4, port),
                        IpAddr::V6(v6) => format!("[{}]:{}", v6, port),
                    };
                    info!("📡 Local discovery from {} — responding", src.ip());
                    let req = P2PMessage::PeerRequest {
                        from: self.node_id.clone(),
                    };
                    let _ = self.send_to(&req, &reply_addr).await;
                }
            }
        }
    }

    async fn cleanup_dead_peers(&self) {
        let mut peers = self.peers.write().await;
        let before = peers.len();
        peers.retain(|_, p| p.is_alive());
        let removed = before - peers.len();
        if removed > 0 {
            info!("🧹 Removed {} timed-out peers", removed);
        }
    }

    /// Insert or update a peer discovered via libp2p (gossipsub / mDNS).
    pub async fn insert_peer(&self, peer: PeerInfo) {
        let node_id = peer.node_id.clone();
        if node_id == self.node_id {
            return;
        }
        let is_new = !self.peers.read().await.contains_key(&node_id);
        if is_new {
            info!(
                "🆕 Peer via libp2p: {} model={} layers={}-{}",
                &node_id[..node_id.len().min(12)],
                peer.model,
                peer.start_layer,
                peer.end_layer
            );
        }
        self.peers.write().await.insert(node_id, peer);
    }

    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.len()
    }

    pub async fn alive_peers(&self) -> Vec<PeerInfo> {
        self.peers
            .read()
            .await
            .values()
            .filter(|p| p.is_alive())
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sending an IPv4 destination on the dual-stack socket fails with EINVAL on
    /// macOS, so IPv4 destinations must be mapped. Regressing this kills peer
    /// discovery on one side only, which is very hard to spot.
    #[tokio::test]
    async fn ipv4_destinations_are_mapped_on_a_dual_stack_socket() {
        let net = Network::new(
            "n".into(), "p".into(), 0, vec![], None, None, None,
        )
        .await
        .unwrap();

        let v4: SocketAddr = "192.168.100.138:42777".parse().unwrap();
        let out = net.normalize(v4);

        if matches!(net.bind_addr, SocketAddr::V6(_)) {
            assert_eq!(out, "[::ffff:192.168.100.138]:42777".parse().unwrap());
        } else {
            assert_eq!(out, v4, "IPv4-only socket must keep the address as-is");
        }
    }

    #[tokio::test]
    async fn ipv6_destinations_are_left_alone() {
        let net = Network::new(
            "n".into(), "p".into(), 0, vec![], None, None, None,
        )
        .await
        .unwrap();
        let v6: SocketAddr = "[fe80::1]:42777".parse().unwrap();
        assert_eq!(net.normalize(v6), v6);
    }
}
