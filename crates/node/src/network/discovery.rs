use crate::cluster_logs::{self, ClusterLogs, ClusterLogsCodec, LogOutbound};
use crate::infer_tunnel::{self, InferTunnelCodec, TunnelOutbound};
use crate::network::{is_valid_destination, Network, P2PMessage, PeerInfo};
use crate::network_chat::{
    self, ChatOutbound, ChatRequest, ChatResponse, NetworkChat, NetworkChatCodec,
};
use crate::reqlog::{self, AuditCodec, AuditEvent, AuditLog, AuditOutbound};
use anyhow::Result;
use futures::StreamExt;
use libp2p::{
    autonat, dcutr, gossipsub, identify, kad, mdns, noise, relay,
    request_response::{self, OutboundRequestId},
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour, SwarmEvent},
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

/// Built-in public relays.
///
/// **The list is empty on purpose.** It used to carry three public IPFS
/// bootstrappers. A bootstrapper is not a relay: they speak Kademlia and identify,
/// they do not grant a Circuit Relay v2 reservation to an arbitrary peer, and the
/// addresses pinned here were already stale. The result was a reservation that was
/// never accepted, with no error saying so - and, without a reservation, DCuTR never
/// runs, because the hole punch is coordinated over a relayed connection that
/// did not exist. See `docs/subsystems/p2p-network.md`, section "NAT traversal".
///
/// A useful relay for this project is a pom node with a public IP running
/// `serve_relay`: only it honors `max_circuit_bytes` of the size of a prefill
/// frame. Configure with `--relay-addr`.
const DEFAULT_RELAY_NODES: &[&str] = &[];

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
pub(crate) fn udp_to_libp2p_multiaddr(addr: &str) -> Option<Multiaddr> {
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
/// Dialable multiaddresses of a peer, from what it announced.
///
/// Accepts both forms: a full multiaddress (`/ip4/.../quic-v1`,
/// `/ip4/<relay>/.../p2p-circuit/...`), which is what `Announce.libp2p_addrs`
/// carries, and the legacy `host:port`, derived by the `UDP+10000` convention. A
/// circuit only exists in the first form.
///
/// The trailing `/p2p/<peer-id>` is appended when missing. In a circuit
/// address it identifies the destination **after** the relay, which already appears before
/// the `/p2p-circuit`; appending it again would make the swarm dial the relay as if
/// it were the destination.
fn tunnel_peer_multiaddrs(peer_id: PeerId, advertised: &[String]) -> Vec<Multiaddr> {
    use libp2p::multiaddr::Protocol;

    let mut seen = HashSet::new();
    advertised
        .iter()
        .filter_map(|addr| {
            addr.parse::<Multiaddr>()
                .ok()
                .filter(|ma| ma.iter().next().is_some())
                .or_else(|| udp_to_libp2p_multiaddr(addr))
        })
        .map(|addr| {
            if matches!(addr.iter().last(), Some(Protocol::P2p(pid)) if pid == peer_id) {
                addr
            } else {
                addr.with(Protocol::P2p(peer_id))
            }
        })
        .filter(|addr| seen.insert(addr.clone()))
        .collect()
}

/// Wait between redials per peer, after a dial that failed.
///
/// Without it, with the tunnel down, every batch of cluster logs (every
/// 100 ms) and every call through the tunnel became a new dial: each one opened
/// or tried to open a circuit on the relay until it refused everything ("resource limit
/// exceeded"), and the record of each failure entered the next batch of logs -
/// a self-sustaining loop. The wait doubles on every failure, from
/// `DIAL_BACKOFF_FLOOR` up to `DIAL_BACKOFF_CEILING`, and an established connection
/// resets it.
#[derive(Default)]
struct DialBackoff {
    peers: HashMap<PeerId, (u32, std::time::Instant)>,
}

const DIAL_BACKOFF_FLOOR: Duration = Duration::from_millis(250);
const DIAL_BACKOFF_CEILING: Duration = Duration::from_secs(10);

impl DialBackoff {
    /// The peer is backing off: it is not worth dialing again now.
    fn blocked(&self, peer: &PeerId) -> bool {
        self.peers
            .get(peer)
            .is_some_and(|(_, until)| *until > std::time::Instant::now())
    }

    /// Records a failure and returns how many in a row there are by now.
    fn failed(&mut self, peer: PeerId) -> u32 {
        let entry = self
            .peers
            .entry(peer)
            .or_insert((0, std::time::Instant::now()));
        entry.0 = entry.0.saturating_add(1);
        let wait = DIAL_BACKOFF_FLOOR
            .saturating_mul(1u32 << entry.0.saturating_sub(1).min(8))
            .min(DIAL_BACKOFF_CEILING);
        entry.1 = std::time::Instant::now() + wait;
        entry.0
    }

    fn connected(&mut self, peer: &PeerId) {
        self.peers.remove(peer);
    }
}

/// Rehydrates the swarm's address book before a call through the tunnel.
///
/// The announce may have arrived over UDP and the `PeerId` may have been passed to the
/// gateway before the matching gossipsub event registered the addresses in the
/// swarm. `request_response::send_request` does not dial without a known address:
/// it fails immediately with `Failed to dial the requested peer`. Consulting the
/// live table here closes that window and also covers a swarm that lost the
/// address after a reconnection.
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
            return Err(format!("no live announce for PeerId {peer_id}"));
        };
        peer.libp2p_dialable_addrs()
    } else {
        advertised_override.to_vec()
    };
    let addresses = tunnel_peer_multiaddrs(peer_id, &advertised);
    if addresses.is_empty() {
        return Err(format!("PeerId {peer_id} has no dialable address"));
    }
    for address in addresses {
        swarm.add_peer_address(peer_id, address);
    }
    if !swarm.is_connected(&peer_id) {
        // The request-response behaviour can queue the request while this dial
        // completes. `AlreadyDialing` is harmless; the important part is that
        // the address book is populated before `send_request` runs.
        if let Err(error) = swarm.dial(peer_id) {
            debug!("libp2p: redial of peer {peer_id} did not start: {error}");
        }
    }
    Ok(())
}

/// Collects from the swarm the addresses another node could dial and publishes them in the
/// `Announce` (`Network::set_libp2p_addrs`).
///
/// Three sources, all from the swarm itself and none invented here:
/// the confirmed external addresses (AutoNAT), the listeners with a public IP, and
/// the circuit listeners - which are the whole point of the exercise, because it is the
/// only address a node behind CGNAT has to offer.
///
/// A private, loopback or unspecified address is left out: publishing it only
/// wastes a dial that will fail. The exception is the circuit, whose IP is the
/// relay's and does not need to be public for this node to be reachable through it.
fn publish_swarm_addrs(swarm: &Swarm<DiscBehaviour>, network: &Network) {
    use libp2p::multiaddr::Protocol;

    let local = *swarm.local_peer_id();
    let is_circuit = |ma: &Multiaddr| ma.iter().any(|p| matches!(p, Protocol::P2pCircuit));
    let is_dialable = |ma: &Multiaddr| {
        if is_circuit(ma) {
            return true;
        }
        ma.iter().any(|p| match p {
            Protocol::Ip4(v4) => {
                !v4.is_unspecified() && !crate::addrs::is_private_or_local(&v4.to_string())
            }
            Protocol::Ip6(v6) => {
                !v6.is_unspecified() && !crate::addrs::is_private_or_local(&v6.to_string())
            }
            Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) => true,
            _ => false,
        })
    };

    let mut seen = HashSet::new();
    let mut addrs = Vec::new();
    for ma in swarm
        .external_addresses()
        .cloned()
        .chain(swarm.listeners().cloned())
    {
        if !is_dialable(&ma) {
            continue;
        }
        let ma = if matches!(ma.iter().last(), Some(Protocol::P2p(pid)) if pid == local) {
            ma
        } else {
            ma.with(Protocol::P2p(local))
        };
        let text = ma.to_string();
        if seen.insert(text.clone()) {
            addrs.push(text);
        }
    }

    if network.set_libp2p_addrs(addrs.clone()) {
        info!(
            "libp2p: announcing {} multiaddress(es): {:?}",
            addrs.len(),
            addrs
        );
    }
}

/// How often the relay policy is re-evaluated.
const RELAY_POLICY_INTERVAL: Duration = Duration::from_secs(5);
/// How many member relays a node keeps a reservation on at once.
const MEMBER_RELAY_RESERVATIONS: usize = 2;

/// Where this node holds circuit reservations.
///
/// A dedicated relay is a configured one (`relay_peers`, which includes the
/// trust document's `relay_hints`): a public relay. A member relay is a node
/// of this cluster that announces a relay offer (`PeerInfo::relay`). The two
/// are kept **side by side**: a public relay carries no file transfer, so a
/// node behind NAT needs a reservation on a member relay as well for a file
/// to reach it when the hole punch does not close.
struct RelayPolicy {
    /// Circuit listeners requested from each relay. The key being present means
    /// "already asked", so identify does not open a repeated listener.
    listeners: HashMap<PeerId, Vec<libp2p::core::transport::ListenerId>>,
    /// Relays with an accepted reservation and a listener still alive.
    accepted: HashSet<PeerId>,
}

impl RelayPolicy {
    fn new() -> Self {
        Self {
            listeners: HashMap::new(),
            accepted: HashSet::new(),
        }
    }

    /// Whether one more relay that is not dedicated may be asked.
    fn member_slot_free(&self, dedicated: &HashSet<PeerId>) -> bool {
        self.listeners
            .keys()
            .filter(|peer| !dedicated.contains(peer))
            .count()
            < MEMBER_RELAY_RESERVATIONS
    }

    /// A circuit listener closed: that relay's reservation ended.
    fn listener_closed(&mut self, listener: libp2p::core::transport::ListenerId) {
        let owner = self
            .listeners
            .iter()
            .find(|(_, ids)| ids.contains(&listener))
            .map(|(peer, _)| *peer);
        if let Some(peer) = owner {
            let ids = self.listeners.get_mut(&peer).expect("owner found above");
            ids.retain(|id| *id != listener);
            if ids.is_empty() {
                self.listeners.remove(&peer);
                self.accepted.remove(&peer);
            }
        }
    }
}

/// What the peers table says about `peer_id` as a relay: `Some(true)` for a
/// member announcing a relay offer, `Some(false)` for a member that does not
/// relay, `None` for a node that is not a member (a relay outside the
/// network, reached as a bootstrap).
async fn member_relay_offer(network: &Network, peer_id: &PeerId) -> Option<bool> {
    let text = peer_id.to_string();
    network
        .alive_peers()
        .await
        .iter()
        .find(|peer| peer.libp2p_peer_id.as_deref() == Some(text.as_str()))
        .map(|peer| peer.relay.is_some())
}

/// Reapplies the relay policy: tells the relay server who the members are,
/// asks connected dedicated relays that lost their reservation to reserve
/// again, and reserves on the member relays the peers table announces.
async fn apply_relay_policy(
    swarm: &mut Swarm<DiscBehaviour>,
    network: &Network,
    relay_peer_addrs: &HashSet<Multiaddr>,
    dedicated: &HashSet<PeerId>,
    connected_relays: &HashSet<PeerId>,
    dialed_addrs: &HashMap<PeerId, Multiaddr>,
    relays: &mut RelayPolicy,
) {
    let peers = network.alive_peers().await;
    network
        .relay_gate()
        .set_members(peers.iter().filter_map(|peer| peer.libp2p_peer_id.clone()));
    for peer in connected_relays {
        if !relays.listeners.contains_key(peer) {
            reserve_circuit(
                swarm,
                network,
                *peer,
                relay_peer_addrs,
                dialed_addrs.get(peer),
                relays,
            )
            .await;
        }
    }
    let members: Vec<PeerId> = peers
        .iter()
        .filter(|peer| peer.relay.is_some())
        .filter_map(|peer| peer.libp2p_peer_id.as_deref()?.parse().ok())
        .filter(|peer| !dedicated.contains(peer))
        .collect();
    for peer in members {
        if !relays.member_slot_free(dedicated) {
            break;
        }
        if relays.listeners.contains_key(&peer) || !swarm.is_connected(&peer) {
            continue;
        }
        info!("libp2p: reserving a circuit on member relay {peer}");
        reserve_circuit(
            swarm,
            network,
            peer,
            relay_peer_addrs,
            dialed_addrs.get(&peer),
            relays,
        )
        .await;
    }
}

/// Requests a Circuit Relay v2 reservation on this relay.
///
/// The circuit address is `<relay>/p2p-circuit`; `listen_on` on it makes the swarm
/// open the reservation and start announcing the relayed address. Without that address
/// there is no relayed connection, and without a relayed connection DCuTR has nothing over which to
/// coordinate the hole punch - this was the missing link.
///
/// The relay's dialable address comes from `relay_reservation_addrs`: the
/// configured list, the live announce, and lastly the address this node dialed.
/// The identify `listen_addrs` stay out, per this folder's "only Announce
/// dials" policy.
async fn reserve_circuit(
    swarm: &mut Swarm<DiscBehaviour>,
    network: &Network,
    peer_id: PeerId,
    relay_peer_addrs: &HashSet<Multiaddr>,
    dialed: Option<&Multiaddr>,
    relays: &mut RelayPolicy,
) {
    if relays.listeners.contains_key(&peer_id) {
        return;
    }
    relays.listeners.insert(peer_id, Vec::new());
    let peer_id_text = peer_id.to_string();
    let announced = network
        .alive_peers()
        .await
        .into_iter()
        .find(|peer| peer.libp2p_peer_id.as_deref() == Some(peer_id_text.as_str()))
        .map(|peer| peer.libp2p_dialable_addrs())
        .unwrap_or_default();
    let addresses = relay_reservation_addrs(peer_id, relay_peer_addrs, &announced, dialed);
    if addresses.is_empty() {
        relays.listeners.remove(&peer_id);
        debug!("libp2p: relay {peer_id} has no dialable address to reserve a circuit");
        return;
    }
    for address in addresses {
        let circuit = address.with(libp2p::multiaddr::Protocol::P2pCircuit);
        match swarm.listen_on(circuit.clone()) {
            Ok(listener) => {
                info!("libp2p: reserving a circuit at {circuit}");
                relays.listeners.entry(peer_id).or_default().push(listener);
            }
            Err(error) => {
                debug!("libp2p: circuit reservation at {circuit} did not start: {error}")
            }
        }
    }
}

/// Where to reserve a circuit on a relay, in order of authority.
///
/// 1. The `--relay-peers` relay, resolved by pkarr.
/// 2. The live announce's addresses, when the relay is an admitted member of the network.
/// 3. The address **this node** dialed to reach the relay - the bootstrap the
///    operator configured, for example. Without it, a dedicated relay, which is not a
///    member of the network it serves, was nowhere to be found: the reservation never
///    happened, and the bootstrap on the relay was not enough, contrary to what the
///    documentation said. It does not violate the "only Announce dials" policy: it is an
///    address we already dialed, not one the peer reported about itself.
fn relay_reservation_addrs(
    peer_id: PeerId,
    relay_peer_addrs: &HashSet<Multiaddr>,
    announced: &[String],
    dialed: Option<&Multiaddr>,
) -> Vec<Multiaddr> {
    let configured = relay_peer_addrs.iter().find(|ma| {
        ma.iter()
            .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2p(pid) if pid == peer_id))
    });
    if let Some(ma) = configured {
        return vec![ma.clone()];
    }
    let announced = tunnel_peer_multiaddrs(peer_id, announced);
    if !announced.is_empty() {
        return announced;
    }
    dialed
        .map(|address| tunnel_peer_multiaddrs(peer_id, &[address.to_string()]))
        .unwrap_or_default()
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

/// Announces stay far below this; only an unusually large gossip message
/// still gets IDONTWANT.
const GOSSIP_IDONTWANT_THRESHOLD_BYTES: usize = 64 * 1024;

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
    mdns: Toggle<mdns::tokio::Behaviour>,
    identify: identify::Behaviour,
    relay: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    autonat: autonat::Behaviour,
    relay_server: relay::Behaviour,
    infer_tunnel: request_response::Behaviour<InferTunnelCodec>,
    cluster_logs: request_response::Behaviour<ClusterLogsCodec>,
    network_chat: request_response::Behaviour<NetworkChatCodec>,
    network_lobby: request_response::Behaviour<crate::network_lobby::LobbyCodec>,
    audit: request_response::Behaviour<AuditCodec>,
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
    audit_rx: mpsc::Receiver<AuditOutbound>,
    audit_log: Arc<AuditLog>,
    remote_audit_tx: mpsc::Sender<Vec<AuditEvent>>,
    relay_peer_addrs: HashSet<Multiaddr>,
    relay_peer_ids: HashSet<PeerId>,
    connected_relays: HashSet<PeerId>,
    /// Peers for which `listen_on(.../p2p-circuit)` was already requested.
    /// A repeated `listen_on` opens a new listener on every call; the identify
    /// of the same relay arrives many times.
    relays: RelayPolicy,
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
        relay_keys: &[String],
        use_default_relays: bool,
        network: Arc<Network>,
        keypair: libp2p::identity::Keypair,
        tunnel_rx: mpsc::Receiver<TunnelOutbound>,
        log_rx: mpsc::Receiver<LogOutbound>,
        logs: Arc<ClusterLogs>,
        chat_rx: mpsc::Receiver<ChatOutbound>,
        chat: Arc<NetworkChat>,
        audit_rx: mpsc::Receiver<AuditOutbound>,
        audit_log: Arc<AuditLog>,
        plugins: Arc<crate::plugins::PluginManager>,
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

        let relay_gate = network.relay_gate();
        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )?
            // QUIC is not a latency optimization here: it is what makes DCuTR
            // work. The TCP hole punch depends on simultaneous open, which
            // most home NATs and practically every CGNAT refuse;
            // the UDP one is the path DCuTR was designed to use.
            .with_quic()
            .with_dns()?
            .with_relay_client(noise::Config::new, yamux::Config::default)?
            .with_behaviour(|key, relay_client| {
                let gossipsub_cfg = gossipsub::ConfigBuilder::default()
                    .heartbeat_interval(Duration::from_secs(1))
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    // Without this, gossipsub silently stops carrying
                    // announces. See `announce_message_id`.
                    .message_id_fn(announce_message_id)
                    // IDONTWANT saves bandwidth only for large messages. The
                    // default threshold (1000 bytes) is below the size of a
                    // typical announce (~1.3 KB), so every announce made each
                    // node queue an IDONTWANT for every mesh peer; a peer
                    // behind a slow relay or tunnel then filled its send queue
                    // and the log repeated "Send Queue full" thousands of times.
                    .idontwant_message_size_threshold(GOSSIP_IDONTWANT_THRESHOLD_BYTES)
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

                // mDNS is always on: `advertise_addr` is a priority hint, not
                // isolation, and the LAN is still the shortest path between two
                // nodes on the same network.
                let mdns = Some(
                    mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)
                        .expect("mdns init"),
                )
                .into();

                // No address cache: by default identify keeps the
                // `listen_addrs` each peer reports and offers them for dialing.
                // A peer on the host's network reports 127.0.0.1, and a node on a Docker
                // bridge dialed its own loopback - "Unexpected peer ID",
                // itself. Only the `Announce` says where to dial.
                let identify = identify::Behaviour::new(
                    identify::Config::new("/pom/1.0.0".to_string(), key.public())
                        .with_cache_size(0),
                );

                // The relay server always has capacity; who gets a
                // reservation is decided per request by `RelayGate`: everyone
                // on a public relay (`serve_relay`), the admitted members of
                // this cluster on a node the internet reaches, nobody
                // otherwise. A circuit's source is not checked: a candidate
                // that is not a member yet has to reach a member behind NAT.
                let gate = relay_gate.clone();
                let mut relay_server_cfg = relay::Config {
                    max_reservations: 128,
                    max_reservations_per_peer: 4,
                    reservation_duration: Duration::from_secs(3600),
                    // Both limits apply to the whole circuit, not
                    // per request: libp2p sums the bytes of both directions
                    // since opening (`CopyFuture`) and closes the circuit
                    // when it passes the cap or the deadline. The old cap, the
                    // size of a prefill frame, dropped the circuit
                    // after roughly one frame, and the 120 s deadline
                    // dropped it anyway: a peer that only the relay
                    // reaches lost the route in minutes and left the table.
                    // No byte cap (`0` = unlimited) and a long deadline;
                    // the reservation keeps renewing every hour.
                    max_circuit_duration: Duration::from_secs(24 * 60 * 60),
                    max_circuit_bytes: 0,
                    // libp2p's defaults are 16 circuits in total and 4 per
                    // peer. Circuits here live for a day, so a handful of
                    // nodes behind NAT filled them and every later dial
                    // through the relay - a member reaching a member, a
                    // candidate reaching a lobby - was refused with
                    // "resource limit exceeded".
                    max_circuits: 1024,
                    max_circuits_per_peer: 32,
                    // libp2p's default circuit-source limits (one circuit
                    // per 2 min per peer, burst 30; one per minute per IP,
                    // burst 60) are shared by every node behind the same NAT,
                    // and each of them redials a peer only the relay reaches
                    // over TCP and QUIC. They drained the bucket and the relay
                    // refused every later circuit from that IP, the lobby's
                    // included, with "resource limit exceeded". Still
                    // bounded, but sized for a site of nodes.
                    circuit_src_rate_limiters: Vec::new(),
                    ..relay::Config::default()
                }
                .circuit_src_per_peer(
                    std::num::NonZeroU32::new(60).expect("60 > 0"),
                    Duration::from_secs(5),
                )
                .circuit_src_per_ip(
                    std::num::NonZeroU32::new(512).expect("512 > 0"),
                    Duration::from_secs(1),
                );
                // Added to libp2p's own rate limits, which stay.
                relay_server_cfg.reservation_rate_limiters.push(Box::new(
                    move |peer: PeerId, _: &Multiaddr, _: std::time::Instant| {
                        gate.allows_reservation(&peer.to_string())
                    },
                ));

                let relay_server = relay::Behaviour::new(peer_id, relay_server_cfg);

                let infer_tunnel = infer_tunnel::infer_tunnel_behaviour();
                let cluster_logs = cluster_logs::cluster_logs_behaviour();
                let network_chat = network_chat::network_chat_behaviour();
                let network_lobby = crate::network_lobby::network_lobby_behaviour();
                let audit = reqlog::audit_behaviour();

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
                    network_lobby,
                    audit,
                }
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        let listen_addr: Multiaddr = format!("/ip4/0.0.0.0/tcp/{}", libp2p_port).parse()?;
        swarm.listen_on(listen_addr)?;
        if let Ok(v6_addr) = format!("/ip6/::/tcp/{}", libp2p_port).parse::<Multiaddr>() {
            swarm.listen_on(v6_addr).ok();
        }
        // Same port as TCP, on the other side of the stack: the peer discovers both
        // through identify and DCuTR picks whichever it manages to punch.
        if let Ok(v4_quic) =
            format!("/ip4/0.0.0.0/udp/{}/quic-v1", libp2p_port).parse::<Multiaddr>()
        {
            swarm.listen_on(v4_quic).ok();
        }
        if let Ok(v6_quic) = format!("/ip6/::/udp/{}/quic-v1", libp2p_port).parse::<Multiaddr>() {
            swarm.listen_on(v6_quic).ok();
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

        // A dedicated relay is identified by its key, not its address: a relay
        // whose pkarr record did not resolve at startup is still dedicated
        // when the cold dial (`main::spawn_pkarr_bootstrap`) reaches it.
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
            .chain(
                relay_keys
                    .iter()
                    .filter_map(|key| crate::pkarr_node::peer_id_from_uri(key)),
            )
            .collect();
        network.set_dedicated_relays(relay_peer_ids.iter().map(PeerId::to_string));

        for ma in &relay_peer_addrs {
            info!("libp2p: dialling relay {}", ma);
            swarm.dial(ma.clone()).ok();
        }

        let (announce_tx, announce_rx) = mpsc::channel(64);
        let (dial_tx, dial_rx) = mpsc::channel(64);
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
            audit_rx,
            audit_log,
            remote_audit_tx,
            relay_peer_addrs,
            relay_peer_ids,
            connected_relays: HashSet::new(),
            relays: RelayPolicy::new(),
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
            mut audit_rx,
            audit_log,
            remote_audit_tx,
            relay_peer_addrs,
            relay_peer_ids,
            mut connected_relays,
            mut relays,
            http,
        } = self;
        // Outstanding outbound tunnel calls, keyed by the id the swarm
        // assigns on `send_request` — resolved in `handle_event` when the
        // matching `Message::Response` or `Event::OutboundFailure` arrives.
        // Lives here, not on `Discovery`, because only this loop ever touches
        // it — same reasoning as `connected_relays`.
        let mut pending_tunnel: HashMap<
            OutboundRequestId,
            oneshot::Sender<
                Result<infer_tunnel::TunnelResponse, infer_tunnel::TunnelDeliveryError>,
            >,
        > = HashMap::new();
        let mut pending_chat: HashMap<OutboundRequestId, oneshot::Sender<ChatResponse>> =
            HashMap::new();
        // Last address dialed per peer, for `reserve_circuit`. Only this
        // loop touches it, like `pending_tunnel`.
        let mut dialed_addrs: HashMap<PeerId, Multiaddr> = HashMap::new();
        let mut dial_backoff = DialBackoff::default();
        let (inbound_response_tx, mut inbound_response_rx) =
            mpsc::channel::<InboundTunnelResponse>(64);
        let mut relay_policy_tick = tokio::time::interval(RELAY_POLICY_INTERVAL);
        relay_policy_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                event = swarm.next() => {
                    let Some(ev) = event else { break };
                    handle_event(&mut swarm, &network, ev, &relay_peer_addrs, &relay_peer_ids, &mut connected_relays, &mut relays, &mut dialed_addrs, &http, &mut pending_tunnel, &mut pending_chat, &inbound_response_tx, &logs, &chat, &audit_log, &remote_audit_tx, &mut dial_backoff).await;
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
                    // The target is the trailing `/p2p/<id>` (in a circuit, the
                    // peer after the relay). Already connected or backing off:
                    // no new dial.
                    let target = match addr.iter().last() {
                        Some(libp2p::multiaddr::Protocol::P2p(peer)) => Some(peer),
                        _ => None,
                    };
                    if target.is_some_and(|peer| swarm.is_connected(&peer) || dial_backoff.blocked(&peer)) {
                        continue;
                    }
                    info!("pkarr: dialling {}", addr);
                    if let Err(e) = swarm.dial(addr) {
                        warn!("pkarr dial: {}", e);
                    }
                }
                _ = relay_policy_tick.tick() => {
                    apply_relay_policy(&mut swarm, &network, &relay_peer_addrs, &relay_peer_ids, &connected_relays, &dialed_addrs, &mut relays).await;
                }
                out = tunnel_rx.recv() => {
                    let Some(TunnelOutbound { peer_id, addr_candidates, request, reply }) = out else { break };
                    if !swarm.is_connected(&peer_id) {
                        // Backing off: fails immediately, and the caller (the route
                        // plan, the delivery) moves on to the next path.
                        if dial_backoff.blocked(&peer_id) {
                            let _ = reply.send(Err(infer_tunnel::TunnelDeliveryError::before_request(
                                "peer on hold after dials that failed",
                            )));
                            continue;
                        }
                        if let Err(error) = ensure_tunnel_peer_address(&mut swarm, &network, peer_id, &addr_candidates).await {
                            let _ = reply.send(Err(
                                infer_tunnel::TunnelDeliveryError::before_request(error),
                            ));
                            continue;
                        }
                    }
                    let request_id = swarm.behaviour_mut().infer_tunnel.send_request(&peer_id, request);
                    pending_tunnel.insert(request_id, reply);
                }
                out = log_rx.recv() => {
                    let Some(LogOutbound { peer_id, addr_candidates, request }) = out else { break };
                    if !swarm.is_connected(&peer_id) {
                        // A batch of logs never dials a peer that is backing off: that is
                        // what closed the loop of dials every 100 ms.
                        if dial_backoff.blocked(&peer_id) {
                            logs.try_send_dropped(request.entries.len() as u64 + request.dropped);
                            continue;
                        }
                        if let Err(error) = ensure_tunnel_peer_address(&mut swarm, &network, peer_id, &addr_candidates).await {
                            debug!("cluster-logs: peer {peer_id} unavailable: {error}");
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
                    if network.is_principal() {
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
                out = audit_rx.recv() => {
                    let Some(AuditOutbound { peer_id, addr_candidates, event }) = out else { break };
                    if !swarm.is_connected(&peer_id) {
                        if let Err(error) = ensure_tunnel_peer_address(&mut swarm, &network, peer_id, &addr_candidates).await {
                            debug!("request-audit: peer {peer_id} unavailable: {error}");
                            continue;
                        }
                    }
                    let _ = swarm.behaviour_mut().audit.send_request(&peer_id, event);
                }
            }
        }
    }
}

async fn handle_event(
    swarm: &mut Swarm<DiscBehaviour>,
    network: &Arc<Network>,
    event: SwarmEvent<DiscBehaviourEvent>,
    relay_peer_addrs: &HashSet<Multiaddr>,
    relay_peer_ids: &HashSet<PeerId>,
    connected_relays: &mut HashSet<PeerId>,
    relays: &mut RelayPolicy,
    dialed_addrs: &mut HashMap<PeerId, Multiaddr>,
    http: &reqwest::Client,
    pending_tunnel: &mut HashMap<
        OutboundRequestId,
        oneshot::Sender<Result<infer_tunnel::TunnelResponse, infer_tunnel::TunnelDeliveryError>>,
    >,
    pending_chat: &mut HashMap<OutboundRequestId, oneshot::Sender<ChatResponse>>,
    inbound_response_tx: &mpsc::Sender<InboundTunnelResponse>,
    logs: &ClusterLogs,
    chat: &Arc<NetworkChat>,
    audit_log: &AuditLog,
    remote_audit_tx: &mpsc::Sender<Vec<AuditEvent>>,
    dial_backoff: &mut DialBackoff,
) {
    match event {
        // ── Gossipsub: peer announced itself ────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Gossipsub(gossipsub::Event::Message {
            message,
            ..
        })) => {
            // Off the network (discovery off, or taken off by its operator):
            // nothing a peer says is heard. Without this the swarm, which is
            // still connected to its relays, took every announce on the shared
            // topic and dialed its sender - through relay circuits, over and
            // over - so a node that was supposed to be silent kept knocking
            // on every cluster's door and loading the relay.
            if network.is_detached() {
                return;
            }
            let Some(source) = message.source else {
                warn!("libp2p: dropped an Announce without a signed source peer");
                return;
            };
            if let Ok(P2PMessage::Announce {
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
                libp2p_peer_id,
                libp2p_addrs,
                tp,
                runtime_capacity,
                cluster_logs_enabled,
                idle,
                network_chat_enabled,
                cluster_id,
                trust_version,
                relay,
                ..
            }) = serde_json::from_slice(&message.data)
            {
                if libp2p_peer_id.as_deref() != Some(source.to_string().as_str()) {
                    warn!(
                        "libp2p: dropped an Announce from {} with a mismatching declared PeerId",
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
                    // The announced multiaddresses are the source when they exist:
                    // that is where the circuit address comes from, and without it a peer
                    // behind NAT has no destination at all. Without them, `addr` is the
                    // sender's canonical endpoint; the legacy candidate list
                    // does not become a dial fan-out, because the identify listen
                    // addresses are not part of the cluster's announced
                    // endpoint contract.
                    let advertised = if libp2p_addrs.is_empty() {
                        vec![addr_candidates
                            .first()
                            .cloned()
                            .unwrap_or_else(|| addr.clone())]
                    } else {
                        libp2p_addrs.clone()
                    };
                    (peer_id, tunnel_peer_multiaddrs(peer_id, &advertised))
                });

                let peer = PeerInfo {
                    deploy_id,
                    node_id: node_id.clone(),
                    pubkey: pubkey.clone(),
                    cluster_id,
                    trust_version,
                    relay,
                    // The gossipsub path has no source address to
                    // rewrite, and the announced one is usually the literal
                    // `0.0.0.0`. With candidates there is something to use: the first
                    // of them is a real address, and `0.0.0.0` only
                    // survives when the peer announced none.
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
                    libp2p_peer_id: libp2p_peer_id.clone(),
                    libp2p_addrs: libp2p_addrs.clone(),
                    tp,
                    runtime_capacity,
                    idle,
                };
                network.remember_public_discovery_peer(&peer).await;
                // Never `admit_peer` here: the probe may need the libp2p
                // tunnel, which only moves when this loop spins again. Without a
                // verdict in the cache, the probe runs on another task and the peer's next
                // announce finds the answer. See `admit_in_background`.
                match network.admission_without_probe(&peer).await {
                    Some(true) => {}
                    Some(false) => {
                        debug!(
                            "libp2p: dropped an Announce from {} without proof of membership",
                            crate::network::short(&node_id)
                        );
                        return;
                    }
                    None => {
                        debug!(
                            "libp2p: Announce from {} waits for the admission probe",
                            crate::network::short(&node_id)
                        );
                        network.admit_in_background(peer);
                        return;
                    }
                }
                network.insert_admitted_peer(peer).await;

                network.trigger_cluster_logs_probe(&node_id, &pubkey, cluster_logs_enabled, tls);
                network.trigger_quic_probe(&node_id, &pubkey, quic, tls);
                network.trigger_network_chat_probe(&node_id, &pubkey, network_chat_enabled, tls);
                network.trigger_network_settings_probe(&node_id, &pubkey, tls);

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

                    if !swarm.is_connected(&peer_id) && !dial_backoff.blocked(&peer_id) {
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
            // Identify's listen_addrs describe the interfaces the remote
            // process bound, not the endpoint it explicitly advertised to the
            // cluster. In particular, they include Docker bridges, LAN IPs,
            // and stale Tailscale addresses. They are deliberately not added
            // to Kademlia: Announce.addr_candidates is the sole dial source.
            let ignored_addresses = info.listen_addrs.len();
            if ignored_addresses > 0 {
                for address in &info.listen_addrs {
                    // Remove entries learned by an older binary before the
                    // Announce-only policy took effect. `remove_address` is
                    // harmless when the address was never in the store.
                    swarm
                        .behaviour_mut()
                        .kademlia
                        .remove_address(&peer_id, address);
                }
                debug!(
                    "identify: ignoring {ignored_addresses} listen address(es) of {peer_id}; using only the Announce"
                );
            }
            // Whoever offers the hop protocol is a relay, whether or not it is in the configured
            // list. Reserving only on the configured relays left out
            // the case that matters most: a pom node with a public IP in the
            // cluster itself, which is exactly where the reservation should be made.
            let is_relay = connected_relays.contains(&peer_id)
                || info
                    .protocols
                    .iter()
                    .any(|p| *p == relay::HOP_PROTOCOL_NAME);
            // A dedicated relay reserves right away, and so does a relay
            // that is not a member (one outside the network, reached as a
            // bootstrap). A member is asked only when it announces a relay
            // offer; the policy round picks it up if the announce comes later.
            if is_relay {
                let ask = relay_peer_ids.contains(&peer_id)
                    || (relays.member_slot_free(relay_peer_ids)
                        && member_relay_offer(network, &peer_id).await != Some(false));
                if ask {
                    reserve_circuit(
                        swarm,
                        network,
                        peer_id,
                        relay_peer_addrs,
                        dialed_addrs.get(&peer_id),
                        relays,
                    )
                    .await;
                }
            }
        }

        SwarmEvent::NewListenAddr { address, .. } => {
            info!("libp2p listening on {}", address);
            publish_swarm_addrs(swarm, network);
        }

        SwarmEvent::ListenerClosed { listener_id, .. } => {
            relays.listener_closed(listener_id);
            publish_swarm_addrs(swarm, network);
        }

        SwarmEvent::ExpiredListenAddr { address, .. } => {
            info!("libp2p: listener expired at {address}");
            publish_swarm_addrs(swarm, network);
        }

        SwarmEvent::ExternalAddrConfirmed { address } => {
            info!("libp2p: external address confirmed {address}");
            publish_swarm_addrs(swarm, network);
        }

        SwarmEvent::ExternalAddrExpired { address } => {
            info!("libp2p: external address expired {address}");
            publish_swarm_addrs(swarm, network);
        }

        SwarmEvent::ConnectionEstablished {
            peer_id,
            connection_id,
            endpoint,
            ..
        } => {
            network.note_libp2p_link(&peer_id, connection_id, endpoint.get_remote_address());
            dial_backoff.connected(&peer_id);
            if relay_peer_ids.contains(&peer_id) {
                connected_relays.insert(peer_id);
            }
            // The address we dialed is `reserve_circuit`'s last resort
            // for a relay that is not a member of the network. A circuit will not do: that would be
            // reserving a relay through another.
            if let libp2p::core::ConnectedPoint::Dialer { address, .. } = &endpoint {
                if !address
                    .iter()
                    .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit))
                {
                    dialed_addrs.insert(peer_id, address.clone());
                }
            }
            debug!("libp2p: connected to {}", peer_id);
        }

        SwarmEvent::ConnectionClosed {
            peer_id,
            connection_id,
            cause,
            ..
        } => {
            network.drop_libp2p_link(&peer_id, connection_id);
            debug!("libp2p: disconnected from {} ({:?})", peer_id, cause);
        }

        // A dial that failed, with the reason per address. Without this the tunnel
        // only said "Failed to dial the requested peer" (`request_response`), and
        // finding out whether the relay refused the circuit, whether the address was stale
        // or whether the transport did not get there required debug logs on the
        // right machine at the right moment.
        SwarmEvent::OutgoingConnectionError {
            peer_id: Some(peer_id),
            error,
            ..
        } => {
            let failures = dial_backoff.failed(peer_id);
            // Only the 1st, 2nd, 4th, 8th... consecutive failure becomes a log: the record also
            // goes into the cluster's log batches, and one line per dial
            // would feed the very loop the backoff exists to cut.
            if failures.is_power_of_two() {
                info!("libp2p: dial to {peer_id} failed ({failures} in a row): {error}");
            }
        }

        // ── Relay reservation accepted ───────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Relay(
            relay::client::Event::ReservationReqAccepted { relay_peer_id, .. },
        )) => {
            info!("Relay slot reserved on {}", relay_peer_id);
            relays.accepted.insert(relay_peer_id);
            network.remember_reservation(&relay_peer_id.to_string());
            // The reservation becomes a circuit listener; that is the address the
            // `Announce` needs to carry so the other side has something to dial.
            publish_swarm_addrs(swarm, network);
        }

        // ── DCuTR: hole punch result ─────────────────────────────────────────
        // dcutr::Event is a struct { remote_peer_id, result: Result<ConnectionId, Error> }
        SwarmEvent::Behaviour(DiscBehaviourEvent::Dcutr(dcutr::Event {
            remote_peer_id,
            result,
        })) => match result {
            Ok(connection_id) => {
                info!(
                    "Direct connection to {} (hole punch succeeded)",
                    remote_peer_id
                );
                network.mark_hole_punched(&remote_peer_id, connection_id);
                network.remember_hole_punch(Ok(()));
            }
            Err(e) => {
                warn!(
                    "DCuTR failed for {} ({:?}) — relay fallback active",
                    remote_peer_id, e
                );
                network.remember_hole_punch(Err(format!("{remote_peer_id}: {e}")));
            }
        },

        // ── AutoNAT: NAT type detected ───────────────────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Autonat(autonat::Event::StatusChanged {
            new,
            ..
        })) => {
            info!("NAT status: {:?}", new);
            network.set_nat_status(match &new {
                autonat::NatStatus::Public(_) => "public",
                autonat::NatStatus::Private => "private",
                autonat::NatStatus::Unknown => "unknown",
            });
            // Discovering that one is public and not telling the swarm is of no use
            // at all: without a confirmed external address, this node does not announce it in
            // identify, peers do not learn it, and DCuTR does not have the public
            // address for the peer to dial at the moment of the punch.
            // A node the internet reaches relays for its cluster; one that
            // is known to be behind NAT does not. `Unknown` keeps what was
            // known (an address the operator configured, for instance).
            match &new {
                autonat::NatStatus::Public(_) => network.relay_gate().set_reachable(true),
                autonat::NatStatus::Private => network.relay_gate().set_reachable(false),
                autonat::NatStatus::Unknown => {}
            }
            if let autonat::NatStatus::Public(address) = new {
                info!("libp2p: external address confirmed {address}");
                swarm.add_external_address(address);
                publish_swarm_addrs(swarm, network);
            }
        }

        // ── Infer tunnel: inference HTTP calls carried over the swarm as a
        // NAT-traversal fallback. `Message::Request` is the inbound side
        // (forwarded to our own loopback inference server); `Message::Response`
        // and `Event::OutboundFailure` resolve a call *we* dispatched, via
        // `tunnel_rx` in `run()`'s select loop, by looking up the
        // `OutboundRequestId` the swarm handed back from `send_request`.
        SwarmEvent::Behaviour(DiscBehaviourEvent::InferTunnel(event)) => match event {
            request_response::Event::Message {
                peer,
                connection_id,
                message,
            } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    // The destination is chosen by the sender and **resolved here**:
                    // the remote peer names one of this node's two servers,
                    // never a port. On the gateway, the closed list of routes is
                    // checked before forwarding - see
                    // `gateway_path_is_allowed`, which explains why a tunnel
                    // open to the gateway would be a privilege escalation.
                    let target = match request.target {
                        infer_tunnel::TunnelTarget::Inference => network
                            .inference_port
                            .ok_or((503u16, "no inference server on this node".to_string())),
                        infer_tunnel::TunnelTarget::Gateway => {
                            if infer_tunnel::is_transfer_data_path(&request.path)
                                && !network
                                    .connection_carries_transfers(&peer, connection_id)
                                    .await
                            {
                                // File bytes never cross a public relay. The
                                // relay only sees an encrypted stream, so the
                                // two ends are the ones that hold the rule.
                                debug!(
                                    "infer-tunnel: refused a file chunk from {peer}: the connection crosses a relay that carries no transfers"
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
                                    "a file chunk over the tunnel needs a bounded range"
                                        .to_string(),
                                ))
                            } else if infer_tunnel::gateway_path_is_allowed(&request.path) {
                                network
                                    .gateway_port
                                    .ok_or((503u16, "no gateway on this node".to_string()))
                            } else {
                                warn!(
                                    "infer-tunnel: {} asked for route {} through the gateway; not on the list",
                                    peer, request.path
                                );
                                Err((403u16, "route not carried by the tunnel".to_string()))
                            }
                        }
                    };
                    match target {
                        Ok(port) => {
                            debug!(
                                "infer-tunnel: {} → loopback :{} {}",
                                peer, port, request.path
                            );
                            let response_tx = inbound_response_tx.clone();
                            spawn_loopback_request(
                                http,
                                port,
                                request,
                                move |response| async move {
                                    let _ = response_tx.send((channel, response)).await;
                                },
                            );
                        }
                        Err((status, message)) => {
                            // Nothing to forward to — this node has no such
                            // server running (no layer assignment yet, a
                            // pure-relay node), or the route is not one the
                            // tunnel carries. Answer rather than dropping the
                            // channel silently, so the caller's fallback logic
                            // gets a definite failure instead of a timeout.
                            let _ = swarm.behaviour_mut().infer_tunnel.send_response(
                                channel,
                                infer_tunnel::TunnelResponse {
                                    status,
                                    body: message.into_bytes(),
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
                    let before_request =
                        matches!(&error, request_response::OutboundFailure::DialFailure);
                    let failure = if before_request {
                        infer_tunnel::TunnelDeliveryError::before_request(error.to_string())
                    } else {
                        infer_tunnel::TunnelDeliveryError::after_dispatch(error.to_string())
                    };
                    let _ = reply.send(Err(failure));
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

        // ── Lobby: non-members talking to the principal ───────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::NetworkLobby(event)) => match event {
            request_response::Event::Message {
                peer,
                message:
                    request_response::Message::Request {
                        request, channel, ..
                    },
                ..
            } => {
                let response = answer_lobby_request(swarm, network, chat, peer, request).await;
                let _ = swarm
                    .behaviour_mut()
                    .network_lobby
                    .send_response(channel, response);
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                debug!("network-lobby: request from {peer} failed: {error:?}");
            }
            _ => {}
        },

        // ── Network Chat: the principal validates and relays ───────────────
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
                                candidate.libp2p_peer_id.as_deref()
                                    == Some(peer.to_string().as_str())
                            })
                            .is_some_and(|candidate| network.peer_is_principal(&candidate));
                        if !source_is_principal || !chat.enabled() {
                            ChatResponse::rejected("relay_not_authorized")
                        } else {
                            match chat.accept(request.action.clone()) {
                                Ok(true) => {
                                    relay_chat(swarm, network, chat, request.action, Some(peer))
                                        .await;
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
                                candidate.libp2p_peer_id.as_deref()
                                    == Some(peer.to_string().as_str())
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
                debug!("network-chat: request from {peer} failed: {error:?}");
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
                    let accepted = match network.live_node_of(&peer).await {
                        Some(sender) => logs.try_ingest_remote(&sender, request),
                        None => false,
                    };
                    if !accepted {
                        debug!("cluster-logs: batch refused from {peer}");
                    }
                    let _ = swarm
                        .behaviour_mut()
                        .cluster_logs
                        .send_response(channel, cluster_logs::LogResponse { accepted });
                }
                request_response::Message::Response { .. } => {}
            },
            request_response::Event::OutboundFailure { peer, error, .. } => {
                debug!("cluster-logs: send to {peer} failed: {error:?}");
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                debug!("cluster-logs: batch received from {peer} failed: {error:?}");
            }
            request_response::Event::ResponseSent { .. } => {}
        },

        // ── Request audit log: every admin replica ──────────────────────────
        SwarmEvent::Behaviour(DiscBehaviourEvent::Audit(event)) => match event {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request {
                    request, channel, ..
                } => {
                    let ordered = match network.live_node_of(&peer).await {
                        Some(sender) => audit_log.order_remote_events(&sender, request),
                        None => None,
                    };
                    let accepted = match ordered {
                        Some(events) if events.is_empty() => true,
                        Some(events) => match remote_audit_tx.try_send(events) {
                            Ok(()) => true,
                            Err(error) => error
                                .into_inner()
                                .into_iter()
                                .all(|event| audit_log.try_ingest_remote_ordered(event)),
                        },
                        None => false,
                    };
                    if !accepted {
                        debug!("request-audit: event refused from {peer}");
                    }
                    let _ = swarm
                        .behaviour_mut()
                        .audit
                        .send_response(channel, reqlog::AuditResponse { accepted });
                }
                request_response::Message::Response { .. } => {}
            },
            request_response::Event::OutboundFailure { peer, error, .. } => {
                debug!("request-audit: send to {peer} failed: {error:?}");
            }
            request_response::Event::InboundFailure { peer, error, .. } => {
                debug!("request-audit: event received from {peer} failed: {error:?}");
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
/// `PeerId` the Noise handshake proved. A text becomes an ordinary `lobby`
/// room message and is relayed to the members through the star; the answer is
/// the recent history of that room, which is public by definition.
async fn answer_lobby_request(
    swarm: &mut Swarm<DiscBehaviour>,
    network: &Network,
    chat: &Arc<NetworkChat>,
    peer: PeerId,
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
    let links = network.libp2p_links(&peer.to_string());
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
        relay_chat(swarm, network, chat, presence, None).await;
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
            Ok(true) => relay_chat(swarm, network, chat, action, None).await,
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
        let addresses = peer.libp2p_dialable_addrs();
        if !swarm.is_connected(&peer_id) {
            if let Err(error) =
                ensure_tunnel_peer_address(swarm, network, peer_id, &addresses).await
            {
                debug!("network-chat: peer {} unavailable: {}", peer.node_id, error);
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
    port: u16,
    request: infer_tunnel::TunnelRequest,
    on_response: F,
) where
    F: FnOnce(infer_tunnel::TunnelResponse) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let http = http.clone();
    tokio::spawn(async move {
        let response = infer_tunnel::serve_loopback(&http, port, request).await;
        on_response(response).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The circuit address is the only way a peer behind CGNAT has
    /// to offer itself, and it does not fit the `host:port` convention. It must
    /// go through `tunnel_peer_multiaddrs` intact.
    #[test]
    fn an_announced_circuit_address_is_dialable() {
        let peer: PeerId = "12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN"
            .parse()
            .expect("peer id");
        let relay = "12D3KooWRBy97UB99e3J6hiPesre1MZeuNQvfan4gBziswrRJsNK";
        let circuit = format!("/ip4/203.0.113.7/udp/52777/quic-v1/p2p/{relay}/p2p-circuit");

        let addrs = tunnel_peer_multiaddrs(peer, &[circuit.clone()]);

        assert_eq!(
            addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            vec![format!("{circuit}/p2p/{peer}")]
        );
    }

    /// The `/p2p/<destination>` already comes in the address the swarm itself published.
    /// Appending it again produced a multiaddress that nobody dials.
    #[test]
    fn the_p2p_suffix_is_not_duplicated() {
        let peer: PeerId = "12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN"
            .parse()
            .expect("peer id");
        let complete = format!("/ip4/203.0.113.7/udp/52777/quic-v1/p2p/{peer}");

        let addrs = tunnel_peer_multiaddrs(peer, &[complete.clone()]);

        assert_eq!(
            addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            vec![complete]
        );
    }

    /// A peer on an earlier binary does not publish `libp2p_addrs`; the legacy
    /// derivation by `UDP+10000` remains its path.
    #[test]
    fn the_legacy_host_port_keeps_deriving() {
        let peer: PeerId = "12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN"
            .parse()
            .expect("peer id");

        let addrs = tunnel_peer_multiaddrs(peer, &["203.0.113.7:42777".to_string()]);

        assert_eq!(
            addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
            vec![format!("/ip4/203.0.113.7/tcp/52777/p2p/{peer}")]
        );
    }

    /// Member relays are kept next to the dedicated ones, up to a cap: a
    /// public relay carries no file transfer, so it cannot stand in for them.
    #[test]
    fn member_relays_are_kept_beside_dedicated_ones_up_to_a_cap() {
        let dedicated_peer = PeerId::random();
        let dedicated: HashSet<PeerId> = [dedicated_peer].into_iter().collect();

        let mut policy = RelayPolicy::new();
        policy.listeners.insert(dedicated_peer, Vec::new());
        policy.accepted.insert(dedicated_peer);
        assert!(policy.member_slot_free(&dedicated));

        for _ in 0..MEMBER_RELAY_RESERVATIONS {
            policy.listeners.insert(PeerId::random(), Vec::new());
        }
        assert!(!policy.member_slot_free(&dedicated));
    }

    /// Closing a relay's last listener ends its reservation.
    #[test]
    fn closing_the_circuit_listener_ends_the_reservation() {
        let relay = PeerId::random();
        let listener = libp2p::core::transport::ListenerId::next();
        let mut policy = RelayPolicy::new();
        policy.listeners.insert(relay, vec![listener]);
        policy.accepted.insert(relay);

        policy.listener_closed(listener);

        assert!(!policy.listeners.contains_key(&relay));
        assert!(!policy.accepted.contains(&relay));
    }

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

    #[test]
    fn tunnel_addresses_preserve_only_the_explicit_advertised_set() {
        let peer_id = PeerId::random();
        let addrs = tunnel_peer_multiaddrs(peer_id, &["100.122.132.97:42777".into()]);

        assert_eq!(addrs.len(), 1);
        assert_eq!(
            addrs[0].to_string(),
            format!("/ip4/100.122.132.97/tcp/52777/p2p/{peer_id}")
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
    /// A dedicated relay is not a member of the network: without `--relay-peers` and outside
    /// the `peers` table, the address we dialed (the bootstrap) is the only one
    /// left, and without it the reservation never happened.
    #[test]
    fn a_relay_outside_the_network_is_reserved_through_the_dialed_address() {
        let relay = PeerId::random();
        let dialed: Multiaddr = "/ip4/203.0.113.10/tcp/52777".parse().unwrap();

        assert_eq!(
            relay_reservation_addrs(relay, &HashSet::new(), &[], Some(&dialed)),
            vec![format!("/ip4/203.0.113.10/tcp/52777/p2p/{relay}")
                .parse::<Multiaddr>()
                .unwrap()]
        );
        assert!(relay_reservation_addrs(relay, &HashSet::new(), &[], None).is_empty());
    }

    #[test]
    fn relay_reservation_prefers_configured_then_announced_addresses() {
        let relay = PeerId::random();
        let dialed: Multiaddr = "/ip4/203.0.113.10/tcp/52777".parse().unwrap();
        let announced = vec![format!("/ip4/198.51.100.7/tcp/52777/p2p/{relay}")];

        assert_eq!(
            relay_reservation_addrs(relay, &HashSet::new(), &announced, Some(&dialed)),
            vec![announced[0].parse::<Multiaddr>().unwrap()]
        );

        let configured: Multiaddr = format!("/ip4/192.0.2.1/udp/52777/quic-v1/p2p/{relay}")
            .parse()
            .unwrap();
        let relays = HashSet::from([configured.clone()]);
        assert_eq!(
            relay_reservation_addrs(relay, &relays, &announced, Some(&dialed)),
            vec![configured]
        );
    }

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

    // ── How gossipsub tells two announces apart ─────────────────────────────

    fn gossip_msg(data: &[u8]) -> gossipsub::Message {
        gossipsub::Message {
            source: None,
            data: data.to_vec(),
            sequence_number: None,
            topic: gossipsub::IdentTopic::new(GOSSIP_TOPIC).hash(),
        }
    }

    /// The id that the default `message_id_fn` gives to every announce of this project.
    ///
    /// Reproduces the defect at the exact spot: with `MessageAuthenticity::Anonymous`
    /// there is no `source` nor `sequence_number`, so the default function always returns
    /// `15R0` - the captain log's `message_id=31355230`, in hex.
    /// Two different announces get the same id, and that is why
    /// gossipsub stopped propagating.
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
        assert_eq!(a, b, "this is the defect that `announce_message_id` fixes");
        // The id the captain saw in the log, hex of "15R0".
        assert_eq!(a.to_string(), "31355230");
    }

    /// Two consecutive announces differ in `timestamp`, so they need
    /// different ids - otherwise the periodic publish is refused as a duplicate and
    /// the peer vanishes from the network on the other side after `PEER_TIMEOUT`.
    #[test]
    fn two_different_announces_get_two_different_ids() {
        let a = announce_message_id(&gossip_msg(br#"{"type":"Announce","t":1}"#));
        let b = announce_message_id(&gossip_msg(br#"{"type":"Announce","t":2}"#));
        assert_ne!(a, b);
    }

    /// And the same announce arriving through two mesh paths is still
    /// a duplicate - which is what the cache is for.
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

    #[test]
    fn dial_backoff_doubles_up_to_the_ceiling_and_a_connection_clears_it() {
        let peer = PeerId::random();
        let mut backoff = DialBackoff::default();
        assert!(!backoff.blocked(&peer));
        assert_eq!(backoff.failed(peer), 1);
        assert!(backoff.blocked(&peer));
        for _ in 0..20 {
            backoff.failed(peer);
        }
        let (_, until) = backoff.peers[&peer];
        assert!(until <= std::time::Instant::now() + DIAL_BACKOFF_CEILING);
        backoff.connected(&peer);
        assert!(!backoff.blocked(&peer));
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
