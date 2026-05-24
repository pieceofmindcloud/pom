use crate::network::{Network, PeerInfo, P2PMessage};
use anyhow::Result;
use futures::StreamExt;
use libp2p::{
    gossipsub, identify, kad, mdns, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, Multiaddr, Swarm,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const GOSSIP_TOPIC: &str = "rdma-p2p/announce/1.0.0";

/// libp2p TCP port is always UDP port + 10 000.
/// E.g. UDP 42777 → libp2p TCP 52777.
/// Bootstrap peers listed with `--bootstrap host:udp_port` are dialled on udp_port+10000.
fn udp_to_libp2p_multiaddr(addr: &str) -> Option<Multiaddr> {
    let (host, port_str) = addr.rsplit_once(':')?;
    let udp_port: u16 = port_str.parse().ok()?;
    let tcp_port = udp_port.checked_add(10000)?;
    format!("/ip4/{}/tcp/{}", host, tcp_port).parse().ok()
}

#[derive(NetworkBehaviour)]
struct DiscBehaviour {
    gossipsub: gossipsub::Behaviour,
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
}

pub struct Discovery {
    /// Send serialised `P2PMessage::Announce` bytes here to publish them via gossipsub.
    pub announce_tx: mpsc::Sender<Vec<u8>>,
    /// Send a `Multiaddr` here to have the swarm dial it (used by pkarr resolver).
    pub dial_tx: mpsc::Sender<libp2p::Multiaddr>,
    swarm: Swarm<DiscBehaviour>,
    topic: gossipsub::IdentTopic,
    network: Arc<Network>,
    announce_rx: mpsc::Receiver<Vec<u8>>,
    dial_rx: mpsc::Receiver<libp2p::Multiaddr>,
}

impl Discovery {
    pub fn new(
        udp_port: u16,
        bootstrap_addrs: &[String],
        network: Arc<Network>,
    ) -> Result<Self> {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let libp2p_port = udp_port.saturating_add(10000);
        let topic = gossipsub::IdentTopic::new(GOSSIP_TOPIC);

        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_dns()?
            .with_behaviour(|key| {
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

                DiscBehaviour { gossipsub, kademlia, mdns, identify }
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

        let (announce_tx, announce_rx) = mpsc::channel(64);
        let (dial_tx, dial_rx) = mpsc::channel(64);
        Ok(Discovery { announce_tx, dial_tx, swarm, topic, network, announce_rx, dial_rx })
    }

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
}

async fn handle_event(
    swarm: &mut Swarm<DiscBehaviour>,
    network: &Network,
    event: SwarmEvent<DiscBehaviourEvent>,
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

        // ── Identify: learn remote's listen addresses → feed into Kademlia ───
        SwarmEvent::Behaviour(DiscBehaviourEvent::Identify(identify::Event::Received {
            peer_id,
            info,
            ..
        })) => {
            for addr in info.listen_addrs {
                swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
            }
        }

        SwarmEvent::NewListenAddr { address, .. } => {
            info!("🌐 libp2p listening on {}", address);
        }

        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
            debug!("libp2p: connected to {}", peer_id);
        }

        SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
            debug!("libp2p: disconnected from {} ({:?})", peer_id, cause);
        }

        _ => {}
    }
}
