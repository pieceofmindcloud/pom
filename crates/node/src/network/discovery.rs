//! The internet path of discovery: the node's iroh endpoint and everything
//! that rides on it.
//!
//! - **Gossip** (`iroh-gossip`) carries the `Announce` across sites, on one
//!   topic shared by every network; admission decides who enters the table.
//! - **mDNS** finds endpoints on the LAN.
//! - **Relays**: n0's public relays (unless `--no-default-relays`), the
//!   dedicated ones the operator configured (`relay_peers`), and the member
//!   relays other nodes of the cluster announce. iroh holds the connection to
//!   a home relay and punches the NAT on its own, coordinated over the relay.
//! - **An embedded relay server** on TCP `p2p_port + 10000`, which serves
//!   whom `RelayGate` admits: everyone on a public relay (`serve_relay`), the
//!   admitted members on a node the internet reaches.
//! - **Request/response protocols** (tunnel, cluster logs, chat, lobby,
//!   audit), one ALPN each; see `p2p.rs`.

use crate::cluster_logs::{self, ClusterLogs, ClusterLogsCodec, LogOutbound};
use crate::infer_tunnel::{self, InferTunnelCodec, TunnelOutbound};
use crate::network::{Network, P2PMessage, P2pLink, PeerInfo};
use crate::network_chat::{
    self, ChatOutbound, ChatRequest, ChatResponse, NetworkChat, NetworkChatCodec,
};
use crate::p2p::{self, Codec, EndpointAddr, EndpointId, LinkPath, P2p, RelayUrl, SecretKey};
use crate::reqlog::{self, AuditCodec, AuditEvent, AuditLog, AuditOutbound};
use anyhow::{Context, Result};
use futures::StreamExt;
use iroh::endpoint::Connection;
use iroh::Watcher;
use iroh_gossip::api::{Event as GossipEvent, GossipReceiver, GossipSender};
use iroh_gossip::{Gossip, TopicId};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const GOSSIP_TOPIC: &str = "pom/announce/1.0.0";

/// Bound of one gossip message. An announce is about 1.3 KB; iroh-gossip's
/// default (4 KiB) left little room for a node with many capabilities.
const GOSSIP_MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Domain of the announce signature, so the bytes signed here can never be
/// replayed as a signature of anything else the node key signs.
const ANNOUNCE_SIGNATURE_DOMAIN: &[u8] = b"pom/announce-signature/1:";

/// How often the relay policy, the gossip membership and the published
/// addresses are re-evaluated.
const POLICY_INTERVAL: Duration = Duration::from_secs(5);

/// How many member relays this node adds to its relay map at once.
const MEMBER_RELAYS: usize = 2;

fn topic_id() -> TopicId {
    TopicId::from_bytes(Sha256::digest(GOSSIP_TOPIC.as_bytes()).into())
}

fn announce_signed_bytes(payload: &[u8]) -> Vec<u8> {
    let mut message = ANNOUNCE_SIGNATURE_DOMAIN.to_vec();
    message.extend_from_slice(payload);
    message
}

/// A gossip message: the node key's signature over the announce, then the
/// announce itself.
///
/// iroh-gossip authenticates only the neighbor that **delivered** a message,
/// not its author: any node on the topic could forward an announce naming
/// someone else. The signature is what binds the announce to the endpoint it
/// declares (`endpoint_id`), the way gossipsub's signed messages did.
pub(crate) fn seal_announce(secret: &SecretKey, payload: &[u8]) -> Vec<u8> {
    let signature = secret.sign(&announce_signed_bytes(payload));
    let mut sealed = signature.to_bytes().to_vec();
    sealed.extend_from_slice(payload);
    sealed
}

/// The announce inside a gossip message, when `author` signed it.
pub(crate) fn open_announce<'a>(sealed: &'a [u8], author: &EndpointId) -> Option<&'a [u8]> {
    const SIGNATURE_LEN: usize = 64;
    if sealed.len() < SIGNATURE_LEN {
        return None;
    }
    let (signature, payload) = sealed.split_at(SIGNATURE_LEN);
    let signature: [u8; SIGNATURE_LEN] = signature.try_into().ok()?;
    let signature = iroh::Signature::from_bytes(&signature);
    author
        .verify(&announce_signed_bytes(payload), &signature)
        .ok()
        .map(|_| payload)
}

/// The relay map entry of a relay URL. QUIC address discovery is only asked
/// of HTTPS relays: a node's embedded relay (`http://`) does not serve it.
fn relay_config(url: &RelayUrl) -> Arc<iroh::RelayConfig> {
    let quic = if url.scheme() == "https" {
        Some(Default::default())
    } else {
        None
    };
    Arc::new(iroh::RelayConfig::new(url.clone(), quic))
}

/// The textual addresses worth publishing about this endpoint: relay URLs
/// always, direct addresses only when public. A private address goes out in
/// `addr_candidates` already, and mDNS covers the LAN.
fn publishable_addrs(addr: &EndpointAddr) -> Vec<String> {
    p2p::addr_texts(addr)
        .into_iter()
        .filter(|text| match text.parse::<SocketAddr>() {
            Ok(socket) => !crate::addrs::is_private_or_local(&socket.ip().to_string()),
            Err(_) => true,
        })
        .collect()
}

/// Access control of the embedded relay server: `RelayGate` decides, per
/// connecting endpoint.
#[derive(Debug)]
struct GateAccess(Arc<crate::network::RelayGate>);

impl iroh_relay::server::AccessControl for GateAccess {
    async fn on_connect(
        &self,
        request: &iroh_relay::server::ClientRequest,
    ) -> iroh_relay::server::Access {
        if self.0.allows(&request.endpoint_id().to_string()) {
            iroh_relay::server::Access::Allow
        } else {
            iroh_relay::server::Access::Deny {
                reason: Some("this relay serves the members of its own cluster".into()),
            }
        }
    }
}

/// Starts the embedded relay server on `port` (TCP, plain HTTP). The payload
/// it carries is end-to-end encrypted QUIC between the two endpoints; TLS on
/// the relay would protect only the relay protocol, and a node on a bare IP
/// has no certificate to offer.
async fn start_relay_server(
    port: u16,
    gate: Arc<crate::network::RelayGate>,
) -> Result<iroh_relay::server::Server> {
    let mut relay = iroh_relay::server::RelayConfig::new(SocketAddr::from((
        Ipv6Addr::UNSPECIFIED,
        port,
    )));
    relay.access = Arc::new(GateAccess(gate.clone()));
    let mut config = iroh_relay::server::ServerConfig::default();
    config.relay = Some(relay);
    match iroh_relay::server::Server::spawn(config).await {
        Ok(server) => Ok(server),
        Err(_) => {
            // A host without IPv6: the same server on IPv4 only.
            let mut relay = iroh_relay::server::RelayConfig::new(SocketAddr::from((
                Ipv4Addr::UNSPECIFIED,
                port,
            )));
            relay.access = Arc::new(GateAccess(gate));
            let mut config = iroh_relay::server::ServerConfig::default();
            config.relay = Some(relay);
            iroh_relay::server::Server::spawn(config)
                .await
                .map_err(|error| anyhow::anyhow!("relay server on port {port}: {error}"))
        }
    }
}

/// What every protocol handler and outbound call needs.
#[derive(Clone)]
struct Ctx {
    network: Arc<Network>,
    p2p: Arc<P2p>,
    chat: Arc<NetworkChat>,
    logs: Arc<ClusterLogs>,
    audit_log: Arc<AuditLog>,
    remote_audit_tx: mpsc::Sender<Vec<AuditEvent>>,
    /// Used only to forward an inbound tunnel request to this node's own
    /// servers on loopback - see `infer_tunnel::serve_loopback`.
    http: reqwest::Client,
}

/// Follows the paths of one connection for the screens and the traversal
/// counters, until it closes. A connection that starts on a relay and moves
/// to a direct path is a hole punch.
fn link_tracker(network: Arc<Network>) -> Arc<dyn Fn(&Connection) + Send + Sync> {
    Arc::new(move |connection: &Connection| {
        let network = network.clone();
        let connection = connection.clone();
        tokio::spawn(async move {
            let peer = connection.remote_id();
            let peer_text = peer.to_string();
            let id = connection.stable_id();
            let mut relayed_before = false;
            let mut hole_punched = false;
            let mut paths = connection.paths_stream();
            while let Some(list) = paths.next().await {
                let selected = list
                    .iter()
                    .find(|path| path.is_selected())
                    .and_then(|path| p2p::link_path(path.remote_addr()));
                match &selected {
                    Some(LinkPath::Relay(_)) => relayed_before = true,
                    Some(LinkPath::Direct(address)) if relayed_before && !hole_punched => {
                        hole_punched = true;
                        info!(
                            "Direct path to {} at {address} (hole punch succeeded)",
                            peer.fmt_short()
                        );
                        network.remember_hole_punch(Ok(()));
                    }
                    _ => {}
                }
                if let Some(path) = selected {
                    network.note_link(&peer_text, id, P2pLink::new(path, hole_punched));
                }
            }
            network.drop_link(&peer_text, id);
        });
    })
}

pub struct Discovery {
    pub announce_tx: mpsc::Sender<Vec<u8>>,
    pub dial_tx: mpsc::Sender<EndpointAddr>,
    ctx: Ctx,
    secret: SecretKey,
    udp_port: u16,
    relay_port: u16,
    /// Kept alive for the life of the loop: dropping the router closes the
    /// endpoint, and dropping the server stops relaying.
    router: iroh::protocol::Router,
    _gossip: Gossip,
    _relay_server: Option<iroh_relay::server::Server>,
    gossip_tx: GossipSender,
    gossip_rx: GossipReceiver,
    mdns: Option<iroh_mdns_address_lookup::MdnsAddressLookup>,
    dedicated_relays: Vec<RelayUrl>,
    announce_rx: mpsc::Receiver<Vec<u8>>,
    dial_rx: mpsc::Receiver<EndpointAddr>,
    tunnel_rx: mpsc::Receiver<TunnelOutbound>,
    log_rx: mpsc::Receiver<LogOutbound>,
    chat_rx: mpsc::Receiver<ChatOutbound>,
    audit_rx: mpsc::Receiver<AuditOutbound>,
}

impl Discovery {
    /// Binds the endpoint under the node key (`secret`), so its `EndpointId`
    /// is the node's public key and stays the same across restarts.
    ///
    /// `relay_urls` are the dedicated relays (`relay_peers`, which includes the
    /// trust document's `relay_hints`); `use_default_relays` adds n0's public
    /// relays. `tunnel_rx` is created by the caller (`main.rs`): the gateway's
    /// HTTP server holds the matching sender before this function runs.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        udp_port: u16,
        relay_urls: &[String],
        use_default_relays: bool,
        network: Arc<Network>,
        secret: SecretKey,
        tunnel_rx: mpsc::Receiver<TunnelOutbound>,
        log_rx: mpsc::Receiver<LogOutbound>,
        logs: Arc<ClusterLogs>,
        chat_rx: mpsc::Receiver<ChatOutbound>,
        chat: Arc<NetworkChat>,
        audit_rx: mpsc::Receiver<AuditOutbound>,
        audit_log: Arc<AuditLog>,
        plugins: Arc<crate::plugins::PluginManager>,
    ) -> Result<Self> {
        let port = udp_port.saturating_add(p2p::P2P_PORT_OFFSET);

        let mut dedicated_relays = Vec::new();
        for text in relay_urls {
            match p2p::relay_url(text) {
                Some(url) => dedicated_relays.push(url),
                None => warn!("relay_peers: {text} is not a relay URL (https://... or http://...)"),
            }
        }
        let relay_map = if use_default_relays {
            iroh::RelayMode::Default.relay_map()
        } else {
            iroh::RelayMap::empty()
        };
        for url in &dedicated_relays {
            relay_map.insert(url.clone(), relay_config(url));
        }
        network.set_dedicated_relays(dedicated_relays.iter().map(|url| url.to_string()));

        let book = iroh::address_lookup::MemoryLookup::new();
        let endpoint = Self::bind(port, &secret, &relay_map, &book).await?;
        if let Some(pinned) = network.advertised_host() {
            let host = pinned.trim_start_matches('[').trim_end_matches(']');
            if let Ok(ip) = host.parse::<std::net::IpAddr>() {
                endpoint.add_external_addr(SocketAddr::new(ip, port)).await;
            }
        }

        // mDNS is always on: `advertise_addr` is a priority hint, not
        // isolation, and the LAN is still the shortest path between two nodes
        // on the same network.
        let mdns = match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(endpoint.id())
        {
            Ok(mdns) => {
                if let Ok(services) = endpoint.address_lookup() {
                    services.add(mdns.clone());
                }
                Some(mdns)
            }
            Err(error) => {
                warn!("mDNS off: {error}");
                None
            }
        };

        let p2p = P2p::new(endpoint.clone(), book);
        let tracker = link_tracker(network.clone());
        p2p.set_on_connection(tracker.clone());

        let (remote_audit_tx, mut remote_audit_rx) =
            mpsc::channel::<Vec<AuditEvent>>(reqlog::AUDIT_TRANSPORT_QUEUE_CAPACITY);
        let remote_audit_log = audit_log.clone();
        tokio::spawn(async move {
            while let Some(events) = remote_audit_rx.recv().await {
                for event in events {
                    if !plugins
                        .ingest_audit_event(crate::plugins::LOGS_PLUGIN_CODE, event.clone())
                        .await
                        && !remote_audit_log.try_ingest_remote_ordered(event)
                    {
                        warn!("received audit entered neither the plugin nor the local fallback");
                    }
                }
            }
        });

        let ctx = Ctx {
            network: network.clone(),
            p2p: p2p.clone(),
            chat,
            logs,
            audit_log,
            remote_audit_tx,
            http: reqwest::Client::new(),
        };

        let gossip = Gossip::builder()
            .max_message_size(GOSSIP_MAX_MESSAGE_BYTES)
            .spawn(endpoint.clone());
        let router = Self::router(&endpoint, &gossip, &ctx, tracker);
        let topic = gossip
            .subscribe(topic_id(), Vec::new())
            .await
            .context("gossip subscription")?;
        let (gossip_tx, gossip_rx) = topic.split();

        let relay_server = match start_relay_server(port, network.relay_gate()).await {
            Ok(server) => {
                info!("p2p: relay server on TCP {port}");
                Some(server)
            }
            Err(error) => {
                warn!("p2p: no relay server, this node will not relay for its cluster: {error:#}");
                None
            }
        };
        network.relay_gate().set_available(relay_server.is_some());

        info!(
            "p2p: endpoint {} on UDP {port}, {} relay(s) configured",
            endpoint.id(),
            relay_map.len()
        );

        let (announce_tx, announce_rx) = mpsc::channel(64);
        let (dial_tx, dial_rx) = mpsc::channel(64);
        Ok(Discovery {
            announce_tx,
            dial_tx,
            ctx,
            secret,
            udp_port,
            relay_port: port,
            router,
            _gossip: gossip,
            _relay_server: relay_server,
            gossip_tx,
            gossip_rx,
            mdns,
            dedicated_relays,
            announce_rx,
            dial_rx,
            tunnel_rx,
            log_rx,
            chat_rx,
            audit_rx,
        })
    }

    /// Binds on both stacks, falling back to IPv4 alone on a host without
    /// IPv6.
    async fn bind(
        port: u16,
        secret: &SecretKey,
        relay_map: &iroh::RelayMap,
        book: &iroh::address_lookup::MemoryLookup,
    ) -> Result<iroh::Endpoint> {
        let builder = || {
            iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
                .secret_key(secret.clone())
                .relay_mode(iroh::RelayMode::Custom(relay_map.clone()))
                .address_lookup(book.clone())
        };
        let dual = builder()
            .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?
            .bind_addr(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)))?
            .bind()
            .await;
        match dual {
            Ok(endpoint) => Ok(endpoint),
            Err(error) => {
                debug!("p2p: dual-stack bind failed ({error}), trying IPv4 only");
                Ok(builder()
                    .bind_addr(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?
                    .bind()
                    .await
                    .with_context(|| format!("binding the endpoint on UDP {port}"))?)
            }
        }
    }

    fn router(
        endpoint: &iroh::Endpoint,
        gossip: &Gossip,
        ctx: &Ctx,
        tracker: Arc<dyn Fn(&Connection) + Send + Sync>,
    ) -> iroh::protocol::Router {
        let tunnel = {
            let ctx = ctx.clone();
            p2p::Serve::new(InferTunnelCodec, move |connection: Connection, request| {
                let ctx = ctx.clone();
                async move { answer_tunnel_request(&ctx, &connection, request).await }
            })
            .on_connection(tracker.clone())
        };
        let lobby = {
            let ctx = ctx.clone();
            p2p::Serve::new(
                crate::network_lobby::LobbyCodec,
                move |connection: Connection, request| {
                    let ctx = ctx.clone();
                    async move {
                        answer_lobby_request(
                            &ctx,
                            &ctx.network,
                            &ctx.chat,
                            connection.remote_id(),
                            request,
                        )
                        .await
                    }
                },
            )
            .on_connection(tracker.clone())
        };
        let chat = {
            let ctx = ctx.clone();
            p2p::Serve::new(NetworkChatCodec, move |connection: Connection, request| {
                let ctx = ctx.clone();
                async move { answer_chat_request(&ctx, connection.remote_id(), request).await }
            })
            .on_connection(tracker.clone())
        };
        let logs = {
            let ctx = ctx.clone();
            p2p::Serve::new(ClusterLogsCodec, move |connection: Connection, request| {
                let ctx = ctx.clone();
                async move {
                    let accepted = match ctx.network.live_node_of(&connection.remote_id()).await {
                        Some(sender) => ctx.logs.try_ingest_remote(&sender, request),
                        None => false,
                    };
                    if !accepted {
                        debug!(
                            "cluster-logs: batch refused from {}",
                            connection.remote_id().fmt_short()
                        );
                    }
                    cluster_logs::LogResponse { accepted }
                }
            })
            .on_connection(tracker.clone())
        };
        let audit = {
            let ctx = ctx.clone();
            p2p::Serve::new(AuditCodec, move |connection: Connection, request| {
                let ctx = ctx.clone();
                async move {
                    let ordered = match ctx.network.live_node_of(&connection.remote_id()).await {
                        Some(sender) => ctx.audit_log.order_remote_events(&sender, request),
                        None => None,
                    };
                    let accepted = match ordered {
                        Some(events) if events.is_empty() => true,
                        Some(events) => match ctx.remote_audit_tx.try_send(events) {
                            Ok(()) => true,
                            Err(error) => error
                                .into_inner()
                                .into_iter()
                                .all(|event| ctx.audit_log.try_ingest_remote_ordered(event)),
                        },
                        None => false,
                    };
                    if !accepted {
                        debug!(
                            "request-audit: event refused from {}",
                            connection.remote_id().fmt_short()
                        );
                    }
                    reqlog::AuditResponse { accepted }
                }
            })
            .on_connection(tracker)
        };
        iroh::protocol::Router::builder(endpoint.clone())
            .accept(iroh_gossip::ALPN, gossip.clone())
            .accept(InferTunnelCodec::PROTOCOL.as_bytes(), tunnel)
            .accept(crate::network_lobby::LobbyCodec::PROTOCOL.as_bytes(), lobby)
            .accept(NetworkChatCodec::PROTOCOL.as_bytes(), chat)
            .accept(ClusterLogsCodec::PROTOCOL.as_bytes(), logs)
            .accept(AuditCodec::PROTOCOL.as_bytes(), audit)
            .spawn()
    }

    pub async fn run(self) {
        let Discovery {
            announce_tx: _,
            dial_tx: _,
            ctx,
            secret,
            udp_port,
            relay_port,
            router,
            _gossip,
            _relay_server,
            gossip_tx,
            mut gossip_rx,
            mdns,
            dedicated_relays,
            mut announce_rx,
            mut dial_rx,
            mut tunnel_rx,
            mut log_rx,
            mut chat_rx,
            mut audit_rx,
        } = self;
        let endpoint = router.endpoint().clone();
        let mut mdns_events: std::pin::Pin<
            Box<dyn futures::Stream<Item = iroh_mdns_address_lookup::DiscoveryEvent> + Send>,
        > = match &mdns {
            Some(mdns) => Box::pin(mdns.subscribe().await),
            None => Box::pin(futures::stream::pending()),
        };
        let mut policy = PolicyState::default();
        let mut policy_tick = tokio::time::interval(POLICY_INTERVAL);
        policy_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let network = ctx.network.clone();
        loop {
            tokio::select! {
                event = gossip_rx.next() => {
                    match event {
                        Some(Ok(GossipEvent::Received(message))) => {
                            handle_announce(&ctx, &message.content).await;
                        }
                        Some(Ok(GossipEvent::NeighborUp(peer))) => {
                            debug!("gossip: neighbor up {}", peer.fmt_short());
                        }
                        Some(Ok(GossipEvent::NeighborDown(peer))) => {
                            debug!("gossip: neighbor down {}", peer.fmt_short());
                            policy.joined.remove(&peer);
                        }
                        Some(Ok(GossipEvent::Lagged)) => debug!("gossip: receiver lagged"),
                        Some(Err(error)) => warn!("gossip: {error}"),
                        None => break,
                    }
                }
                msg = announce_rx.recv() => {
                    let Some(data) = msg else { break };
                    let sealed = seal_announce(&secret, &data);
                    if let Err(error) = gossip_tx.broadcast(sealed.into()).await {
                        debug!("gossip broadcast: {error}");
                    }
                }
                addr = dial_rx.recv() => {
                    let Some(addr) = addr else { break };
                    // Already connected or backing off: no new dial.
                    if ctx.p2p.is_connected(&addr.id) || ctx.p2p.backing_off(&addr.id) {
                        continue;
                    }
                    info!("pkarr: dialling {}", addr.id.fmt_short());
                    ctx.p2p.learn(addr.id, &p2p::addr_texts(&addr));
                    join_gossip(&gossip_tx, &mut policy, addr.id).await;
                }
                event = mdns_events.next() => {
                    if let Some(iroh_mdns_address_lookup::DiscoveryEvent::Discovered { endpoint_info, .. }) = event {
                        let id = endpoint_info.endpoint_id;
                        if !network.is_detached() && id != endpoint.id() {
                            info!("mDNS: {}", id.fmt_short());
                            join_gossip(&gossip_tx, &mut policy, id).await;
                        }
                    }
                }
                _ = policy_tick.tick() => {
                    apply_policy(
                        &ctx,
                        &endpoint,
                        &gossip_tx,
                        &dedicated_relays,
                        udp_port,
                        relay_port,
                        &mut policy,
                    ).await;
                }
                out = tunnel_rx.recv() => {
                    let Some(TunnelOutbound { peer_id, addr_candidates, request, reply }) = out else { break };
                    let transport = ctx.p2p.clone();
                    tokio::spawn(async move {
                        let result = transport
                            .call(InferTunnelCodec, peer_id, &addr_candidates, request)
                            .await
                            .map_err(infer_tunnel::TunnelDeliveryError::from);
                        if let Err(error) = &result {
                            debug!("infer-tunnel: outbound call to {} failed: {error}", peer_id.fmt_short());
                        }
                        let _ = reply.send(result);
                    });
                }
                out = log_rx.recv() => {
                    let Some(LogOutbound { peer_id, addr_candidates, request }) = out else { break };
                    // A batch of logs never dials a peer that is backing off:
                    // that is what closed the loop of dials every 100 ms.
                    if ctx.p2p.backing_off(&peer_id) && !ctx.p2p.is_connected(&peer_id) {
                        ctx.logs.try_send_dropped(request.entries.len() as u64 + request.dropped);
                        continue;
                    }
                    let transport = ctx.p2p.clone();
                    tokio::spawn(async move {
                        if let Err(error) = transport
                            .call(ClusterLogsCodec, peer_id, &addr_candidates, request)
                            .await
                        {
                            debug!("cluster-logs: send to {} failed: {error}", peer_id.fmt_short());
                        }
                    });
                }
                out = chat_rx.recv() => {
                    let Some(ChatOutbound { request, reply }) = out else { break };
                    if !ctx.chat.enabled() {
                        let _ = reply.send(ChatResponse::rejected("network_chat_disabled"));
                        continue;
                    }
                    if network.is_principal() {
                        let result =
                            accept_original_chat(&ctx, &network, &ctx.chat, request, None).await;
                        let _ = reply.send(result);
                        continue;
                    }
                    let Some((peer_id, addr_candidates)) = network.principal_peer().await else {
                        let _ = reply.send(ChatResponse::rejected("principal_unavailable"));
                        continue;
                    };
                    let transport = ctx.p2p.clone();
                    tokio::spawn(async move {
                        let response = match transport
                            .call(NetworkChatCodec, peer_id, &addr_candidates, request)
                            .await
                        {
                            Ok(response) => response,
                            Err(error) => ChatResponse::rejected(error.to_string()),
                        };
                        let _ = reply.send(response);
                    });
                }
                out = audit_rx.recv() => {
                    let Some(AuditOutbound { peer_id, addr_candidates, event }) = out else { break };
                    let transport = ctx.p2p.clone();
                    tokio::spawn(async move {
                        if let Err(error) = transport
                            .call(AuditCodec, peer_id, &addr_candidates, event)
                            .await
                        {
                            debug!("request-audit: send to {} failed: {error}", peer_id.fmt_short());
                        }
                    });
                }
            }
        }
        let _ = router.shutdown().await;
    }
}

/// What the periodic policy round remembers between rounds.
#[derive(Default)]
struct PolicyState {
    /// Endpoints asked to join the gossip topic and not since seen leaving.
    joined: HashSet<EndpointId>,
    /// Member relays this node added to its relay map.
    member_relays: HashSet<RelayUrl>,
}

async fn join_gossip(gossip: &GossipSender, policy: &mut PolicyState, peer: EndpointId) {
    if policy.joined.insert(peer) {
        if let Err(error) = gossip.join_peers(vec![peer]).await {
            debug!("gossip: join {}: {error}", peer.fmt_short());
            policy.joined.remove(&peer);
        }
    }
}

/// One policy round: whom the relay server admits and under which URL this
/// node offers it, which member relays this node uses, which peers the gossip
/// topic should reach, what the `Announce` publishes as this endpoint's
/// addresses, and what the NAT report says about reachability.
#[allow(clippy::too_many_arguments)]
async fn apply_policy(
    ctx: &Ctx,
    endpoint: &iroh::Endpoint,
    gossip: &GossipSender,
    dedicated: &[RelayUrl],
    udp_port: u16,
    relay_port: u16,
    policy: &mut PolicyState,
) {
    let network = &ctx.network;
    let peers = network.alive_peers().await;
    let gate = network.relay_gate();
    gate.set_members(peers.iter().filter_map(|peer| peer.endpoint_id.clone()));

    // Reachability, from the NAT report: the public address the relays saw
    // is one of this machine's own addresses, so nothing translates it.
    if let Some(report) = endpoint.net_report().get() {
        if let Some(global) = report.global_v4 {
            let ip = global.ip().to_string();
            network.set_observed_addr(&format!("{ip}:{udp_port}"));
            let public = crate::addrs::local_addrs()
                .iter()
                .any(|local| local.ip == ip);
            network.set_nat_status(if public { "public" } else { "private" });
            gate.set_reachable(public);
        }
    }
    let offer_url = network
        .effective_host()
        .filter(|host| {
            !crate::addrs::is_private_or_local(host.trim_start_matches('[').trim_end_matches(']'))
        })
        .map(|host| format!("http://{host}:{relay_port}"));
    gate.set_url(offer_url);

    // Member relays: the ones the table announces, beside the dedicated ones,
    // up to a cap. A dedicated relay is a public one and carries no file
    // transfer, so a node behind NAT needs a member relay as well.
    let wanted: Vec<RelayUrl> = peers
        .iter()
        .filter(|peer| peer.node_id != network.node_id)
        .filter_map(|peer| peer.relay.as_ref()?.url.as_deref().and_then(p2p::relay_url))
        .filter(|url| !dedicated.contains(url))
        .take(MEMBER_RELAYS)
        .collect();
    for url in &wanted {
        if policy.member_relays.insert(url.clone()) {
            info!("p2p: using member relay {url}");
            endpoint.insert_relay(url.clone(), relay_config(url)).await;
        }
    }
    let stale: Vec<RelayUrl> = policy
        .member_relays
        .iter()
        .filter(|url| !wanted.contains(url))
        .cloned()
        .collect();
    for url in stale {
        endpoint.remove_relay(&url).await;
        policy.member_relays.remove(&url);
    }
    network.set_relays_in_use(
        endpoint
            .home_relay_status()
            .get()
            .iter()
            .filter(|status| status.is_connected())
            .map(|status| status.url().to_string())
            .collect(),
    );

    // Gossip membership: every admitted peer this node has not asked yet.
    if !network.is_detached() {
        for peer in &peers {
            let Some(id) = peer.endpoint_id.as_deref().and_then(p2p::parse_endpoint_id) else {
                continue;
            };
            if id == endpoint.id() {
                continue;
            }
            ctx.p2p.learn(id, &peer.endpoint_dialable_addrs());
            join_gossip(gossip, policy, id).await;
        }
    }

    let addrs = publishable_addrs(&endpoint.addr());
    if network.set_endpoint_addrs(addrs.clone()) {
        info!("p2p: announcing {} address(es): {:?}", addrs.len(), addrs);
    }
}

/// An `Announce` that came over gossip.
async fn handle_announce(ctx: &Ctx, sealed: &[u8]) {
    let network = &ctx.network;
    // Off the network (discovery off, or taken off by its operator): nothing
    // a peer says is heard, and nobody is dialed because of it.
    if network.is_detached() {
        return;
    }
    // The author is the endpoint the announce declares; its signature has to
    // verify before anything in it is believed.
    const SIGNATURE_LEN: usize = 64;
    let Some(payload) = sealed.get(SIGNATURE_LEN..) else {
        return;
    };
    let Ok(message) = serde_json::from_slice::<P2PMessage>(payload) else {
        return;
    };
    let P2PMessage::Announce {
        node_id,
        deploy_id,
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
        inference_quic_port,
        inference_frame_capabilities,
        gateway_port,
        gateway_quic_port,
        profile,
        tls,
        quic,
        hostname,
        capabilities,
        disk_total_mb,
        disk_free_mb,
        endpoint_id,
        endpoint_addrs,
        tp,
        runtime_capacity,
        cluster_logs_enabled,
        idle,
        network_chat_enabled,
        cluster_id,
        trust_version,
        membership,
        relay,
        ..
    } = message
    else {
        return;
    };
    let Some(author) = endpoint_id.as_deref().and_then(p2p::parse_endpoint_id) else {
        warn!("p2p: dropped an Announce without an endpoint id");
        return;
    };
    if open_announce(sealed, &author).is_none() {
        warn!(
            "p2p: dropped an Announce from {} whose signature does not verify",
            author.fmt_short()
        );
        return;
    }
    if author == ctx.p2p.id() {
        return;
    }

    let peer = PeerInfo {
        deploy_id,
        node_id: node_id.clone(),
        pubkey: pubkey.clone(),
        cluster_id,
        trust_version,
        membership,
        relay,
        // The gossip path has no source address to rewrite, and the announced
        // one is usually the literal `0.0.0.0`. With candidates there is
        // something to use: the first of them is a real address, and
        // `0.0.0.0` only survives when the peer announced none.
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
        inference_quic_port,
        inference_frame_capabilities,
        gateway_port,
        gateway_quic_port,
        profile,
        tls,
        hostname,
        capabilities,
        connection_verified: false,
        disk_total_mb,
        disk_free_mb,
        api_keys_active: 0,
        endpoint_id: endpoint_id.clone(),
        endpoint_addrs,
        tp,
        runtime_capacity,
        idle,
    };
    network.remember_public_discovery_peer(&peer).await;
    // Never `admit_peer` here: the probe may need the tunnel, and a probe per
    // announce would stall the gossip loop. Without a verdict in the cache,
    // the probe runs on another task and the peer's next announce finds the
    // answer. See `admit_in_background`.
    match network.admission_without_probe(&peer).await {
        Some(true) => {}
        Some(false) => {
            debug!(
                "p2p: dropped an Announce from {} without proof of membership",
                crate::network::short(&node_id)
            );
            return;
        }
        None => {
            debug!(
                "p2p: Announce from {} waits for the admission probe",
                crate::network::short(&node_id)
            );
            network.admit_in_background(peer);
            return;
        }
    }
    let tunnel_capable = peer
        .inference_frame_capabilities
        .iter()
        .any(|c| c == p2p::TUNNEL_CAPABILITY);
    let dial_addrs = peer.endpoint_dialable_addrs();
    network.insert_announced_peer(peer).await;

    network.trigger_cluster_logs_probe(&node_id, &pubkey, cluster_logs_enabled, tls);
    network.trigger_quic_probe(&node_id, &pubkey, quic, tls);
    network.trigger_network_chat_probe(&node_id, &pubkey, network_chat_enabled, tls);
    network.trigger_network_settings_probe(&node_id, &pubkey, tls);

    ctx.p2p.learn(author, &dial_addrs);
    // Warm the tunnel connection ahead of ever needing it: the relay path and
    // the hole punch have time to settle in the background now, instead of
    // adding that latency cold onto the very first real inference request.
    if tunnel_capable && !ctx.p2p.is_connected(&author) && !ctx.p2p.backing_off(&author) {
        let transport = ctx.p2p.clone();
        tokio::spawn(async move {
            if let Err(error) = transport
                .connect(author, &dial_addrs, InferTunnelCodec::PROTOCOL)
                .await
            {
                debug!("p2p: could not warm tunnel peer {}: {error}", author.fmt_short());
            }
        });
    }
}

/// One request that arrived through the tunnel, answered by one of this node's
/// own servers on loopback.
async fn answer_tunnel_request(
    ctx: &Ctx,
    connection: &Connection,
    request: infer_tunnel::TunnelRequest,
) -> infer_tunnel::TunnelResponse {
    let network = &ctx.network;
    let peer = connection.remote_id();
    // The destination is chosen by the sender and **resolved here**: the
    // remote peer names one of this node's two servers, never a port. On the
    // gateway, the closed list of routes is checked before forwarding - see
    // `gateway_path_is_allowed`, which explains why a tunnel open to the
    // gateway would be a privilege escalation.
    let target = match request.target {
        infer_tunnel::TunnelTarget::Inference => network
            .inference_port
            .ok_or((503u16, "no inference server on this node".to_string())),
        infer_tunnel::TunnelTarget::Gateway => {
            if infer_tunnel::is_transfer_data_path(&request.path)
                && !network
                    .path_carries_transfers(p2p::selected_path(connection).as_ref())
                    .await
            {
                // File bytes never cross a public relay. The relay only sees
                // an encrypted stream, so the two ends are the ones that hold
                // the rule.
                debug!(
                    "infer-tunnel: refused a file chunk from {}: the connection crosses a relay that carries no transfers",
                    peer.fmt_short()
                );
                Err((
                    infer_tunnel::TRANSFER_ROUTE_REFUSED_STATUS,
                    "this connection carries no file transfers".to_string(),
                ))
            } else if infer_tunnel::is_transfer_data_path(&request.path)
                && !infer_tunnel::transfer_chunk_is_bounded(&request.headers)
            {
                Err((
                    416u16,
                    "a file chunk over the tunnel needs a bounded range".to_string(),
                ))
            } else if infer_tunnel::gateway_path_is_allowed(&request.path) {
                network
                    .gateway_port
                    .ok_or((503u16, "no gateway on this node".to_string()))
            } else {
                warn!(
                    "infer-tunnel: {} asked for route {} through the gateway; not on the list",
                    peer.fmt_short(),
                    request.path
                );
                Err((403u16, "route not carried by the tunnel".to_string()))
            }
        }
    };
    match target {
        Ok(port) => {
            debug!(
                "infer-tunnel: {} → loopback :{} {}",
                peer.fmt_short(),
                port,
                request.path
            );
            // Who is calling, as the endpoint's handshake proved it; the
            // gateway and the inference server authorize by it.
            let mut request = request;
            request.headers.retain(|(name, _)| {
                !name.eq_ignore_ascii_case(infer_tunnel::TUNNEL_PEER_HEADER)
            });
            request
                .headers
                .push((infer_tunnel::TUNNEL_PEER_HEADER.to_string(), peer.to_string()));
            infer_tunnel::serve_loopback(&ctx.http, port, request).await
        }
        // Nothing to forward to: this node has no such server running (no
        // layer assignment yet, a pure-relay node), or the route is not one
        // the tunnel carries. Answer rather than dropping the stream, so the
        // caller's fallback logic gets a definite failure instead of a timeout.
        Err((status, message)) => infer_tunnel::TunnelResponse {
            status,
            body: message.into_bytes(),
        },
    }
}

/// Network Chat: the principal validates an original message and relays it;
/// a member accepts what the principal relays.
async fn answer_chat_request(ctx: &Ctx, peer: EndpointId, request: ChatRequest) -> ChatResponse {
    let network = &ctx.network;
    let chat = &ctx.chat;
    let peer_text = peer.to_string();
    let sender = network
        .alive_peers()
        .await
        .into_iter()
        .find(|candidate| candidate.endpoint_id.as_deref() == Some(peer_text.as_str()));
    if request.relayed {
        let source_is_principal = sender
            .as_ref()
            .is_some_and(|candidate| network.peer_is_principal(candidate));
        if !source_is_principal || !chat.enabled() {
            return ChatResponse::rejected("relay_not_authorized");
        }
        return match chat.accept(request.action.clone()) {
            Ok(true) => {
                relay_chat(ctx, network, chat, request.action, Some(peer)).await;
                ChatResponse::accepted()
            }
            Ok(false) => ChatResponse::rejected("network_chat_disabled"),
            Err(error) => ChatResponse::rejected(error.to_string()),
        };
    }
    match sender {
        Some(sender) => {
            accept_original_chat(ctx, network, chat, request, Some((peer, sender.node_id))).await
        }
        None => ChatResponse::rejected("sender_unknown"),
    }
}

async fn accept_original_chat(
    ctx: &Ctx,
    network: &Network,
    chat: &Arc<NetworkChat>,
    request: ChatRequest,
    source: Option<(EndpointId, String)>,
) -> ChatResponse {
    if !chat.enabled() {
        return ChatResponse::rejected("network_chat_disabled");
    }
    if source.is_some() && !network.is_principal() {
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
            if network.is_principal() {
                relay_chat(
                    ctx,
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

/// A node's admission record as it is answered to that node, with the grant
/// when it is approved.
async fn lobby_admission_view(
    network: &Network,
    chat: &Arc<NetworkChat>,
    node_id: &str,
) -> Option<crate::network_lobby::AdmissionView> {
    // The caller's own request, and the grant once an admin approved it. The
    // grant leaves only here: to the node whose signature was just verified,
    // on the connection it was verified on.
    match chat.admission_of(node_id) {
        Some(record) => {
            let grant = if record.status == crate::network_lobby::AdmissionStatus::Approved {
                let (connection_key, relay_peers, settings) = chat.grant_secrets().await;
                network.trust_document_json().map(|trust_document| {
                    crate::network_lobby::AdmissionGrant {
                        trust_document,
                        connection_key,
                        relay_peers,
                        network: Some(settings),
                    }
                })
            } else {
                None
            };
            Some(crate::network_lobby::AdmissionView {
                status: record.status,
                role: record.role,
                grant,
                retry_after_secs: crate::network_lobby::admission_retry_after(
                    &record,
                    chrono::Utc::now(),
                ),
            })
        }
        None => None,
    }
}

/// Answers one lobby request from a machine that is not a member.
///
/// Nothing in the frame identifies the sender: `node_id` comes from the
/// `EndpointId` the QUIC handshake proved. A text becomes an ordinary `lobby`
/// room message and is relayed to the members through the star; the answer is
/// the recent history of that room, which is public by definition.
async fn answer_lobby_request(
    ctx: &Ctx,
    network: &Network,
    chat: &Arc<NetworkChat>,
    peer: EndpointId,
    request: crate::network_lobby::LobbyRequest,
) -> crate::network_lobby::LobbyResponse {
    use crate::network_lobby::{
        GuestSeen, LobbyLimits, LobbyMessage, LobbyRequest, LobbyResponse, LOBBY_ROOM,
    };
    // A chat request names its sender with the node key's signature over this
    // transport. The public reads carry no identity: they are rate limited by
    // the transport peer and say nothing about who asked.
    let caller = peer.to_string();
    // What a node tells about public networks is public data and has nothing
    // to do with the lobby or with being the principal: any node answers,
    // from the board the gateway keeps.
    // A principal telling a relay about its own network. The relay keeps it
    // only when the node key that signed for this transport is the primary
    // of the trust document being published.
    if let LobbyRequest::Publish {
        identity,
        network: listed,
    } = &request
    {
        if !chat.directory_board().allow(&caller) {
            return LobbyResponse::rejected("rate_limited");
        }
        if identity.verified_node_id(&peer).is_none() {
            return LobbyResponse::rejected("identity_invalid");
        }
        let Some(listed) = crate::gateway::validated_publication(listed.clone(), &identity.pubkey)
        else {
            return LobbyResponse::rejected("publication_invalid");
        };
        return if chat.directory_board().accept_publication(listed) {
            LobbyResponse {
                accepted: true,
                ..LobbyResponse::default()
            }
        } else {
            LobbyResponse::rejected("directory_disabled")
        };
    }
    // A node saying it is leaving, over the lobby connection. Acted on only
    // for the node whose signature over this transport verifies, and only
    // when the principal knows it: on the member list, or currently admitted.
    if let LobbyRequest::Leave { identity } = &request {
        if !chat.directory_board().allow(&caller) {
            return LobbyResponse::rejected("rate_limited");
        }
        if !network.is_principal() {
            return LobbyResponse::rejected("not_principal");
        }
        let Some(node_id) = identity.verified_node_id(&peer) else {
            return LobbyResponse::rejected("identity_invalid");
        };
        let known = chat.admission_of(&node_id).is_some()
            || network
                .alive_peers()
                .await
                .iter()
                .any(|member| member.node_id == node_id);
        if !known {
            return LobbyResponse::rejected("not_a_member");
        }
        chat.member_departed(&node_id);
        chat.request_rotation(&node_id);
        return LobbyResponse {
            accepted: true,
            node_id,
            ..LobbyResponse::default()
        };
    }
    // A member that missed a key rotation asks for the current key. The
    // answer is the same grant an approval carries, and it goes only to a
    // node on the member list whose signature over this transport verifies.
    if let LobbyRequest::ClaimKey { identity } = &request {
        if !chat.directory_board().allow(&caller) {
            return LobbyResponse::rejected("rate_limited");
        }
        if !network.is_principal() {
            return LobbyResponse::rejected("not_principal");
        }
        let Some(node_id) = identity.verified_node_id(&peer) else {
            return LobbyResponse::rejected("identity_invalid");
        };
        if chat.node_is_blocked(&node_id).await {
            return LobbyResponse::rejected("node_blocked");
        }
        let admission = lobby_admission_view(network, chat, &node_id).await;
        return LobbyResponse {
            accepted: true,
            node_id,
            admission,
            ..LobbyResponse::default()
        };
    }
    if matches!(request, LobbyRequest::Describe | LobbyRequest::Directory) {
        use crate::network_lobby::BoardAnswer;
        if !chat.directory_board().allow(&caller) {
            return LobbyResponse::rejected("rate_limited");
        }
        let answer = if matches!(request, LobbyRequest::Describe) {
            chat.directory_board().own()
        } else {
            chat.directory_board().known()
        };
        return match answer {
            BoardAnswer::Ready(networks) => LobbyResponse {
                accepted: true,
                networks,
                ..LobbyResponse::default()
            },
            BoardAnswer::Pending => LobbyResponse {
                accepted: true,
                pending: true,
                ..LobbyResponse::default()
            },
            BoardAnswer::Disabled => LobbyResponse::rejected("directory_disabled"),
        };
    }
    // The published stats answer under their own switch and leave no
    // presence: asking what a network runs is not entering its lobby.
    if matches!(request, LobbyRequest::Info) {
        if !network.is_principal() || !chat.public_stats_enabled().await {
            return LobbyResponse::rejected("stats_disabled");
        }
        if !chat.public_stats().allow(&caller) {
            return LobbyResponse::rejected("rate_limited");
        }
        return LobbyResponse {
            accepted: true,
            error: None,
            info: chat.public_stats().current(),
            ..LobbyResponse::default()
        };
    }
    if !network.is_principal() || !chat.lobby_enabled().await {
        return LobbyResponse::rejected("lobby_disabled");
    }
    let proof = match &request {
        LobbyRequest::Post { identity, .. }
        | LobbyRequest::Fetch { identity, .. }
        | LobbyRequest::Admission { identity, .. } => identity,
        LobbyRequest::Info
        | LobbyRequest::Describe
        | LobbyRequest::Directory
        | LobbyRequest::ClaimKey { .. }
        | LobbyRequest::Leave { .. }
        | LobbyRequest::Publish { .. } => {
            unreachable!("answered above")
        }
    };
    let Some(node_id) = proof.verified_node_id(&peer) else {
        return LobbyResponse::rejected("identity_invalid");
    };
    if chat.node_is_blocked(&node_id).await {
        return LobbyResponse::rejected("node_blocked");
    }
    let peers = network.alive_peers().await;
    // A member has the authenticated room; the lobby path is for outsiders.
    if node_id == network.node_id || peers.iter().any(|member| member.node_id == node_id) {
        return LobbyResponse::rejected("already_member");
    }
    let mut card = match &request {
        LobbyRequest::Post { card, .. }
        | LobbyRequest::Fetch { card, .. }
        | LobbyRequest::Admission { card, .. } => card.clone().sanitized(),
        LobbyRequest::Info
        | LobbyRequest::Describe
        | LobbyRequest::Directory
        | LobbyRequest::ClaimKey { .. }
        | LobbyRequest::Leave { .. }
        | LobbyRequest::Publish { .. } => {
            unreachable!("answered above")
        }
    };
    // How the candidate reaches this node is what this node sees, never what
    // the candidate says: an admin weighs it when deciding on admission.
    let links = network.p2p_links(&peer.to_string());
    card.relayed = (!links.is_empty()).then(|| links.iter().all(|link| link.relayed));
    let is_post = matches!(
        request,
        LobbyRequest::Post { .. } | LobbyRequest::Admission { .. }
    );
    let seen = match chat.lobby_guests().admit(&node_id, &card, is_post) {
        Ok(seen) => seen,
        Err(error) => return LobbyResponse::rejected(error),
    };
    // The conversation runs on the network Chat; without it the lobby only
    // takes admission requests.
    let messages_closed = !chat.enabled();
    if seen == GuestSeen::Announce && !messages_closed {
        let presence = network_chat::ChatAction::LobbyPresence {
            message_id: uuid::Uuid::new_v4().simple().to_string(),
            room_id: LOBBY_ROOM.to_string(),
            sender_node_id: node_id.clone(),
            recipient_node_id: None,
            card: card.clone(),
            sent_at: chrono::Utc::now(),
        };
        relay_chat(ctx, network, chat, presence, None).await;
    }
    if let LobbyRequest::Admission { role, .. } = &request {
        if !chat.accepts_role(*role).await {
            return LobbyResponse::rejected("role_not_accepted");
        }
        match chat.request_admission(&node_id, card.clone(), *role) {
            Ok(crate::network_lobby::AdmissionStatus::Pending) if chat.auto_approves().await => {
                chat.decide_admission(
                    &node_id,
                    Some(crate::network_lobby::AdmissionStatus::Approved),
                );
            }
            Ok(_) => {}
            Err(error) => return LobbyResponse::rejected(error),
        }
    }
    let admission = lobby_admission_view(network, chat, &node_id).await;
    if let LobbyRequest::Post {
        message_id, text, ..
    } = request
    {
        if messages_closed {
            return LobbyResponse::rejected("lobby_messages_closed");
        }
        let text = text.trim().to_string();
        if text.is_empty() || text.len() > LobbyLimits::TEXT_BYTES {
            return LobbyResponse::rejected("text_invalid");
        }
        if message_id.is_empty()
            || message_id.len() > LobbyLimits::MESSAGE_ID_BYTES
            || !message_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return LobbyResponse::rejected("message_id_invalid");
        }
        // Namespaced by the sender so a candidate cannot shadow (the insert
        // ignores a repeated id) a message someone else is about to send.
        let action = network_chat::ChatAction::Text {
            message_id: format!("lobby-{}-{message_id}", &node_id[..node_id.len().min(16)]),
            room_id: LOBBY_ROOM.to_string(),
            sender_node_id: node_id.clone(),
            recipient_node_id: None,
            text,
            // The principal's clock: a candidate does not order the room.
            sent_at: chrono::Utc::now(),
        };
        match chat.accept(action.clone()) {
            Ok(true) => relay_chat(ctx, network, chat, action, None).await,
            Ok(false) => return LobbyResponse::rejected("lobby_disabled"),
            Err(error) => return LobbyResponse::rejected(error.to_string()),
        }
    }
    let history = match chat.history(LOBBY_ROOM, LobbyLimits::HISTORY) {
        Ok(history) => history,
        Err(error) => return LobbyResponse::rejected(error.to_string()),
    };
    let messages = history
        .into_iter()
        .filter(|event| event.kind == "text" && event.recipient_node_id.is_none())
        .map(|event| {
            let member = if event.sender_node_id == network.node_id {
                Some(network.hostname())
            } else {
                peers
                    .iter()
                    .find(|member| member.node_id == event.sender_node_id)
                    .map(|member| member.hostname.clone())
            };
            let sender_name = member
                .clone()
                .or_else(|| chat.lobby_guests().hostname(&event.sender_node_id))
                .unwrap_or_default();
            LobbyMessage {
                message_id: event.message_id,
                sender_node_id: event.sender_node_id,
                sender_name,
                member: member.is_some(),
                text: event.text.unwrap_or_default(),
                sent_at: event.sent_at,
            }
        })
        .collect();
    LobbyResponse {
        accepted: true,
        error: None,
        node_id,
        messages,
        admission,
        messages_closed,
        ..LobbyResponse::default()
    }
}

/// Sends `action` to every live member except `excluded_peer`, each call on
/// its own task: a member that is slow or unreachable holds up nobody.
async fn relay_chat(
    ctx: &Ctx,
    network: &Network,
    chat: &Arc<NetworkChat>,
    action: network_chat::ChatAction,
    excluded_peer: Option<EndpointId>,
) {
    let peers = network.alive_peers().await;
    let excluded_peer_id = excluded_peer.map(|id| id.to_string());
    for peer in peers {
        if peer.node_id == network.node_id
            || peer.endpoint_id.as_deref() == excluded_peer_id.as_deref()
            || action
                .recipient_node_id()
                .is_some_and(|recipient| recipient != peer.node_id)
            || chat.node_is_blocked(&peer.node_id).await
        {
            continue;
        }
        let Some(peer_id) = peer.endpoint_id.as_deref().and_then(p2p::parse_endpoint_id) else {
            continue;
        };
        let addresses = peer.endpoint_dialable_addrs();
        let request = ChatRequest {
            action: action.clone(),
            api_key: String::new(),
            relayed: true,
        };
        let transport = ctx.p2p.clone();
        let node_id = peer.node_id.clone();
        tokio::spawn(async move {
            if let Err(error) = transport
                .call(NetworkChatCodec, peer_id, &addresses, request)
                .await
            {
                debug!("network-chat: peer {node_id} unavailable: {error}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SecretKey {
        SecretKey::from_bytes(&[seed; 32])
    }

    /// iroh-gossip authenticates only the neighbor that delivered a message.
    /// The signature is what binds an announce to the endpoint it names.
    #[test]
    fn an_announce_opens_only_for_its_author() {
        let author = key(1);
        let other = key(2);
        let sealed = seal_announce(&author, b"{\"type\":\"Announce\"}");
        assert_eq!(
            open_announce(&sealed, &author.public()),
            Some(&b"{\"type\":\"Announce\"}"[..])
        );
        assert_eq!(open_announce(&sealed, &other.public()), None);

        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(open_announce(&tampered, &author.public()), None);
        assert_eq!(open_announce(&sealed[..10], &author.public()), None);
    }

    /// Every network shares one topic; admission, not the topic, decides who
    /// enters the table. Two builds must agree on it.
    #[test]
    fn the_topic_is_stable() {
        assert_eq!(topic_id(), topic_id());
        assert_eq!(
            topic_id(),
            TopicId::from_bytes(Sha256::digest(b"pom/announce/1.0.0").into())
        );
    }

    /// A private direct address is not published: it only wastes a dial from
    /// the internet. A relay URL always is.
    #[test]
    fn only_public_direct_addresses_and_relays_are_published() {
        let id = key(3).public();
        let addr = p2p::endpoint_addr(
            id,
            &[
                "192.168.1.10:52777".to_string(),
                "203.0.113.7:52777".to_string(),
                "https://relay.example.org/".to_string(),
            ],
        );
        let published = publishable_addrs(&addr);
        assert_eq!(
            published,
            vec![
                "203.0.113.7:52777".to_string(),
                "https://relay.example.org/".to_string()
            ]
        );
    }

    /// A node's own relay is plain HTTP and serves no QUIC address discovery.
    #[test]
    fn quic_discovery_is_asked_only_of_https_relays() {
        let https = p2p::relay_url("https://relay.example.org").unwrap();
        let http = p2p::relay_url("http://203.0.113.7:52777").unwrap();
        assert!(relay_config(&https).quic.is_some());
        assert!(relay_config(&http).quic.is_none());
    }
}
