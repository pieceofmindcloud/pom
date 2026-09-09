use crate::cluster_logs::{self, ClusterLogs, ClusterLogsCodec, LogOutbound};
use crate::infer_tunnel::{self, InferTunnelCodec, TunnelOutbound};
use crate::network_chat::{self, ChatOutbound, ChatRequest, ChatResponse, NetworkChat, NetworkChatCodec};
use crate::network::{is_valid_destination, Network, P2PMessage, PeerInfo};
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

type InboundTunnelResponse = (
    request_response::ResponseChannel<infer_tunnel::TunnelResponse>,
    infer_tunnel::TunnelResponse,
);

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

    if !is_valid_destination(addr) {
        return None;
    }
    let (host, port_str) = split_host_port(addr)?;
    let udp_port: u16 = port_str.parse().ok()?;
    if udp_port == 0 {
        return None;
    }
    let tcp_port = udp_port.checked_add(10000)?;

    let ma = match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => format!("/ip4/{v4}/tcp/{tcp_port}"),
        Ok(IpAddr::V6(v6)) => format!("/ip6/{v6}/tcp/{tcp_port}"),
        // A hostname: libp2p resolves it, the swarm is built `.with_dns()`.
        Err(_) => format!("/dns/{host}/tcp/{tcp_port}"),
    };
    ma.parse().ok()
}

/// Convert the UDP-style addresses from an `Announce` into the libp2p
/// addresses that belong to the announced peer.
///
/// Gossipsub carries the peer identity and the project's advertised UDP port,
/// while the libp2p TCP listener uses that port plus 10,000. Registering these
/// addresses with the swarm is required before `swarm.dial(peer_id)` can work;
/// a peer learned only through gossip otherwise has an identity but no dialable
/// address and libp2p returns `Failed to dial the requested peer`.
fn tunnel_peer_multiaddrs(peer_id: PeerId, advertised: &[String]) -> Vec<Multiaddr> {
    use libp2p::multiaddr::Protocol;

    let mut seen = HashSet::new();
    advertised
        .iter()
        .filter_map(|addr| udp_to_libp2p_multiaddr(addr))
        .map(|addr| addr.with(Protocol::P2p(peer_id)))
        .filter(|addr| seen.insert(addr.clone()))
        .collect()
}

/// Reidrata o livro de endereços do swarm antes de uma chamada pelo túnel.
///
/// O anúncio pode ter chegado pelo UDP e o `PeerId` pode ter sido repassado ao
/// gateway antes de o evento gossipsub correspondente registrar os endereços no
/// swarm. `request_response::send_request` não disca sem um endereço conhecido:
/// ele falha imediatamente com `Failed to dial the requested peer`. Consultar a
/// tabela viva aqui fecha essa janela e também cobre um swarm que perdeu o
/// endereço após uma reconexão.
async fn ensure_tunnel_peer_address(
    swarm: &mut Swarm<DiscBehaviour>,
    network: &Network,
    peer_id: PeerId,
    advertised_override: &[String],
) -> std::result::Result<(), String> {
    let advertised = if advertised_override.is_empty() {
        let peer_id_text = peer_id.to_string();
        let peer = network
            .alive_peers()
            .await
            .into_iter()
            .find(|peer| peer.libp2p_peer_id.as_deref() == Some(peer_id_text.as_str()));
        let Some(peer) = peer else {
            return Err(format!("nenhum anúncio vivo para o PeerId {peer_id}"));
        };
        let mut advertised = peer.addr_candidates;
        if advertised.is_empty() {
            advertised.push(peer.addr);
        }
        advertised
    } else {
        advertised_override.to_vec()
    };
    let addresses = tunnel_peer_multiaddrs(peer_id, &advertised);
    if addresses.is_empty() {
        return Err(format!("o PeerId {peer_id} não tem endereço discável"));
    }
    for address in addresses {
        swarm.add_peer_address(peer_id, address);
    }
    if !swarm.is_connected(&peer_id) {
        // The request-response behaviour can queue the request while this dial
        // completes. `AlreadyDialing` is harmless; the important part is that
        // the address book is populated before `send_request` runs.
        if let Err(error) = swarm.dial(peer_id) {
            debug!("libp2p: rediscagem do peer {peer_id} não iniciou: {error}");
        }
    }
    Ok(())
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
    if udp_port == 0 {
        return None;
    }
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
    cluster_logs: request_response::Behaviour<ClusterLogsCodec>,
    network_chat: request_response::Behaviour<NetworkChatCodec>,
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
    log_rx: mpsc::Receiver<LogOutbound>,
    logs: Arc<ClusterLogs>,
    chat_rx: mpsc::Receiver<ChatOutbound>,
    chat: Arc<NetworkChat>,
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
        log_rx: mpsc::Receiver<LogOutbound>,
        logs: Arc<ClusterLogs>,
        chat_rx: mpsc::Receiver<ChatOutbound>,
        chat: Arc<NetworkChat>,
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
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    // Without this, gossipsub silently stops carrying
                    // announces. See `announce_message_id`.
                    .message_id_fn(announce_message_id)
                    .build()
                    .expect("gossipsub config");
                let gossipsub = gossipsub::Behaviour::new(
                    gossipsub::MessageAuthenticity::Signed(key.clone()),
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
                let cluster_logs = cluster_logs::cluster_logs_behaviour();
                let network_chat = network_chat::network_chat_behaviour();

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
                    cluster_logs,
                    network_chat,
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
            log_rx,
            logs,
            chat_rx,
            chat,
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
            mut log_rx,
            logs,
            mut chat_rx,
            chat,
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
        let mut pending_chat: HashMap<
            OutboundRequestId,
            oneshot::Sender<ChatResponse>,
        > = HashMap::new();
        let (inbound_response_tx, mut inbound_response_rx) =
            mpsc::channel::<InboundTunnelResponse>(64);
        loop {
            tokio::select! {
                biased;
                event = swarm.next() => {
                    let Some(ev) = event else { break };
                    handle_event(&mut swarm, &network, ev, &relay_peer_addrs, &relay_peer_ids, &mut connected_relays, &http, &mut pending_tunnel, &mut pending_chat, &inbound_response_tx, &logs, &chat).await;
                }
                msg = announce_rx.recv() => {
                    let Some(data) = msg else { break };
                    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), data) {
                        debug!("gossipsub publish: {:?}", e);
                    }
                }
                response = inbound_response_rx.recv() => {
                    let Some((channel, response)) = response else { break };
                    let _ = swarm.behaviour_mut().infer_tunnel.send_response(channel, response);
                }
                ma = dial_rx.recv() => {
                    let Some(addr) = ma else { break };
                    info!("pkarr: dialling {}", addr);
                    if let Err(e) = swarm.dial(addr) {
                        warn!("pkarr dial: {}", e);
                    }
                }
                out = tunnel_rx.recv() => {
                    let Some(TunnelOutbound { peer_id, addr_candidates, request, reply }) = out else { break };
                    if !swarm.is_connected(&peer_id) {
                        if let Err(error) = ensure_tunnel_peer_address(&mut swarm, &network, peer_id, &addr_candidates).await {
                            let _ = reply.send(Err(error));
                            continue;
                        }
                    }
                    let request_id = swarm.behaviour_mut().infer_tunnel.send_request(&peer_id, request);
                    pending_tunnel.insert(request_id, reply);
                }
                out = log_rx.recv() => {
                    let Some(LogOutbound { peer_id, addr_candidates, request }) = out else { break };
                    if !swarm.is_connected(&peer_id) {
                        if let Err(error) = ensure_tunnel_peer_address(&mut swarm, &network, peer_id, &addr_candidates).await {
                            debug!("cluster-logs: peer {peer_id} indisponível: {error}");
                            continue;
                        }
                    }
                    let _ = swarm.behaviour_mut().cluster_logs.send_request(&peer_id, request);
                }
                out = chat_rx.recv() => {
                    let Some(ChatOutbound { request, reply }) = out else { break };
                    if !chat.enabled() {
                        let _ = reply.send(ChatResponse::rejected("network_chat_disabled"));
                        continue;
                    }
                    if network.profile() == crate::profile::NodeProfile::Admin {
                        let result = accept_original_chat(
                            &mut swarm,
                            &network,
                            &chat,
                            request,
                            None,
                        ).await;
                        let _ = reply.send(result);
                        continue;
                    }
                    let Some((peer_id, addr_candidates)) = network.principal_peer().await else {
                        let _ = reply.send(ChatResponse::rejected("principal_unavailable"));
                        continue;
                    };
                    if !swarm.is_connected(&peer_id) {
                        if let Err(error) = ensure_tunnel_peer_address(&mut swarm, &network, peer_id, &addr_candidates).await {
                            let _ = reply.send(ChatResponse::rejected(error));
                            continue;
                        }
                    }
                    let request_id = swarm.behaviour_mut().network_chat.send_request(&peer_id, request);
                    pending_chat.insert(request_id, reply);
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
    pending_chat: &mut HashMap<OutboundRequestId, oneshot::Sender<ChatResponse>>,
    inbound_response_tx: &mpsc::Sender<InboundTunnelResponse>,
    logs: &ClusterLogs,
    chat: &Arc<NetworkChat>,
) {
    match event {
        // ── Gossipsub: peer announced itself ────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Gossipsub(gossipsub::Event::Message {
            message,
            ..
        })) => {
            let Some(source) = message.source else {
                warn!("libp2p: descartei Announce sem peer de origem assinado");
                return;
            };
            if let Ok(P2PMessage::Announce {
                node_id,
                deploy_id,
                pubkey,
                authority_pubkey,
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
                profile,
                tls,
                hostname,
                capabilities,
                disk_total_mb,
                disk_free_mb,
                libp2p_peer_id,
                tp,
                runtime_capacity,
                cluster_logs_enabled,
                idle,
                network_chat_enabled,
            }) = serde_json::from_slice(&message.data)
            {
                if libp2p_peer_id.as_deref() != Some(source.to_string().as_str()) {
                    warn!(
                        "libp2p: descartei Announce de {} com PeerId declarado divergente",
                        source
                    );
                    return;
                }
                let dial_for_tunnel = libp2p_peer_id
                    .as_deref()
                    .and_then(|s| s.parse::<PeerId>().ok())
                    .filter(|_| {
                        inference_frame_capabilities
                            .iter()
                            .any(|c| c == "transport/libp2p-tunnel-v1")
                    });
                let tunnel_peer_addrs = dial_for_tunnel.map(|peer_id| {
                    let mut advertised = addr_candidates.clone();
                    if advertised.is_empty() {
                        advertised.push(addr.clone());
                    }
                    (peer_id, tunnel_peer_multiaddrs(peer_id, &advertised))
                });

                network
                    .insert_peer(PeerInfo {
                        deploy_id,
                        node_id: node_id.clone(),
                        pubkey,
                        authority_pubkey,
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
                        profile,
                        tls,
                        hostname,
                        capabilities,
                        connection_verified: false,
                        disk_total_mb,
                        disk_free_mb,
                        api_keys_active: 0,
                        libp2p_peer_id: libp2p_peer_id.clone(),
                        tp,
                        runtime_capacity,
                        idle,
                    })
                    .await;

                network.trigger_cluster_logs_probe(
                    &node_id,
                    profile,
                    cluster_logs_enabled,
                    tls,
                );
                network.trigger_network_chat_probe(
                    &node_id,
                    profile,
                    network_chat_enabled,
                    tls,
                );

                // Warm the connection ahead of ever needing the NAT-tunnel
                // fallback: a relay reservation / DCuTR hole-punch has time
                // to complete in the background now, instead of adding that
                // latency cold onto the very first real inference request.
                // `is_connected` skips a redundant dial for an already-live
                // link (e.g. this peer is also reachable directly on the LAN).
                if let Some((peer_id, addresses)) = tunnel_peer_addrs {
                    for address in &addresses {
                        swarm.add_peer_address(peer_id, address.clone());
                    }

                    if !swarm.is_connected(&peer_id) {
                        if addresses.is_empty() {
                            debug!("libp2p: tunnel peer {peer_id} announced no dialable address");
                        } else {
                            debug!(
                                "libp2p: warming connection to tunnel-capable peer {peer_id} via {} address(es)",
                                addresses.len()
                            );
                            if let Err(error) = swarm.dial(peer_id) {
                                debug!("libp2p: could not warm tunnel peer {peer_id}: {error}");
                            }
                        }
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
                            let response_tx = inbound_response_tx.clone();
                            spawn_loopback_request(
                                http,
                                inference_port,
                                request,
                                move |response| async move {
                                    let _ = response_tx.send((channel, response)).await;
                                },
                            );
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

        // ── Chat da rede: o principal valida e retransmite ────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::NetworkChat(event)) => match event {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    let response = if request.relayed {
                        let source_is_principal = network
                            .alive_peers()
                            .await
                            .into_iter()
                            .find(|candidate| {
                                candidate.libp2p_peer_id.as_deref() == Some(peer.to_string().as_str())
                            })
                            .is_some_and(|candidate| {
                                candidate.profile == crate::profile::NodeProfile::Admin
                            });
                        if !source_is_principal || !chat.enabled() {
                            ChatResponse::rejected("relay_not_authorized")
                        } else {
                            match chat.accept(request.action.clone()) {
                                Ok(true) => {
                                    relay_chat(swarm, network, chat, request.action, Some(peer)).await;
                                    ChatResponse::accepted()
                                }
                                Ok(false) => ChatResponse::rejected("network_chat_disabled"),
                                Err(error) => ChatResponse::rejected(error.to_string()),
                            }
                        }
                    } else {
                        let source_node_id = network
                            .alive_peers()
                            .await
                            .into_iter()
                            .find(|candidate| {
                                candidate.libp2p_peer_id.as_deref() == Some(peer.to_string().as_str())
                            })
                            .map(|candidate| candidate.node_id);
                        match source_node_id {
                            Some(source_node_id) => {
                                accept_original_chat(
                                    swarm,
                                    network,
                                    chat,
                                    request,
                                    Some((peer, source_node_id)),
                                )
                                .await
                            }
                            None => ChatResponse::rejected("sender_unknown"),
                        }
                    };
                    let _ = swarm
                        .behaviour_mut()
                        .network_chat
                        .send_response(channel, response);
                }
                request_response::Message::Response {
                    request_id,
                    response,
                } => {
                    if let Some(reply) = pending_chat.remove(&request_id) {
                        let _ = reply.send(response);
                    }
                }
            },
            request_response::Event::OutboundFailure {
                request_id, error, ..
            } => {
                if let Some(reply) = pending_chat.remove(&request_id) {
                    let _ = reply.send(ChatResponse::rejected(error.to_string()));
                }
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                debug!("network-chat: pedido de {peer} falhou: {error:?}");
            }
            request_response::Event::ResponseSent { .. } => {}
        },

        // ── Cluster process logs: principal-only receiver ───────────────────
        // This request/response behaviour is separate from the inference
        // tunnel. The bounded writer owns persistence, so the swarm answers
        // without waiting for SQLite or the browser.
        SwarmEvent::Behaviour(DiscBehaviourEvent::ClusterLogs(event)) => match event {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    let accepted = logs.try_ingest_remote(request);
                    if !accepted {
                        debug!("cluster-logs: lote recusado de {peer}");
                    }
                    let _ = swarm
                        .behaviour_mut()
                        .cluster_logs
                        .send_response(channel, cluster_logs::LogResponse { accepted });
                }
                request_response::Message::Response { .. } => {}
            },
            request_response::Event::OutboundFailure { peer, error, .. } => {
                debug!("cluster-logs: envio para {peer} falhou: {error:?}");
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                debug!("cluster-logs: lote recebido de {peer} falhou: {error:?}");
            }
            request_response::Event::ResponseSent { .. } => {}
        },

        _ => {}
    }
}

async fn accept_original_chat(
    swarm: &mut Swarm<DiscBehaviour>,
    network: &Network,
    chat: &Arc<NetworkChat>,
    request: ChatRequest,
    source: Option<(PeerId, String)>,
) -> ChatResponse {
    if !chat.enabled() {
        return ChatResponse::rejected("network_chat_disabled");
    }
    if source.is_some() && network.profile() != crate::profile::NodeProfile::Admin {
        return ChatResponse::rejected("principal_unavailable");
    }
    let transport_node_id = source
        .as_ref()
        .map(|(_, node_id)| node_id.as_str())
        .unwrap_or(network.node_id.as_str());
    if let Err(error) = chat
        .validate_original(&request.action, &request.api_key, transport_node_id)
        .await
    {
        return ChatResponse::rejected(error.to_string());
    }
    let action = request.action;
    match chat.accept(action.clone()) {
        Ok(true) => {
            if network.profile() == crate::profile::NodeProfile::Admin {
                relay_chat(
                    swarm,
                    network,
                    chat,
                    action,
                    source.map(|(peer_id, _)| peer_id),
                )
                .await;
            }
            ChatResponse::accepted()
        }
        Ok(false) => ChatResponse::rejected("network_chat_disabled"),
        Err(error) => ChatResponse::rejected(error.to_string()),
    }
}

async fn relay_chat(
    swarm: &mut Swarm<DiscBehaviour>,
    network: &Network,
    chat: &Arc<NetworkChat>,
    action: network_chat::ChatAction,
    excluded_peer: Option<PeerId>,
) {
    let peers = network.alive_peers().await;
    let excluded_peer_id = excluded_peer.map(|id| id.to_string());
    for peer in peers {
        if peer.node_id == network.node_id
            || peer.libp2p_peer_id.as_deref() == excluded_peer_id.as_deref()
            || action
                .recipient_node_id()
                .is_some_and(|recipient| recipient != peer.node_id)
            || chat.node_is_blocked(&peer.node_id).await
        {
            continue;
        }
        let Some(peer_id_text) = peer.libp2p_peer_id.as_deref() else {
            continue;
        };
        let Ok(peer_id) = peer_id_text.parse::<PeerId>() else {
            continue;
        };
        let addresses = peer.dialable_addrs();
        if !swarm.is_connected(&peer_id) {
            if let Err(error) = ensure_tunnel_peer_address(swarm, network, peer_id, &addresses).await {
                debug!("network-chat: peer {} indisponível: {}", peer.node_id, error);
                continue;
            }
        }
        swarm.behaviour_mut().network_chat.send_request(
            &peer_id,
            ChatRequest {
                action: action.clone(),
                api_key: String::new(),
                relayed: true,
            },
        );
    }
}

/// Forward an inbound inference request without blocking the swarm event loop.
///
/// The loopback HTTP call can run for the whole duration of a prefill, decode,
/// or TP reduction. Keeping that await inside `handle_event` prevents the swarm
/// from draining GossipSub and other libp2p protocols, eventually filling the
/// per-peer GossipSub send queue.
fn spawn_loopback_request<F, Fut>(
    http: &reqwest::Client,
    inference_port: u16,
    request: infer_tunnel::TunnelRequest,
    on_response: F,
) where
    F: FnOnce(infer_tunnel::TunnelResponse) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let http = http.clone();
    tokio::spawn(async move {
        let response = infer_tunnel::serve_loopback(&http, inference_port, request).await;
        on_response(response).await;
    });
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

    #[test]
    fn an_announced_tunnel_peer_gets_a_dialable_peer_multiaddr() {
        let peer_id = PeerId::random();
        let addrs = tunnel_peer_multiaddrs(
            peer_id,
            &[
                "[2001:db8::42]:42777".to_string(),
                "192.0.2.42:42777".to_string(),
            ],
        );

        assert_eq!(
            addrs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec![
                format!("/ip6/2001:db8::42/tcp/52777/p2p/{peer_id}"),
                format!("/ip4/192.0.2.42/tcp/52777/p2p/{peer_id}"),
            ]
        );
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

    #[test]
    fn a_tcp_port_at_the_udp_offset_is_refused() {
        assert_eq!(
            observed_host_port(&"/ip4/203.0.113.5/tcp/10000".parse().unwrap()),
            None
        );
    }

    #[test]
    fn a_zero_udp_port_is_not_a_dialable_multiaddr() {
        assert!(udp_to_libp2p_multiaddr("[2001:db8::42]:0").is_none());
    }

    #[test]
    fn wildcard_udp_addresses_are_not_tunnel_destinations() {
        assert!(udp_to_libp2p_multiaddr("0.0.0.0:42778").is_none());
        assert!(udp_to_libp2p_multiaddr("[::]:42778").is_none());
        assert!(udp_to_libp2p_multiaddr("0.0.0.0:0").is_none());
        assert!(udp_to_libp2p_multiaddr("[::]:0").is_none());
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

    #[tokio::test]
    async fn slow_inference_forward_is_dispatched_off_the_swarm_loop() {
        use axum::{routing::get, Router};
        use std::time::Instant;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/health",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                "ok"
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (response_tx, response_rx) = oneshot::channel();
        let http = reqwest::Client::new();
        let started = Instant::now();
        spawn_loopback_request(
            &http,
            port,
            infer_tunnel::TunnelRequest::get("/health"),
            move |response| async move {
                let _ = response_tx.send(response);
            },
        );

        assert!(
            started.elapsed() < Duration::from_millis(50),
            "dispatch must return before the slow loopback request completes"
        );
        let response = tokio::time::timeout(Duration::from_secs(1), response_rx)
            .await
            .expect("background request should complete")
            .expect("background request should send a response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
    }
}
