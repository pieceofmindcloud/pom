use crate::bitswap::{self, BitswapCodec};
use crate::network::{Network, PeerInfo, P2PMessage};
use anyhow::Result;
use futures::StreamExt;
use libp2p::{
    autonat, dcutr, gossipsub, identify, kad, mdns, noise, relay, request_response,
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
    bitswap:      request_response::Behaviour<BitswapCodec>,
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
    relay_peer_ids:   HashSet<PeerId>,
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

                // Build relay server config. When `serve_relay` is false we set
                // max_reservations to 0 so no clients can reserve slots.
                let relay_server_cfg = if serve_relay {
                    relay::Config {
                        max_reservations: 128,
                        max_reservations_per_peer: 4,
                        reservation_duration: Duration::from_secs(3600),
                        max_circuit_duration: Duration::from_secs(120),
                        max_circuit_bytes: 1 << 17,
                        ..relay::Config::default()
                    }
                } else {
                    relay::Config {
                        max_reservations: 0,
                        ..relay::Config::default()
                    }
                };

                let relay_server = relay::Behaviour::new(peer_id, relay_server_cfg);

                let bitswap = bitswap::bitswap_behaviour();

                DiscBehaviour {
                    gossipsub,
                    kademlia,
                    mdns,
                    identify,
                    relay: relay_client,
                    dcutr: dcutr::Behaviour::new(peer_id),
                    autonat: autonat::Behaviour::new(peer_id, autonat::Config::default()),
                    relay_server,
                    bitswap,
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
            relay_peer_addrs,
            relay_peer_ids,
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
            relay_peer_ids,
            mut connected_relays,
        } = self;
        loop {
            tokio::select! {
                event = swarm.next() => {
                    let Some(ev) = event else { break };
                    handle_event(&mut swarm, &network, ev, &relay_peer_addrs, &relay_peer_ids, &mut connected_relays).await;
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
    relay_peer_ids: &HashSet<PeerId>,
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
                gateway_port,
                role,
                hostname,
                disk_total_mb,
                disk_free_mb,
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
                        gateway_port,
                        role,
                        hostname,
                        connection_verified: false,
                        disk_total_mb,
                        disk_free_mb,
                        api_keys_active: 0,
                    })
                    .await;
            }
        }

        // ── mDNS: LAN peer found ─────────────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
            for (peer_id, addr) in list {
                info!("mDNS: {} at {}", peer_id, addr);
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
                if let Some(relay_addr) = relay_peer_addrs.iter().find(|ma| {
                    ma.iter().any(|p| matches!(p, libp2p::multiaddr::Protocol::P2p(pid) if pid == peer_id))
                }) {
                    let relay_circuit = relay_addr.clone().with(libp2p::multiaddr::Protocol::P2pCircuit);
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
        })) => {
            match result {
                Ok(_conn_id) => {
                    info!("Direct connection to {} (hole punch succeeded)", remote_peer_id);
                }
                Err(e) => {
                    warn!("DCuTR failed for {} ({:?}) — relay fallback active", remote_peer_id, e);
                }
            }
        }

        // ── AutoNAT: NAT type detected ───────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Autonat(
            autonat::Event::StatusChanged { new, .. },
        )) => {
            info!("NAT status: {:?}", new);
        }

        // ── Bitswap: content-addressed chunk exchange ───────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Bitswap(event)) => {
            match event {
                request_response::Event::Message { peer, message, .. } => {
                    match message {
                        request_response::Message::Request { request, .. } => {
                            debug!("Bitswap request from {}: {:?}", peer, request);
                        }
                        request_response::Message::Response { response, .. } => {
                            debug!("Bitswap response from {}: {:?}", peer, response);
                        }
                    }
                }
                _ => {}
            }
        }

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

        assert_eq!(ma("192.168.100.138:42777").as_deref(),
                   Some("/ip4/192.168.100.138/tcp/52777"));
        assert_eq!(
            ma("[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777").as_deref(),
            Some("/ip6/2804:d55:5579:1000:89f3:3f94:7c07:e183/tcp/52777")
        );
        assert_eq!(ma("[::1]:42901").as_deref(), Some("/ip6/::1/tcp/52901"));
        assert_eq!(ma("ksia.local:42777").as_deref(), Some("/dns/ksia.local/tcp/52777"));
        assert_eq!(ma("sem-porta"), None);
    }

    #[test]
    fn host_and_port_split_on_the_brackets_not_the_colons() {
        assert_eq!(split_host_port("[2804:d55::1]:42777"), Some(("2804:d55::1", "42777")));
        assert_eq!(split_host_port("10.0.0.1:42777"), Some(("10.0.0.1", "42777")));
        assert_eq!(split_host_port("[2804:d55::1]"), None);
    }
}
