use crate::infer_tunnel::{self, InferTunnelCodec, TunnelOutbound};
use crate::network::{Network, P2PMessage, PeerInfo};
use crate::wire;
use anyhow::Result;
use futures::StreamExt;
use libp2p::{
    autonat, dcutr, gossipsub, identify, kad, mdns, noise, relay,
    request_response::{self, OutboundRequestId},
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, PeerId, Swarm,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

const GOSSIP_TOPIC: &str = "pom/announce/1.0.0";

const DEFAULT_RELAY_NODES: &[&str] = &[
    "/ip4/147.75.87.27/tcp/4001/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN",
    "/ip4/147.75.195.153/tcp/4001/p2p/QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa",
    "/ip4/104.131.131.82/tcp/4001/p2p/QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ",
];

/// Split `host:port`, including the bracketed IPv6 form.
///
/// A bare IPv6 address is nothing but colons, so `rsplit_once(':')` picks a
/// piece of the address as the port. The brackets are what disambiguate it, and
/// they are also what the operator types.
fn split_host_port(addr: &str) -> Option<(&str, &str)> {
    if let Some(rest) = addr.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        Some((host, after.strip_prefix(':')?))
    } else {
        addr.rsplit_once(':')
    }
}

/// libp2p TCP port is always UDP port + 10 000.
///
/// Returned `None` for every IPv6 bootstrap before: the address was pasted into
/// an `/ip4/` multiaddr, which never parses, so the dial was dropped without a
/// word in the log.
fn udp_to_libp2p_multiaddr(addr: &str) -> Option<Multiaddr> {
    use std::net::IpAddr;

    let (host, port_str) = split_host_port(addr)?;
    let udp_port: u16 = port_str.parse().ok()?;
    let tcp_port = udp_port.checked_add(10000)?;

    let ma = match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => format!("/ip4/{v4}/tcp/{tcp_port}"),
        Ok(IpAddr::V6(v6)) => format!("/ip6/{v6}/tcp/{tcp_port}"),
        // A hostname: libp2p resolves it, the swarm is built `.with_dns()`.
        Err(_) => format!("/dns/{host}/tcp/{tcp_port}"),
    };
    ma.parse().ok()
}

/// Inverse of `udp_to_libp2p_multiaddr`: extract this node's own `host:port`
/// (UDP-port convention) from a libp2p `identify` `observed_addr` — the
/// address a remote peer told us it saw *us* connecting from, which is the
/// cheapest reliable "what's my public IP" signal available without a
/// dedicated STUN dependency (it's the same principle STUN itself uses).
///
/// `None` for anything that isn't a plain `/ip4|ip6/.../tcp/<port>` (a
/// relay-circuit or QUIC observed address isn't a `host:port` a peer could
/// dial for HTTP anyway) or whose host is still private/loopback/link-local
/// — that's not new information, just a peer on the same LAN or a NAT
/// rewriting within another private range.
fn observed_host_port(addr: &Multiaddr) -> Option<String> {
    use libp2p::multiaddr::Protocol;

    let mut host: Option<String> = None;
    let mut tcp_port: Option<u16> = None;
    for proto in addr.iter() {
        match proto {
            Protocol::Ip4(v4) => host = Some(v4.to_string()),
            Protocol::Ip6(v6) => host = Some(format!("[{v6}]")),
            Protocol::Tcp(port) => tcp_port = Some(port),
            _ => {}
        }
    }
    let host = host?;
    let tcp_port = tcp_port?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if crate::addrs::is_private_or_local(bare) {
        return None;
    }
    let udp_port = tcp_port.checked_sub(10000)?;
    Some(format!("{host}:{udp_port}"))
}

/// How gossipsub tells two announces apart.
///
/// **This has to be set, and the default is silently wrong for us.** Gossipsub
/// dedupes by message id, both when publishing (`PublishError::Duplicate`) and
/// when receiving (`duplicate_cache`, 60 s by default). Its default id function
/// is `source + sequence_number` - and both of those are `None` under
/// [`gossipsub::MessageAuthenticity::Anonymous`], which is what this swarm
/// uses. Every message this node ever published therefore carried the *same*
/// id, the constant `15R0` (`31355230` in the hex the log prints): the captain
/// saw `Not publishing a message that has already been published
/// message_id=31355230` every 5 s, with the same id, forever.
///
/// The damage is not the log line. It is that dedup by a constant id means
/// **one announce per minute gets through in each direction**, whatever it
/// says: this node could publish one, and - worse - could *accept* one, from
/// whichever peer's announce arrived first. Every other peer's announce, and
/// every change of model, layer range or advertised address, was dropped as a
/// duplicate of an unrelated message. Peers time out after 30 s
/// (`PEER_TIMEOUT`), so a peer reachable only over gossipsub flapped.
///
/// Hashing the payload is the whole fix: the announce already carries
/// `timestamp: Utc::now()` (`Network::self_announce`), so consecutive
/// announces differ in bytes and get distinct ids, while the same message
/// arriving twice through two mesh paths still collapses into one - which is
/// what the dedup is actually for.
///
/// Local by construction: `message_id_fn` never travels, so an upgraded node
/// interoperates with one still running the default.
fn announce_message_id(message: &gossipsub::Message) -> gossipsub::MessageId {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    message.topic.hash(&mut h);
    message.data.hash(&mut h);
    gossipsub::MessageId::from(h.finish().to_be_bytes().to_vec())
}

#[derive(NetworkBehaviour)]
struct DiscBehaviour {
    gossipsub: gossipsub::Behaviour,
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
    relay: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    autonat: autonat::Behaviour,
    relay_server: relay::Behaviour,
    infer_tunnel: request_response::Behaviour<InferTunnelCodec>,
}

pub struct Discovery {
    pub announce_tx: mpsc::Sender<Vec<u8>>,
    pub dial_tx: mpsc::Sender<Multiaddr>,
    swarm: Swarm<DiscBehaviour>,
    topic: gossipsub::IdentTopic,
    network: Arc<Network>,
    announce_rx: mpsc::Receiver<Vec<u8>>,
    dial_rx: mpsc::Receiver<Multiaddr>,
    tunnel_rx: mpsc::Receiver<TunnelOutbound>,
    relay_peer_addrs: HashSet<Multiaddr>,
    relay_peer_ids: HashSet<PeerId>,
    connected_relays: HashSet<PeerId>,
    /// Used only to forward an inbound tunnel request to this node's own
    /// inference server on loopback — see `infer_tunnel::serve_loopback`.
    http: reqwest::Client,
}

impl Discovery {
    /// `keypair` should be derived from this node's pkarr identity
    /// (`PkarrNode::secret_key_bytes`) whenever one is available, so the
    /// libp2p `PeerId` is stable across restarts — a fresh keypair every
    /// process start (the old behaviour) means peers can never reliably
    /// target this node by `PeerId` for anything, including the NAT-tunnel
    /// inference path. Callers without a pkarr identity pass a freshly
    /// generated one (`libp2p::identity::Keypair::generate_ed25519()`);
    /// discovery still works, just without a stable identity.
    /// `tunnel_rx` is created by the caller (`main.rs`), not here — unlike
    /// `announce_tx`/`dial_tx`, the matching sender has to exist and be
    /// cloneable into `GatewayState` *before* this function is ever called
    /// (the gateway's own HTTP server starts earlier in `main()` than
    /// libp2p discovery does), so the channel can't be born inside
    /// `Discovery::new` the way the other two are.
    pub fn new(
        udp_port: u16,
        bootstrap_addrs: &[String],
        relay_addrs: &[String],
        serve_relay: bool,
        use_default_relays: bool,
        network: Arc<Network>,
        keypair: libp2p::identity::Keypair,
        tunnel_rx: mpsc::Receiver<TunnelOutbound>,
    ) -> Result<Self> {
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
                    // Without this, gossipsub silently stops carrying
                    // announces. See `announce_message_id`.
                    .message_id_fn(announce_message_id)
                    .build()
                    .expect("gossipsub config");
                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Anonymous,
                    gossipsub_cfg,
                )
                .expect("gossipsub init");

                let peer_id = key.public().to_peer_id();
                let mut kademlia =
                    kad::Behaviour::new(peer_id, kad::store::MemoryStore::new(peer_id));
                kademlia.set_mode(Some(kad::Mode::Server));

                let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)
                    .expect("mdns init");

                let identify = identify::Behaviour::new(identify::Config::new(
                    "/pom/1.0.0".to_string(),
                    key.public(),
                ));

                // Build relay server config. When `serve_relay` is false we set
                // max_reservations to 0 so no clients can reserve slots.
                let relay_server_cfg = if serve_relay {
                    relay::Config {
                        max_reservations: 128,
                        max_reservations_per_peer: 4,
                        reservation_duration: Duration::from_secs(3600),
                        max_circuit_duration: Duration::from_secs(120),
                        // A relayed (non-hole-punched) infer-tunnel hop's
                        // request can be up to `wire::MAX_FRAME_PAYLOAD_BYTES`
                        // — a real prefill chunk's hidden state easily runs
                        // several MB. The libp2p default (1<<17 = 128KB) would
                        // silently truncate it whenever this node is the relay
                        // circuit a NAT'd peer ends up routed through. Only
                        // fixable here, for a self-hosted relay — the public
                        // `DEFAULT_RELAY_NODES` (third-party IPFS
                        // infrastructure) enforce their own cap, outside this
                        // project's control.
                        max_circuit_bytes: wire::MAX_FRAME_PAYLOAD_BYTES as u64,
                        ..relay::Config::default()
                    }
                } else {
                    relay::Config {
                        max_reservations: 0,
                        ..relay::Config::default()
                    }
                };

                let relay_server = relay::Behaviour::new(peer_id, relay_server_cfg);

                let infer_tunnel = infer_tunnel::infer_tunnel_behaviour();

                DiscBehaviour {
                    gossipsub,
                    kademlia,
                    mdns,
                    identify,
                    relay: relay_client,
                    dcutr: dcutr::Behaviour::new(peer_id),
                    autonat: autonat::Behaviour::new(peer_id, autonat::Config::default()),
                    relay_server,
                    infer_tunnel,
                }
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        let listen_addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{}", libp2p_port).parse()?;
        swarm.listen_on(listen_addr)?;
        if let Ok(v6_addr) = format!("/ip6/::/tcp/{}", libp2p_port).parse::<Multiaddr>() {
            swarm.listen_on(v6_addr).ok();
        }

        swarm.behaviour_mut().gossipsub.subscribe(&topic).ok();

        for addr_str in bootstrap_addrs {
            if let Some(ma) = udp_to_libp2p_multiaddr(addr_str) {
                info!("libp2p: dialling bootstrap {}", ma);
                if let Err(e) = swarm.dial(ma) {
                    warn!("libp2p dial {}: {}", addr_str, e);
                }
            }
        }

        let relay_peer_ids: HashSet<PeerId> = relay_peer_addrs
            .iter()
            .filter_map(|ma| {
                ma.iter().find_map(|p| {
                    if let libp2p::multiaddr::Protocol::P2p(pid) = p {
                        Some(pid)
                    } else {
                        None
                    }
                })
            })
            .collect();

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
            tunnel_rx,
            relay_peer_addrs,
            relay_peer_ids,
            connected_relays: HashSet::new(),
            http: reqwest::Client::new(),
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
            mut tunnel_rx,
            relay_peer_addrs,
            relay_peer_ids,
            mut connected_relays,
            http,
        } = self;
        // Outstanding outbound tunnel calls, keyed by the id the swarm
        // assigns on `send_request` — resolved in `handle_event` when the
        // matching `Message::Response` or `Event::OutboundFailure` arrives.
        // Lives here, not on `Discovery`, because only this loop ever touches
        // it — same reasoning as `connected_relays`.
        let mut pending_tunnel: HashMap<
            OutboundRequestId,
            oneshot::Sender<Result<infer_tunnel::TunnelResponse, String>>,
        > = HashMap::new();
        loop {
            tokio::select! {
                event = swarm.next() => {
                    let Some(ev) = event else { break };
                    handle_event(&mut swarm, &network, ev, &relay_peer_addrs, &relay_peer_ids, &mut connected_relays, &http, &mut pending_tunnel).await;
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
                out = tunnel_rx.recv() => {
                    let Some(TunnelOutbound { peer_id, request, reply }) = out else { break };
                    let request_id = swarm.behaviour_mut().infer_tunnel.send_request(&peer_id, request);
                    pending_tunnel.insert(request_id, reply);
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
    relay_peer_ids: &HashSet<PeerId>,
    connected_relays: &mut HashSet<PeerId>,
    http: &reqwest::Client,
    pending_tunnel: &mut HashMap<
        OutboundRequestId,
        oneshot::Sender<Result<infer_tunnel::TunnelResponse, String>>,
    >,
) {
    match event {
        // ── Gossipsub: peer announced itself ────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Gossipsub(gossipsub::Event::Message {
            message,
            ..
        })) => {
            if let Ok(P2PMessage::Announce {
                node_id,
                pubkey,
                addr,
                addr_candidates,
                model,
                start_layer,
                end_layer,
                total_layers,
                memory_mb,
                timestamp,
                ollama_port,
                inference_port,
                inference_frame_capabilities,
                gateway_port,
                role,
                tls,
                hostname,
                capabilities,
                disk_total_mb,
                disk_free_mb,
                libp2p_peer_id,
                tp,
            }) = serde_json::from_slice(&message.data)
            {
                let dial_for_tunnel = libp2p_peer_id
                    .as_deref()
                    .and_then(|s| s.parse::<PeerId>().ok())
                    .filter(|_| {
                        inference_frame_capabilities
                            .iter()
                            .any(|c| c == "transport/libp2p-tunnel-v1")
                    });

                network
                    .insert_peer(PeerInfo {
                        node_id,
                        pubkey,
                        // O caminho do gossipsub nao tem endereco de origem para
                        // reescrever, e o anunciado costuma ser o literal
                        // `0.0.0.0`. Com candidatos ha o que usar: o primeiro
                        // deles e um endereco de verdade, e o `0.0.0.0` so
                        // sobrevive quando o peer nao anunciou nenhum.
                        addr: addr_candidates.first().cloned().unwrap_or(addr),
                        addr_candidates,
                        model,
                        start_layer,
                        end_layer,
                        total_layers,
                        memory_mb,
                        last_seen: timestamp,
                        ollama_port,
                        inference_port,
                        inference_frame_capabilities,
                        gateway_port,
                        role,
                        tls,
                        hostname,
                        capabilities,
                        connection_verified: false,
                        disk_total_mb,
                        disk_free_mb,
                        api_keys_active: 0,
                        libp2p_peer_id: libp2p_peer_id.clone(),
                        tp,
                    })
                    .await;

                // Warm the connection ahead of ever needing the NAT-tunnel
                // fallback: a relay reservation / DCuTR hole-punch has time
                // to complete in the background now, instead of adding that
                // latency cold onto the very first real inference request.
                // `is_connected` skips a redundant dial for an already-live
                // link (e.g. this peer is also reachable directly on the LAN).
                if let Some(peer_id) = dial_for_tunnel {
                    if !swarm.is_connected(&peer_id) {
                        debug!("libp2p: warming connection to tunnel-capable peer {peer_id}");
                        swarm.dial(peer_id).ok();
                    }
                }
            }
        }

        // ── mDNS: LAN peer found ─────────────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
            for (peer_id, addr) in list {
                info!("mDNS: {} at {}", peer_id, addr);
                swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(&peer_id, addr.clone());
                if let Err(e) = swarm.dial(addr) {
                    debug!("mDNS dial: {}", e);
                }
            }
        }

        // ── mDNS: LAN peer expired ────────────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
            for (peer_id, addr) in list {
                swarm
                    .behaviour_mut()
                    .kademlia
                    .remove_address(&peer_id, &addr);
            }
        }

        // ── Identify: learn remote's addresses; listen on relay circuit ───────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            // What this peer saw when *we* connected to *them* — the closest
            // thing to a free STUN reply this project has: real evidence of
            // this node's own public address, learned without a dedicated
            // NAT-discovery dependency. See `observed_host_port`'s doc
            // comment and `Network::set_observed_addr`.
            if let Some(host_port) = observed_host_port(&info.observed_addr) {
                network.set_observed_addr(&host_port);
            }
            for addr in info.listen_addrs {
                swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
            }
            if connected_relays.contains(&peer_id) {
                if let Some(relay_addr) = relay_peer_addrs.iter().find(|ma| {
                    ma.iter().any(
                        |p| matches!(p, libp2p::multiaddr::Protocol::P2p(pid) if pid == peer_id),
                    )
                }) {
                    let relay_circuit = relay_addr
                        .clone()
                        .with(libp2p::multiaddr::Protocol::P2pCircuit);
                    swarm.listen_on(relay_circuit).ok();
                }
            }
        }

        SwarmEvent::NewListenAddr { address, .. } => {
            info!("libp2p listening on {}", address);
        }

        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
            if relay_peer_ids.contains(&peer_id) {
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
            info!("Relay slot reserved on {}", relay_peer_id);
        }

        // ── DCuTR: hole punch result ─────────────────────────────────────────
        // dcutr::Event is a struct { remote_peer_id, result: Result<ConnectionId, Error> }
        SwarmEvent::Behaviour(DiscBehaviourEvent::Dcutr(dcutr::Event {
            remote_peer_id,
            result,
        })) => match result {
            Ok(_conn_id) => {
                info!(
                    "Direct connection to {} (hole punch succeeded)",
                    remote_peer_id
                );
            }
            Err(e) => {
                warn!(
                    "DCuTR failed for {} ({:?}) — relay fallback active",
                    remote_peer_id, e
                );
            }
        },

        // ── AutoNAT: NAT type detected ───────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Autonat(autonat::Event::StatusChanged {
            new,
            ..
        })) => {
            info!("NAT status: {:?}", new);
        }

        // ── Infer tunnel: inference HTTP calls carried over the swarm as a
        // NAT-traversal fallback. `Message::Request` is the inbound side
        // (forwarded to our own loopback inference server); `Message::Response`
        // and `Event::OutboundFailure` resolve a call *we* dispatched, via
        // `tunnel_rx` in `run()`'s select loop, by looking up the
        // `OutboundRequestId` the swarm handed back from `send_request`.
        SwarmEvent::Behaviour(DiscBehaviourEvent::InferTunnel(event)) => match event {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    match network.inference_port {
                        Some(inference_port) => {
                            debug!(
                                "infer-tunnel: {} → loopback :{} {}",
                                peer, inference_port, request.path
                            );
                            let response =
                                infer_tunnel::serve_loopback(http, inference_port, request).await;
                            let _ = swarm
                                .behaviour_mut()
                                .infer_tunnel
                                .send_response(channel, response);
                        }
                        None => {
                            // Nothing to forward to — this node has no
                            // inference server running (no layer assignment
                            // yet, or a pure-relay node). Answer rather than
                            // dropping the channel silently, so the caller's
                            // fallback logic gets a definite failure instead
                            // of a timeout.
                            let _ = swarm.behaviour_mut().infer_tunnel.send_response(
                                channel,
                                infer_tunnel::TunnelResponse {
                                    status: 503,
                                    body: b"no inference server on this node".to_vec(),
                                },
                            );
                        }
                    }
                }
                request_response::Message::Response {
                    request_id,
                    response,
                } => {
                    if let Some(reply) = pending_tunnel.remove(&request_id) {
                        let _ = reply.send(Ok(response));
                    } else {
                        // The caller already gave up (e.g. `send_frame_via_tunnel`'s
                        // own timeout fired first) and dropped its `oneshot`
                        // receiver — a late response is expected, not a bug.
                        debug!(
                            "infer-tunnel: response for an unknown/expired request from {}",
                            peer
                        );
                    }
                }
            },
            request_response::Event::OutboundFailure {
                peer,
                request_id,
                error,
                ..
            } => {
                debug!(
                    "infer-tunnel: outbound call to {} failed: {:?}",
                    peer, error
                );
                if let Some(reply) = pending_tunnel.remove(&request_id) {
                    let _ = reply.send(Err(error.to_string()));
                }
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                debug!(
                    "infer-tunnel: inbound call from {} failed: {:?}",
                    peer, error
                );
            }
            request_response::Event::ResponseSent { .. } => {}
        },

        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An IPv6 bootstrap used to be dropped in silence: the address went into an
    /// `/ip4/` multiaddr, the parse failed, and `None` skipped the dial.
    #[test]
    fn a_bootstrap_address_becomes_a_dialable_multiaddr() {
        let ma = |s: &str| udp_to_libp2p_multiaddr(s).map(|m| m.to_string());

        assert_eq!(
            ma("192.168.100.138:42777").as_deref(),
            Some("/ip4/192.168.100.138/tcp/52777")
        );
        assert_eq!(
            ma("[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777").as_deref(),
            Some("/ip6/2804:d55:5579:1000:89f3:3f94:7c07:e183/tcp/52777")
        );
        assert_eq!(ma("[::1]:42901").as_deref(), Some("/ip6/::1/tcp/52901"));
        assert_eq!(
            ma("ksia.local:42777").as_deref(),
            Some("/dns/ksia.local/tcp/52777")
        );
        assert_eq!(ma("sem-porta"), None);
    }

    /// Inverse of `a_bootstrap_address_becomes_a_dialable_multiaddr` — round
    /// trips a public address through both conversions, and confirms a
    /// private one is refused (not new information about our public IP).
    #[test]
    fn an_observed_multiaddr_becomes_a_host_port() {
        assert_eq!(
            observed_host_port(&"/ip4/203.0.113.5/tcp/52777".parse().unwrap()).as_deref(),
            Some("203.0.113.5:42777")
        );
        assert_eq!(
            observed_host_port(&"/ip6/2804:d55::1/tcp/52777".parse().unwrap()).as_deref(),
            Some("[2804:d55::1]:42777")
        );
    }

    #[test]
    fn a_private_observed_address_is_refused() {
        assert_eq!(
            observed_host_port(&"/ip4/192.168.1.5/tcp/52777".parse().unwrap()),
            None
        );
        assert_eq!(
            observed_host_port(&"/ip4/10.0.0.5/tcp/52777".parse().unwrap()),
            None
        );
    }

    /// A relay-circuit or QUIC observed address carries no plain `/tcp/`
    /// component (or, for the circuit case, is missing a resolvable TCP port
    /// on the far side) — not the shape this project's peers dial for HTTP,
    /// so it must be refused rather than mis-parsed into a bogus port.
    #[test]
    fn a_non_tcp_observed_address_is_refused() {
        assert_eq!(
            observed_host_port(&"/ip4/203.0.113.5/udp/52777/quic-v1".parse().unwrap()),
            None
        );
    }

    /// A libp2p TCP port below 10 000 can never have come from this
    /// project's own `udp_port + 10000` convention — the subtraction must
    /// not wrap into a bogus large `u16` instead of refusing outright.
    #[test]
    fn a_tcp_port_below_the_udp_offset_is_refused() {
        assert_eq!(
            observed_host_port(&"/ip4/203.0.113.5/tcp/80".parse().unwrap()),
            None
        );
    }

    // ── Como o gossipsub distingue dois anúncios ────────────────────────────

    fn gossip_msg(data: &[u8]) -> gossipsub::Message {
        gossipsub::Message {
            source: None,
            data: data.to_vec(),
            sequence_number: None,
            topic: gossipsub::IdentTopic::new(GOSSIP_TOPIC).hash(),
        }
    }

    /// O id que o `message_id_fn` padrão dá a todo anúncio deste projeto.
    ///
    /// Reproduz o defeito no lugar exato: com `MessageAuthenticity::Anonymous`
    /// não há `source` nem `sequence_number`, então a função padrão devolve
    /// sempre `15R0` - o `message_id=31355230` do log do captain, em hexa.
    /// Dois anúncios diferentes recebem o mesmo id, e é por isso que o
    /// gossipsub parou de propagar.
    fn libp2p_default_message_id(message: &gossipsub::Message) -> gossipsub::MessageId {
        let mut source_string = match message.source.as_ref() {
            Some(peer_id) => peer_id.to_base58(),
            None => PeerId::from_bytes(&[0, 1, 0])
                .expect("valid peer id")
                .to_base58(),
        };
        source_string.push_str(&message.sequence_number.unwrap_or_default().to_string());
        gossipsub::MessageId::from(source_string)
    }

    #[test]
    fn the_libp2p_default_id_collapses_every_announce_into_one() {
        let a = libp2p_default_message_id(&gossip_msg(br#"{"type":"Announce","t":1}"#));
        let b = libp2p_default_message_id(&gossip_msg(br#"{"type":"Announce","t":2}"#));
        assert_eq!(a, b, "é este o defeito que `announce_message_id` corrige");
        // O id que o captain viu no log, hexa de "15R0".
        assert_eq!(a.to_string(), "31355230");
    }

    /// Dois anúncios consecutivos diferem no `timestamp`, então precisam de
    /// ids diferentes - senão o publish periódico é recusado como duplicata e
    /// o peer some da rede do outro lado depois de `PEER_TIMEOUT`.
    #[test]
    fn two_different_announces_get_two_different_ids() {
        let a = announce_message_id(&gossip_msg(br#"{"type":"Announce","t":1}"#));
        let b = announce_message_id(&gossip_msg(br#"{"type":"Announce","t":2}"#));
        assert_ne!(a, b);
    }

    /// E o mesmo anúncio chegando por dois caminhos da malha continua sendo
    /// uma duplicata - que é para isso que o cache serve.
    #[test]
    fn the_same_announce_keeps_one_id() {
        let bytes = br#"{"type":"Announce","node_id":"abc"}"#;
        assert_eq!(
            announce_message_id(&gossip_msg(bytes)),
            announce_message_id(&gossip_msg(bytes))
        );
    }

    #[test]
    fn host_and_port_split_on_the_brackets_not_the_colons() {
        assert_eq!(
            split_host_port("[2804:d55::1]:42777"),
            Some(("2804:d55::1", "42777"))
        );
        assert_eq!(
            split_host_port("10.0.0.1:42777"),
            Some(("10.0.0.1", "42777"))
        );
        assert_eq!(split_host_port("[2804:d55::1]"), None);
    }
}
