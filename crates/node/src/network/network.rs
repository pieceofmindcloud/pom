use crate::model::LayerAssignment;
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::FutureExt;
use routing_contract::ModelCapability;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

pub const DISCOVERY_PORT: u16 = 42777;
pub const GOSSIP_INTERVAL_SECS: u64 = 5;
pub const PEER_TIMEOUT_SECS: i64 = 30;

/// How many gossip ticks between discovery sweeps.
///
/// 6 ticks x 5 s = 30 s. It cannot be every tick: answering a `PeerRequest`
/// costs 1 `Announce` + the `PeerList`s sliced by MTU, so asking every
/// peer every 5 s is O(peers^2) traffic in a short interval.
pub const DISCOVERY_SWEEP_TICKS: u64 = 6;

/// How many live peers are asked per sweep.
///
/// It is a round-robin, not a sample: the sweep with index `k` starts at
/// `k * SWEEP_FANOUT` over the peer list sorted by `node_id`, so that
/// the whole cluster is covered in `ceil(peers / SWEEP_FANOUT)` sweeps
/// (2 minutes in a cluster of 8) without ever exceeding 3 questions per tick.
///
/// Cost in a cluster of 8 nodes: each node sends 3 `PeerRequest`s every 30 s; each
/// answer is 1 `Announce` (~700 B) + ~4 `PeerList`s of 1200 B, i.e. ~5.5 kB.
/// Per node: ~16.5 kB / 30 s. Whole cluster: ~132 kB / 30 s ≈ 4.4 kB/s. There is no
/// amplification: the answer to a `PeerRequest` is an `Announce` and `PeerList`s, and
/// neither of them provokes another `PeerRequest`.
pub const SWEEP_FANOUT: usize = 3;

#[cfg(not(test))]
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
const ADMISSION_TIMEOUT: Duration = Duration::from_millis(200);

/// The probe through the tunnel may have to build a circuit through a relay on the internet
/// before sending the request, and it runs outside the transport task
/// (`admit_in_background`): it does not need the short budget of the direct ones.
#[cfg(not(test))]
const ADMISSION_TUNNEL_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
const ADMISSION_TUNNEL_TIMEOUT: Duration = Duration::from_millis(500);

const ADMISSION_CACHE_TTL: Duration = Duration::from_secs(30);
const REJECTED_ADMISSION_CACHE_TTL: Duration = Duration::from_secs(5);

/// How long a peer that already proved membership stays admitted while
/// the following probes do not reach it. A peer that only the relay reaches loses the
/// route every time a circuit drops; without this slack, a probe in that interval
/// took it off the table (`Removed timed-out peers`) and it came back minutes later.
/// An explicit refusal does not use the slack.
const ADMISSION_UNREACHABLE_GRACE: Duration = Duration::from_secs(10 * 60);

/// What a node offers as a relay, as it announces it. Present only while the
/// node serves its cluster's endpoints.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayOffer {
    /// Whether file transfers of its cluster may cross it.
    pub transfers: bool,
    /// Where its relay server answers (`http://<public host>:<p2p_port + 10000>`).
    /// Members add it to their relay map; without it the offer is only a flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Whom this node relays for, consulted by the embedded relay server on
/// every connecting endpoint.
///
/// A node relays for **everyone** when the operator asked for it
/// (`serve_relay`): that is a public relay. Otherwise, by default, a node the
/// internet reaches relays for the admitted members of its own cluster
/// (`cluster_relay`), and for nobody else.
#[derive(Debug)]
pub struct RelayGate {
    everyone: AtomicBool,
    cluster: AtomicBool,
    transfers: AtomicBool,
    /// What the command line or the environment decided for this run. It
    /// wins over the saved configuration, so the screen shows it as fixed.
    overrides: RelayOverrides,
    /// The NAT report confirmed a public address, or the operator configured one.
    reachable: AtomicBool,
    /// The relay server is up (its port was free).
    available: AtomicBool,
    /// Endpoint ids of the admitted members, refreshed by the policy round.
    members: std::sync::RwLock<std::collections::HashSet<String>>,
    /// The URL the relay server answers on, when this node has a public host.
    url: std::sync::RwLock<Option<String>>,
}

/// Relay options given on the command line or in the environment
/// (`--serve-relay`, `--no-cluster-relay`, `--no-relay-transfers`).
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct RelayOverrides {
    pub everyone: bool,
    pub no_cluster: bool,
    pub no_transfers: bool,
}

/// The relay options in effect and what this node does with them.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct RelayStatus {
    pub everyone: bool,
    pub cluster: bool,
    pub transfers: bool,
    /// Which of the three the command line fixed for this run.
    pub fixed: RelayOverrides,
    /// The internet reaches this node.
    pub reachable: bool,
    /// It relays right now.
    pub serving: bool,
}

impl RelayGate {
    pub fn new(everyone: bool, cluster: bool, transfers: bool) -> Self {
        Self {
            everyone: AtomicBool::new(everyone),
            cluster: AtomicBool::new(cluster),
            transfers: AtomicBool::new(transfers),
            overrides: RelayOverrides::default(),
            reachable: AtomicBool::new(false),
            available: AtomicBool::new(true),
            members: std::sync::RwLock::new(Default::default()),
            url: std::sync::RwLock::new(None),
        }
    }

    /// Applies what the command line fixed on top of the saved options.
    pub fn with_overrides(mut self, overrides: RelayOverrides) -> Self {
        self.overrides = overrides;
        self.set(
            self.everyone.load(Ordering::Relaxed),
            self.cluster.load(Ordering::Relaxed),
            self.transfers.load(Ordering::Relaxed),
        );
        self
    }

    /// Changes the options live. A relay connection already admitted lasts
    /// until it closes; new ones follow the new options at once.
    pub fn set(&self, everyone: bool, cluster: bool, transfers: bool) {
        self.everyone
            .store(everyone || self.overrides.everyone, Ordering::Relaxed);
        self.cluster
            .store(cluster && !self.overrides.no_cluster, Ordering::Relaxed);
        self.transfers
            .store(transfers && !self.overrides.no_transfers, Ordering::Relaxed);
    }

    pub fn set_reachable(&self, reachable: bool) {
        self.reachable.store(reachable, Ordering::Relaxed);
    }

    /// Whether the relay server came up. Without it nothing is offered.
    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Relaxed);
    }

    /// The URL members dial to reach this node's relay server.
    pub fn set_url(&self, url: Option<String>) {
        if let Ok(mut current) = self.url.write() {
            *current = url;
        }
    }

    pub fn set_members(&self, members: impl IntoIterator<Item = String>) {
        if let Ok(mut current) = self.members.write() {
            *current = members.into_iter().collect();
        }
    }

    /// A public relay: it relays for any node of any network.
    pub fn everyone(&self) -> bool {
        self.everyone.load(Ordering::Relaxed)
    }

    /// Whether this node relays at all right now.
    pub fn serving(&self) -> bool {
        self.available.load(Ordering::Relaxed)
            && (self.everyone()
                || (self.cluster.load(Ordering::Relaxed)
                    && self.reachable.load(Ordering::Relaxed)))
    }

    /// What goes into the announce.
    pub fn offer(&self) -> Option<RelayOffer> {
        self.serving().then(|| RelayOffer {
            transfers: self.transfers.load(Ordering::Relaxed),
            url: self.url.read().ok().and_then(|url| url.clone()),
        })
    }

    /// May the endpoint `peer` use this node's relay server?
    ///
    /// An iroh relay does not tell an endpoint that makes the relay its home
    /// from one that only sends through it, so a member relay admits members
    /// alone: a candidate that is not a member yet reaches a member behind
    /// NAT through a public relay (n0's, or a dedicated one).
    pub fn allows(&self, peer: &str) -> bool {
        self.everyone()
            || (self.serving()
                && self
                    .members
                    .read()
                    .is_ok_and(|members| members.contains(peer)))
    }

    pub fn status(&self) -> RelayStatus {
        RelayStatus {
            everyone: self.everyone(),
            cluster: self.cluster.load(Ordering::Relaxed),
            transfers: self.transfers.load(Ordering::Relaxed),
            fixed: self.overrides,
            reachable: self.reachable.load(Ordering::Relaxed),
            serving: self.serving(),
        }
    }
}

/// A live connection with a peer, as its selected path stands now.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct P2pLink {
    /// `host:port (UDP)` of the other end; through a relay, the relay's host.
    pub address: String,
    pub relayed: bool,
    /// URL of the relay, when the path goes through one.
    pub relay_url: Option<String>,
    /// The connection started on a relay and moved to a direct path.
    pub hole_punched: bool,
}

impl P2pLink {
    pub fn new(path: crate::p2p::LinkPath, hole_punched: bool) -> Self {
        match path {
            crate::p2p::LinkPath::Direct(address) => Self {
                address: format!("{address} (UDP)"),
                relayed: false,
                relay_url: None,
                hole_punched,
            },
            crate::p2p::LinkPath::Relay(url) => Self {
                address: url
                    .parse::<url::Url>()
                    .ok()
                    .and_then(|parsed| {
                        let host = parsed.host_str()?.to_string();
                        Some(match parsed.port() {
                            Some(port) => format!("{host}:{port}"),
                            None => host,
                        })
                    })
                    .unwrap_or_else(|| url.clone()),
                relayed: true,
                relay_url: Some(url),
                hole_punched,
            },
        }
    }
}

/// How this node actually talks to a peer - observed, not announced.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PeerRoute {
    /// `http`: a direct address answered a moment ago (`RouteTable::last_success`).
    /// `p2p_direct`: direct endpoint path, dialed or by hole punch.
    /// `relayed`: only a path through a relay - the hole punch did not close.
    /// `none`: none of the three.
    pub kind: &'static str,
    /// The route's address; in `relayed`, the relay's.
    pub address: Option<String>,
    /// In `relayed`: `dedicated` (configured relay), `public` (one of n0's
    /// default relays) or `member` (another cluster node acting as a relay).
    pub relay_kind: Option<&'static str>,
    /// Some direct connection with the peer came from a hole punch.
    pub hole_punched: bool,
    /// All live endpoint connections, for the screen's detail.
    pub links: Vec<P2pLink>,
}

/// Host of `http(s)://host:port[/...]`, with the IPv6 brackets, to
/// compare addresses of the same peer on different ports.
fn url_host(url: &str) -> &str {
    let rest = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let rest = rest.split('/').next().unwrap_or(rest);
    if rest.starts_with('[') {
        return rest.split_inclusive(']').next().unwrap_or(rest);
    }
    rest.rsplit_once(':').map_or(rest, |(host, _)| host)
}

/// The operator's pinned `host:port` (the UDP port) as an endpoint address:
/// the same host on the endpoint port. Only an IP literal: iroh dials
/// addresses, not names, and resolving a name here would block on every
/// announce. A pinned name still reaches peers through `addr_candidates`.
fn pinned_endpoint_addr(pinned: &str) -> Option<String> {
    let socket = pinned.parse::<std::net::SocketAddr>().ok()?;
    if socket.port() == 0 || socket.ip().is_unspecified() {
        return None;
    }
    let port = socket.port().checked_add(crate::p2p::P2P_PORT_OFFSET)?;
    Some(std::net::SocketAddr::new(socket.ip(), port).to_string())
}

/// Result of a `/handshake` probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionProbe {
    Approved,
    /// The peer itself said no: through the tunnel (authenticated EndpointId) or over HTTPS
    /// with the certificate pinned to the `node_id`. A cleartext HTTP answer does not
    /// count - any machine at that address could have given it.
    Refused,
    /// No authentic answer arrived.
    Unreachable,
}

/// How often, at most, the address of an unreadable peer is asked
/// again.
///
/// The `Announce` goes out every `GOSSIP_INTERVAL_SECS`, so a 30 s window leaves
/// time for the answer to arrive and still keeps the Dashboard, which re-reads the cluster in a
/// short cycle, from becoming a UDP traffic generator.
pub const REDISCOVERY_INTERVAL: Duration = Duration::from_secs(30);

/// Until when the screen may say "rediscovering".
///
/// Three questions to the network with no answer are no longer work in progress: they are a
/// peer that announces over UDP and does not answer on HTTP. Calling that "rediscovering"
/// forever would be the same dishonesty as the permanent warning this work
/// came to remove, just with another word - from here on the screen goes back to saying it
/// cannot read the peer, and says why.
pub const REDISCOVERY_GIVE_UP: Duration = Duration::from_secs(90);

/// Identity of a node in a tensor parallelism group (path C).
///
/// `role` is `"primary"` (owner of the GGUF and pipeline hop) or `"worker"`
/// (shares the compute of that stage). `group_id` identifies the group;
/// `size`/`rank` say how many we are and which position is mine. A node without
/// TP announces `None`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TpIdentity {
    pub role: String,
    pub group_id: String,
    pub size: u32,
    pub rank: u32,
    /// URL of the collective's primary (rank 0), through which all ranks do the
    /// POST /v1/tp/reduce. The deploy fills it in for every member of the group.
    #[serde(default)]
    pub rank0_url: Option<String>,
    /// `node_id` of the primary, which is **what pins the certificate** of the
    /// collective's client (`SHA256(pubkey) == node_id`).
    ///
    /// Without it `tp.rs` used a default `reqwest::Client::new()` against an
    /// https `rank0_url` with a self-signed certificate: every collective POST
    /// died in 13 ms with an invalid certificate, the error was swallowed, and the
    /// partial went on as if it were the sum (18/08).
    #[serde(default)]
    pub rank0_node_id: Option<String>,
    /// All members of the group, **indexed by rank** (0 = primary).
    ///
    /// It is what lets rank 0 set the workers running the same decode: TP
    /// requires all ranks to execute the same graph over the same tokens
    /// at the same time, and without this list rank 0 has no one to broadcast to.
    /// Without lockstep the partials never meet at the barrier and the decode
    /// hangs - the state of 18/08.
    #[serde(default)]
    pub members: Vec<TpMember>,
}

/// A member of a tensor parallelism group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TpMember {
    /// Pins this member's certificate (`SHA256(pubkey) == node_id`).
    pub node_id: String,
    /// Base URL of the inference server (where the decode is triggered).
    pub url: String,
    /// Alternative addresses of the inference server. The first is kept
    /// in `url` for the old format; whoever dials tries the others when the
    /// preferred address is not reachable from this rank.
    #[serde(default)]
    pub url_candidates: Vec<String>,
    /// UDP addresses announced by the member, so the tunnel can rebuild the
    /// TCP endpoint addresses even when the local endpoint has not yet seen the announce.
    #[serde(default)]
    pub addr_candidates: Vec<String>,
    /// EndpointId used by the p2p transport fallback.
    #[serde(default)]
    pub endpoint_id: Option<String>,
    /// The member announced that its endpoint serves the inference tunnel.
    #[serde(default)]
    pub supports_tunnel: bool,
}

/// Effective capacity announced by a ready inference server.
///
/// `max_new_tokens` is the size of one backend leg. `n_ctx` is the effective
/// window that bounds the sum of prompt and output on this peer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCapacity {
    pub n_ctx: u32,
    pub max_new_tokens: u32,
}

/// What this node knows about its own NAT traversal.
///
/// It exists because diagnosing "the two nodes do not talk to each other" depended on
/// `RUST_LOG=debug` on the right machine at the right moment. The NAT report,
/// the relays in use and the hole punches are exactly what says on which rung
/// the traversal stopped.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraversalStatus {
    /// `unknown`, `public` or `private`, from the NAT report.
    pub nat: String,
    /// Endpoint addresses this node publishes today: direct `ip:port` and
    /// relay URLs.
    pub addrs: Vec<String>,
    /// URLs of the relays the endpoint holds a live connection to.
    pub reservations: Vec<String>,
    /// Connections that moved from a relay to a direct path since startup.
    /// iroh reports no failed punch, so `hole_punch_failed` stays at zero.
    pub hole_punch_ok: u64,
    pub hole_punch_failed: u64,
    /// Last hole punch failure, to say *why* it stopped.
    pub last_hole_punch_error: Option<String>,
}

impl TraversalStatus {
    fn new() -> Self {
        Self {
            nat: "unknown".to_string(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Default)]
struct RuntimeCapacityState {
    generation: u64,
    capacity: Option<RuntimeCapacity>,
}

struct NetworkInit {
    bootstrap_nodes: Vec<String>,
    ollama_port: Option<u16>,
    inference_port: Option<u16>,
    inference_quic_port: Option<u16>,
    gateway_port: Option<u16>,
    gateway_quic_port: Option<u16>,
}

struct PeerAdmission {
    auth: crate::auth::AuthState,
    trust: Arc<crate::admin_trust::AdminTrust>,
}

struct AdmissionCacheEntry {
    allowed: bool,
    expires_at: Instant,
    /// Last real approval, for `ADMISSION_UNREACHABLE_GRACE`.
    proven_at: Option<Instant>,
}

/// What enters the cache after a probe. Approved is valid for
/// `ADMISSION_CACHE_TTL`; refused leaves. With no answer, a peer approved less than
/// `ADMISSION_UNREACHABLE_GRACE` ago stays admitted, with a short validity so that
/// the probe tries again soon; without recent proof, it leaves.
fn admission_verdict(
    probe: AdmissionProbe,
    previous_proof: Option<Instant>,
    now: Instant,
) -> AdmissionCacheEntry {
    let rejected = AdmissionCacheEntry {
        allowed: false,
        expires_at: now + REJECTED_ADMISSION_CACHE_TTL,
        proven_at: None,
    };
    match probe {
        AdmissionProbe::Approved => AdmissionCacheEntry {
            allowed: true,
            expires_at: now + ADMISSION_CACHE_TTL,
            proven_at: Some(now),
        },
        AdmissionProbe::Refused => rejected,
        AdmissionProbe::Unreachable => match previous_proof
            .filter(|proven| now.saturating_duration_since(*proven) < ADMISSION_UNREACHABLE_GRACE)
        {
            Some(proven) => AdmissionCacheEntry {
                allowed: true,
                expires_at: now + REJECTED_ADMISSION_CACHE_TTL,
                proven_at: Some(proven),
            },
            None => rejected,
        },
    }
}

/// Message types exchanged between nodes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum P2PMessage {
    /// Node announces itself and its layer assignment
    Announce {
        node_id: String,
        /// The node's Ed25519 public key: the `node_id`, the `endpoint_id`
        /// that signs the gossip and the pkarr key derive from it.
        pubkey: String,
        addr: String,
        /// All of this node's addresses. `serde(default)` keeps an old node
        /// parseable: without candidates, the single `addr` still applies.
        #[serde(default)]
        addr_candidates: Vec<String>,
        model: String,
        /// The deployment this node serves. Empty on a peer of an earlier version,
        /// which makes the grouping fall back to the model label.
        #[serde(default)]
        deploy_id: String,
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
        /// Dedicated UDP port for this node's inference HTTP/3 endpoint.
        #[serde(default)]
        inference_quic_port: Option<u16>,
        /// Inference capabilities this peer can receive. The current product
        /// exposes the canonical batch frame only after inference is ready.
        #[serde(default)]
        inference_frame_capabilities: Vec<String>,
        /// HTTP port serving the API and web UI. `serde(default)` keeps peers
        /// running an older build parseable.
        #[serde(default)]
        gateway_port: Option<u16>,
        /// Dedicated UDP port for this node's gateway HTTP/3 endpoint.
        #[serde(default)]
        gateway_quic_port: Option<u16>,
        #[serde(default)]
        profile: crate::profile::NodeProfile,
        /// Whether this node speaks TLS with the peers.
        ///
        /// **Trigger, not value**: the receiver does not store this. See
        /// `tls_policy` - gossip is cleartext and unauthenticated, so obeying
        /// it would be offering the downgrade that TLS exists to prevent.
        #[serde(default)]
        tls: bool,
        /// Trigger for the QUIC policy's authenticated gate.
        #[serde(default)]
        quic: bool,
        #[serde(default)]
        hostname: String,
        /// Full inventory for new peers. The legacy model/range fields remain
        /// so a rolling cluster can still communicate with older binaries.
        #[serde(default)]
        capabilities: Vec<ModelCapability>,
        #[serde(default)]
        disk_total_mb: u64,
        #[serde(default)]
        disk_free_mb: u64,
        /// This node's endpoint id (the public key, hex), when a stable one could be
        /// derived (see `Discovery::new`). `serde(default)` keeps an older
        /// peer parseable; absent means this node can't be reached over the
        /// p2p NAT-tunnel fallback (`transport/iroh-tunnel-v1`).
        #[serde(default)]
        endpoint_id: Option<String>,
        /// Endpoint addresses this node publishes: direct `ip:port` (the
        /// endpoint's UDP port) and the URLs of the relays it is reachable
        /// through. The relay URL is the only address a node behind CGNAT has
        /// to offer; the `host:port` convention cannot carry it. Whoever
        /// dials adds the `addr_candidates` on the endpoint port.
        #[serde(default)]
        endpoint_addrs: Vec<String>,
        /// Tensor parallelism identity, when this node is primary/worker of
        /// a group. Absent = node without TP (today's case).
        #[serde(default)]
        tp: Option<TpIdentity>,
        /// Capacity present only after `/health` proves the
        /// model loaded. Absent in old announces or during boot.
        #[serde(default)]
        runtime_capacity: Option<RuntimeCapacity>,
        /// Trigger for the log collection's authenticated gate.
        #[serde(default)]
        cluster_logs_enabled: bool,
        /// Model unloaded by idle, keeping the deployment announced.
        #[serde(default)]
        idle: bool,
        /// Trigger for the network Chat's authenticated gate.
        #[serde(default)]
        network_chat_enabled: bool,
        /// Identifier of the public endpoint's trust chain.
        /// The value alone does not authenticate the peer.
        #[serde(default)]
        cluster_id: Option<String>,
        /// The version of the trust chain this node holds
        /// (`AdminTrust::chain_version`). Only a hint of who may have a
        /// newer chain: the chain itself is fetched and verified.
        #[serde(default)]
        trust_version: Option<String>,
        /// Present while this node relays for its cluster (or everyone).
        #[serde(default)]
        relay: Option<RelayOffer>,
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
    /// First-access LAN bootstrap. The Offer carries only trust metadata; the
    /// trust document and connection key are fragmented and encrypted only
    /// after an accepted Proof.
    LocalAutoDiscovery {
        message: crate::local_discovery::LocalAutoDiscoveryMessage,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub node_id: String,
    /// The node's single public key (see `identity.rs`): `node_id`, endpoint id,
    /// pkarr key and authority key.
    pub pubkey: String,
    /// Trust chain claimed by the peer. It is only accepted together with the
    /// locally validated authority tuple.
    #[serde(default)]
    pub cluster_id: Option<String>,
    /// The trust chain version the peer announces; a hint, never proof.
    #[serde(default)]
    pub trust_version: Option<String>,
    /// What the peer offers as a relay, when it does.
    #[serde(default)]
    pub relay: Option<RelayOffer>,
    pub addr: String,
    /// All the addresses at which this peer can be found.
    ///
    /// `addr` remains the historical preferred one and the only one that an old node
    /// understands. This set is what lets whoever is going to dial choose, instead
    /// of accepting the choice of whoever planned from elsewhere in the network.
    #[serde(default)]
    pub addr_candidates: Vec<String>,
    pub model: String,
    /// The deployment this node serves. `plan_pipeline` groups by it,
    /// and it is what allows the same model to be up twice on different sets of
    /// machines. Empty on a peer of an earlier version, in which case the
    /// grouping falls back to the label. See `models/deployment.rs`.
    #[serde(default)]
    pub deploy_id: String,
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
    /// Dedicated UDP port for this peer's inference HTTP/3 endpoint.
    #[serde(default)]
    pub inference_quic_port: Option<u16>,
    #[serde(default)]
    pub inference_frame_capabilities: Vec<String>,
    /// Port serving this peer's HTTP API and web UI, so the dashboard can pull
    /// load figures straight from each peer.
    #[serde(default)]
    pub gateway_port: Option<u16>,
    /// Dedicated UDP port for this peer's gateway HTTP/3 endpoint.
    #[serde(default)]
    pub gateway_quic_port: Option<u16>,
    #[serde(default)]
    pub profile: crate::profile::NodeProfile,
    /// See `P2PMessage::Announce::tls`: trigger for the gate, never a value.
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub capabilities: Vec<ModelCapability>,
    #[serde(default)]
    pub connection_verified: bool,
    #[serde(default)]
    pub disk_total_mb: u64,
    #[serde(default)]
    pub disk_free_mb: u64,
    #[serde(default)]
    pub api_keys_active: u16,
    /// See `P2PMessage::Announce::endpoint_id`.
    #[serde(default)]
    pub endpoint_id: Option<String>,
    /// See `P2PMessage::Announce::endpoint_addrs`.
    #[serde(default)]
    pub endpoint_addrs: Vec<String>,
    /// See `P2PMessage::Announce::tp`.
    #[serde(default)]
    pub tp: Option<TpIdentity>,
    /// Runtime capacity after the inference server became ready.
    /// `None` is not zero: it means the capacity has not been proven yet.
    #[serde(default)]
    pub runtime_capacity: Option<RuntimeCapacity>,
    /// The peer is alive and announced, but without the weights in memory.
    #[serde(default)]
    pub idle: bool,
}

/// Extract the host part of an addr string for use in HTTP URLs.
/// IPv4 `"1.2.3.4:port"` → `"1.2.3.4"`, IPv6 `"[::1]:port"` → `"[::1]"`.
pub(crate) fn ip_host(addr: &str) -> &str {
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

fn has_nonzero_port(addr: &str) -> bool {
    if addr.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    addr.parse::<SocketAddr>()
        .map(|socket| socket.port() != 0)
        .unwrap_or_else(|_| {
            addr.rsplit_once(':')
                .and_then(|(_, port)| port.parse::<u16>().ok())
                .is_some_and(|port| port != 0)
        })
}

fn bare_host(host: &str) -> &str {
    host.trim_start_matches('[').trim_end_matches(']')
}

/// Destinations may be private or loopback when the cluster is local, but an
/// unspecified, multicast, broadcast, or `0.0.0.0/8` address is never a peer.
/// Hostnames remain valid here and are checked by `lookup_host` at send time.
/// UDP/p2p port of this process, recorded when the socket opens. With it and the
/// IPs of the interfaces, `is_self_endpoint` recognizes a candidate of another peer
/// that actually points to this node.
static SELF_P2P_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// IPs of the local interfaces, without loopback. `addrs::local_addrs` runs a
/// process (`ip -o addr`), so the result is valid for a few seconds.
fn local_interface_ips() -> std::collections::HashSet<String> {
    static CACHE: std::sync::Mutex<Option<(Instant, std::collections::HashSet<String>)>> =
        std::sync::Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, ips)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(10) {
            return ips.clone();
        }
    }
    let ips: std::collections::HashSet<String> = crate::addrs::local_addrs()
        .into_iter()
        .map(|local| bare_host(&local.ip).to_string())
        .filter(|ip| {
            ip.parse::<IpAddr>()
                .is_ok_and(|parsed| !parsed.is_loopback())
        })
        .collect();
    *cache = Some((Instant::now(), ips.clone()));
    ips
}

/// `host:port` that is this very process: IP of a local interface with this
/// node's p2p port.
///
/// Two nodes in Docker with bridge networking usually get the same internal IP
/// (`172.18.0.2`) and the same port. The candidate `172.18.0.2:42778` that ksia
/// announces, seen from salem, is salem itself: the Dashboard showed the same
/// machine twice (the "ksia" `/health` was answered by salem) and the
/// deploy planned for ksia landed on salem.
pub(crate) fn is_self_endpoint(addr: &str) -> bool {
    let port = SELF_P2P_PORT.load(std::sync::atomic::Ordering::Relaxed);
    if port == 0 {
        return false;
    }
    let Some((host, candidate_port)) = split_host_port(addr) else {
        return false;
    };
    candidate_port == port && local_interface_ips().contains(bare_host(&host))
}

fn split_host_port(addr: &str) -> Option<(String, u16)> {
    let (host, port) = addr.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

/// `is_valid_destination` for a candidate of **another** peer: it also refuses
/// the address that is this very process. This node's own entry in the table
/// keeps all of its addresses.
fn is_peer_destination(node_id: &str, addr: &str) -> bool {
    is_valid_destination(addr)
        && (crate::infer_tunnel::is_local_node(node_id) || !is_self_endpoint(addr))
}

pub(crate) fn is_valid_destination(addr: &str) -> bool {
    if !has_nonzero_port(addr) {
        return false;
    }
    let host = bare_host(ip_host(addr));
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast() && ip.octets()[0] != 0
        }
        Ok(std::net::IpAddr::V6(ip)) => !ip.is_unspecified() && !ip.is_multicast(),
        Err(_) => !host.is_empty(),
    }
}

fn host_port(host: &str, port: u16) -> String {
    let host = bare_host(host);
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn local_candidates_by_preference(
    observed_addr: Option<&str>,
    local: Vec<crate::addrs::LocalAddr>,
) -> Vec<String> {
    let local = crate::addrs::filter_candidates(local, None);
    let mut preferred = Vec::with_capacity(local.len() + 2);

    let (public, private_or_local): (Vec<_>, Vec<_>) = local
        .into_iter()
        .partition(|ip| !crate::addrs::is_private_or_local(bare_host(ip)));
    preferred.extend(public);

    if let Some(observed) = observed_addr {
        let observed_host = ip_host(observed);
        if !crate::addrs::is_private_or_local(bare_host(observed_host)) {
            preferred.push(observed_host.to_string());
        }
    }

    preferred.extend(private_or_local);
    deduplicate_hosts(preferred)
}

fn deduplicate_hosts(hosts: Vec<String>) -> Vec<String> {
    let mut unique = Vec::with_capacity(hosts.len());
    for host in hosts {
        if unique
            .iter()
            .all(|existing: &String| bare_host(existing) != bare_host(&host))
        {
            unique.push(host);
        }
    }
    unique
}

fn current_public_local_host(local: Vec<crate::addrs::LocalAddr>) -> Option<String> {
    crate::addrs::filter_candidates(local, None)
        .into_iter()
        .find(|ip| !crate::addrs::is_private_or_local(bare_host(ip)))
}

/// Host this node describes itself by, IPv6 always in brackets.
///
/// The pinned and observed hosts come out of `ip_host`, which keeps the
/// brackets of `[v6]:port`. The local-interface host comes from
/// `filter_candidates`, which returns the bare address - and callers append
/// `:port` to this host or feed it back to `ip_host`, so a bare IPv6 became
/// `2804:...:5eb8:42777` in the self entry and `2804` in `self_reply_hosts`.
fn choose_effective_host(
    pinned: Option<String>,
    local: Vec<crate::addrs::LocalAddr>,
    observed: Option<String>,
) -> Option<String> {
    if pinned.is_some() {
        return pinned;
    }
    if let Some(current) = current_public_local_host(local) {
        let host = bare_host(&current);
        return Some(if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        });
    }
    observed.map(|addr| ip_host(&addr).to_string())
}

fn choose_announce_addr(
    pinned_addr: Option<String>,
    observed_addr: Option<String>,
    local: Vec<crate::addrs::LocalAddr>,
    port: u16,
) -> String {
    if let Some(pinned) = pinned_addr {
        return pinned;
    }
    if let Some(current) = current_public_local_host(local) {
        return host_port(&current, port);
    }
    observed_addr.unwrap_or_else(|| format!("0.0.0.0:{port}"))
}

fn announce_addresses(
    pinned: Option<String>,
    observed: Option<String>,
    local: Vec<crate::addrs::LocalAddr>,
    port: u16,
) -> (String, Vec<String>) {
    // The operator's pinned address is a priority hint, not an exclusive
    // contract: it goes first (with the port the operator gave), and the
    // automatic candidates stay behind it. Whoever dials measures and chooses
    // (`RouteTable`, `route_plan`); a stale pin does not isolate the node.
    let addr = choose_announce_addr(pinned.clone(), observed.clone(), local.clone(), port);
    let mut candidates: Vec<String> = pinned.iter().cloned().collect();
    for candidate in local_candidates_by_preference(observed.as_deref(), local)
        .into_iter()
        .map(|ip| host_port(&ip, port))
    {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    (addr, candidates)
}

impl PeerInfo {
    pub fn layer_range(&self) -> String {
        format!("{}-{}", self.start_layer, self.end_layer)
    }

    pub fn layers_count(&self) -> u32 {
        self.end_layer - self.start_layer + 1
    }

    /// Is this peer a tensor parallelism worker (not a pipeline hop)?
    /// The planner excludes workers from the candidates: only the primary of a group
    /// enters the chain.
    pub fn is_tp_worker(&self) -> bool {
        self.tp.as_ref().is_some_and(|t| t.role == "worker")
    }

    /// Alive with its model in memory being loaded: not idle, and no
    /// capacity proven yet (`runtime_capacity` comes once it is ready).
    pub fn is_warming(&self) -> bool {
        !self.idle && self.runtime_capacity.is_none()
    }

    pub fn is_alive(&self) -> bool {
        let now = Utc::now();
        let diff = now.signed_duration_since(self.last_seen);
        diff.num_seconds() < PEER_TIMEOUT_SECS
    }

    /// Full Ollama HTTP URL for this peer (None if no ollama_port advertised).
    ///
    /// **It stays `http://` even with the cluster on TLS, on purpose**: Ollama
    /// is a third-party server running on the peer's machine, not a node
    /// of this cluster. It has neither our certificate nor our identity, and
    /// asking it for TLS would only produce a refused connection.
    pub fn ollama_url(&self) -> Option<String> {
        self.ollama_port
            .map(|port| format!("http://{}:{}", ip_host(&self.addr), port))
    }

    /// Adds the address the packet really came from when the source is still
    /// the only clue. An `Announce` with explicit `addr_candidates` stays the
    /// authority, so a LAN or bridge address the peer itself filtered out is
    /// not reintroduced.
    pub fn merge_observed(&mut self, observed: &str) {
        if !self.addr_candidates.iter().any(|c| c == observed) {
            self.addr_candidates.push(observed.to_string());
        }
    }

    pub fn dialable_addr(&self) -> Option<&str> {
        self.addr_candidates
            .iter()
            .find(|addr| is_peer_destination(&self.node_id, addr))
            .map(String::as_str)
            .or_else(|| {
                is_peer_destination(&self.node_id, &self.addr).then_some(self.addr.as_str())
            })
    }

    /// All known UDP destinations, without turning the first announced
    /// address into permanent truth. The order keeps the announced
    /// candidates and uses `addr` as a fallback for old peers or for a
    /// snapshot in which it was already updated.
    pub fn dialable_addrs(&self) -> Vec<String> {
        let mut addresses = Vec::with_capacity(self.addr_candidates.len() + 1);
        for address in self
            .addr_candidates
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.addr.as_str()))
        {
            if is_peer_destination(&self.node_id, address)
                && !addresses.iter().any(|known| known == address)
            {
                addresses.push(address.to_string());
            }
        }
        addresses
    }

    /// Addresses worth handing to the endpoint when dialing this peer: what
    /// it announced (`endpoint_addrs`: relay URLs and public direct
    /// addresses), then its UDP candidates with the endpoint port
    /// (`UDP + 10000`), which is how a LAN or VPN peer is reached when mDNS
    /// does not cross the segment. iroh tries them all and keeps what answers.
    pub fn endpoint_dialable_addrs(&self) -> Vec<String> {
        let mut addrs = self.endpoint_addrs.clone();
        for candidate in self.dialable_addrs() {
            let Ok(socket) = candidate.parse::<std::net::SocketAddr>() else {
                continue;
            };
            let Some(port) = socket.port().checked_add(crate::p2p::P2P_PORT_OFFSET) else {
                continue;
            };
            let text = std::net::SocketAddr::new(socket.ip(), port).to_string();
            if !addrs.contains(&text) {
                addrs.push(text);
            }
        }
        addrs
    }

    /// URLs of the inference server, one per candidate.
    ///
    /// Falls back to the single `addr` when the peer did not announce candidates, which is the case
    /// of a node running an earlier binary.
    pub fn inference_candidates(&self) -> Vec<String> {
        let Some(port) = self.inference_port else {
            return Vec::new();
        };
        let base = self
            .addr_candidates
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.addr.as_str()));
        let mut urls = Vec::with_capacity(self.addr_candidates.len() + 1);
        for address in base {
            if !is_peer_destination(&self.node_id, address) {
                continue;
            }
            let url = format!(
                "{}://{}:{}",
                crate::peer_client::peer_scheme(),
                ip_host(address),
                port
            );
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
        urls
    }

    /// Full inference server URL for pipeline calls (None if not running).
    ///
    /// Delegates to `inference_candidates()` and takes the first entry —
    /// same address preference (a pinned or peer-observed public address
    /// before the single legacy `addr`), just collapsed to the one URL a
    /// caller with no candidate list of its own (the UI, `PipelineStep`'s
    /// fallback field) actually wants. Building this straight from `addr`
    /// alone, as it used to, meant a NAT'd node's dashboard entry and
    /// `PipelineStep.inference_url` fallback could show a private LAN
    /// address even when a better one was already sitting in
    /// `addr_candidates`.
    pub fn inference_url(&self) -> Option<String> {
        self.inference_candidates().into_iter().next()
    }

    /// `host:port` addresses of the inference QUIC endpoint, built from
    /// the public address announced by the peer and the dedicated UDP port.
    /// This path neither reads nor parses `inference_url`.
    pub fn quic_addr_candidates(&self) -> Vec<String> {
        let Some(port) = self.inference_quic_port else {
            return Vec::new();
        };
        self.dialable_addrs()
            .into_iter()
            .map(|address| format!("{}:{port}", ip_host(&address)))
            .collect()
    }

    /// Base URL of this peer's HTTP API / web UI.
    ///
    /// It is the **first** candidate, not the only one. Whoever can try more than
    /// one address must call `gateway_candidates()` and consult the failure
    /// memory (`RouteTable::usable`): an address that stopped answering
    /// cannot be the end of the line for the control plane just because it is the preferred one.
    pub fn gateway_url(&self) -> Option<String> {
        self.gateway_candidates().into_iter().next()
    }

    /// Gateway URLs, one per known address, in order of preference.
    ///
    /// `addr` comes first because it is a **known fact**: it is the address the
    /// packets of this peer came from, or the one it announced for itself. The other
    /// candidates come after, in the order in which the peer announced them. It is the same
    /// discipline as `pipeline::resolve_step_url` - a guess does not go ahead
    /// of a known fact, because `merge_observed` appends the observed one to the
    /// **end** of the list and the first candidate is usually the peer's LAN.
    ///
    /// Having more than one entry here is what separates this list from what it used to be:
    /// while `gateway_url()` was just `addr`, an address that died left the
    /// Dashboard, the `deploy` and the transfer with nowhere to fall back to, even with a
    /// live address already announced in `addr_candidates`.
    pub fn gateway_candidates(&self) -> Vec<String> {
        self.gateway_candidates_with_tls(crate::peer_client::tls_enabled())
    }

    /// Gateway URLs explicitly using the scheme of an announce.
    ///
    /// The TLS, logs and Chat gates consult the principal before adopting the
    /// announced value, so they cannot depend on the global flag that
    /// is still being confirmed.
    pub fn gateway_candidates_with_tls(&self, tls: bool) -> Vec<String> {
        let Some(port) = self.gateway_port else {
            return Vec::new();
        };
        let mut base: Vec<&str> = vec![self.addr.as_str()];
        base.extend(self.addr_candidates.iter().map(String::as_str));
        let mut urls: Vec<String> = Vec::with_capacity(base.len());
        for a in base {
            if !is_peer_destination(&self.node_id, a) {
                continue;
            }
            let url = crate::peer_client::peer_base_url(&ip_host(a), port, tls);
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
        urls
    }
}

pub struct Network {
    pub node_id: String,
    pub pubkey: String,
    pub bind_addr: SocketAddr,
    /// Destination of the local announce. In production it is the global IPv4 broadcast; the
    /// testkit injects a loopback destination so it never reaches the LAN.
    broadcast_addr: SocketAddr,
    pub peers: Arc<RwLock<HashMap<String, PeerInfo>>>,
    pub bootstrap_nodes: Vec<String>,
    /// Port of the local Ollama instance, advertised in P2P messages.
    pub ollama_port: Option<u16>,
    /// Port of the local llama-cpp-python inference server.
    pub inference_port: Option<u16>,
    /// Dedicated UDP port of the local inference HTTP/3 endpoint.
    pub inference_quic_port: Option<u16>,
    /// Port of this node's HTTP API / web UI.
    pub gateway_port: Option<u16>,
    /// Dedicated UDP port of the local gateway HTTP/3 endpoint.
    pub gateway_quic_port: Option<u16>,
    /// Public announces that have not yet proven membership in the local network.
    ///
    /// This table is deliberately separate from `peers`: public candidates
    /// may be consulted by the directory and by pkarr, but never enter the
    /// Dashboard, pipeline planning or any member control
    /// surface before normal admission.
    public_discovery: Arc<RwLock<HashMap<String, PeerInfo>>>,
    /// Advertised in every `Announce` so peers can name this machine without a
    /// second HTTP round-trip.
    pub profile: Arc<std::sync::RwLock<crate::profile::NodeProfile>>,
    /// The name this machine goes by: `node_name` from the config, or the
    /// system hostname. Read with `hostname()`; the operator renames the
    /// machine live with `set_hostname`.
    hostname: std::sync::RwLock<String>,
    /// Address peers should keep for this node, already `host:port`.
    ///
    /// `None` means "work it out from where my packets came from", which is
    /// right on a LAN and wrong on the public internet: with privacy extensions
    /// on (the default on macOS and on most Linux desktops) the source address
    /// is a *temporary* IPv6 that rotates every few hours, so every peer ends up
    /// holding an address that stops answering. Setting this pins the stable
    /// GUA. See `advertised_addr`.
    ///
    /// `std::sync::RwLock` (not `tokio::sync::RwLock`) because `set_advertise_addr`
    /// never needs to hold it across an `.await` — same reasoning as
    /// `observed_addr` below, which this field now mirrors: the web UI can
    /// change this live, via `POST /api/ui/network/advertise-addr`, without a
    /// process restart.
    advertise_addr: std::sync::RwLock<Option<String>>,
    /// Tensor parallelism identity of this node (path C). `None` outside
    /// a TP group. Mutable like `advertise_addr`: `apply` can turn the
    /// node on as primary/worker without restarting the process.
    tp: std::sync::RwLock<Option<TpIdentity>>,
    /// Capacity that the last local `/health` confirmed. Memory fit may
    /// reduce the effective context after llama.cpp loads the GGUF.
    runtime_capacity: std::sync::RwLock<RuntimeCapacityState>,
    idle: AtomicBool,
    socket: Arc<UdpSocket>,
    /// Shared first-access LAN handshake state. It uses this same UDP socket
    /// so setup and configured runtime do not drift into separate protocols.
    local_discovery: Arc<crate::local_discovery::LocalDiscoveryRuntime>,
    #[cfg(test)]
    test_panic_on_next_message: AtomicBool,
    /// Set while the operator has taken this node off the network.
    ///
    /// A detached node keeps serving its own UI and API — that is how the
    /// operator brings it back — but it stops announcing, stops accepting
    /// anything peers say to it, and forgets everyone it knew. Without the
    /// second half it would be re-discovered on the next gossip tick and the
    /// button would look like it did nothing.
    detached: AtomicBool,
    /// How this node talks to each peer, as measured by this process.
    pub routes: Arc<crate::route::RouteTable>,
    /// Directional RTT from this node to each peer, measured over the
    /// endpoint tunnel when available.
    pub rtts: Arc<crate::route::RttTable>,
    /// This node's iroh `EndpointId`: the hex of the node's public key, the
    /// same string as `pubkey`. Set once, after construction, by `main.rs`,
    /// independent of whether the endpoint actually came up - see
    /// `p2p_tunnel_ready` for that.
    pub endpoint_id: Option<String>,
    /// Whether the endpoint actually bound and can serve inbound tunnel
    /// requests right now. Separate from `endpoint_id` on purpose: the key
    /// exists even when `Discovery::new` then fails (e.g. the UDP port is
    /// already taken) - advertising a capability with no endpoint behind it
    /// would send peers to dial a key that can never answer.
    /// Mutable through `&self` (mirrors `detached`) because it's known only
    /// after `Network` is already behind an `Arc` and shared.
    p2p_tunnel_ready: AtomicBool,
    /// Is the `nextn` (MTP) draft head actually mounted in this node's engine?
    /// Only the inference server knows - it depends on the loaded GGUF, not
    /// on the configuration: a model without a `nextn` block, or any slice that is not
    /// the final one, has nothing to attach. Hence the `Arc`: the engine lives on
    /// another task and is the one that writes here, while the announce (`self_announce`)
    /// and the gateway's local entry read. Announcing `speculate/v1-mtp` without a
    /// head would make the gateway ask for a draft that never comes.
    mtp_ready: Arc<AtomicBool>,
    /// The inference server loaded a model and has the multi-item scheduler
    /// active. It is false during setup, loading, TP and teardown;
    /// announcing the capability before that would make a peer send v3 to a
    /// port that only knows how to answer the old protocol.
    batching_ready: Arc<AtomicBool>,
    /// Admission gate. Absent only in the legacy `testkit`; real processes
    /// always configure it before starting the Network.
    admission: Option<Arc<PeerAdmission>>,
    admission_cache: std::sync::RwLock<HashMap<String, AdmissionCacheEntry>>,
    /// Peers with an admission probe in progress outside the transport task (see
    /// `admit_in_background`), so an announce every 5 s does not pile up probes.
    pending_admissions: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Live endpoint connections per endpoint id, fed by the link tracker
    /// (`discovery.rs`). It is what says whether a peer is reached directly
    /// or only through a relay - see `peer_route`.
    p2p_links: std::sync::RwLock<HashMap<String, Vec<(usize, P2pLink)>>>,
    /// URLs of the dedicated relays (`relay_peers`). A path through any other
    /// relay goes through a member relay or a public one (n0's) - see
    /// `peer_route`.
    dedicated_relays: std::sync::RwLock<std::collections::HashSet<String>>,
    /// Whom this node relays for.
    relay_gate: Arc<RelayGate>,
    /// This node has `slot_save_path` configured and serves `POST
    /// /v1/session/slot`. It becomes true on the `apply` of a deployment and goes back
    /// to false on stop; the gateway requires the cap from all hops before
    /// orchestrating a deployment snapshot.
    slot_persist_ready: Arc<AtomicBool>,
    /// The plugin whose inference service serves this node's model, when it
    /// is not POM's own engine. While set, the announce carries
    /// `engine/plugin-v1:<code>` instead of the hidden-state capabilities, so
    /// no gateway plans this peer as a pipeline hop. See `plugin_engine`.
    external_engine: std::sync::RwLock<Option<String>>,
    /// Whether this node speaks TLS with the peers, in the form the announce reads.
    ///
    /// The source of truth is `Config.tls_required`; this is the copy that gossip
    /// and the accept loop consult without touching the configuration lock.
    tls: Arc<AtomicBool>,
    /// How the gate is triggered: it receives the principal's node_id when an
    /// announce from it disagrees with the local policy.
    ///
    /// It is a channel, and not a direct call, because `Network` does not know the
    /// configuration nor the connection key - whoever knows how to fetch and store is the task
    /// set up in `main.rs`. See `tls_policy`.
    tls_probe: std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    /// Channel of the QUIC policy's authenticated gate.
    quic_probe: std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    /// Channel of the logs policy's authenticated gate.
    cluster_logs_probe:
        std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    /// Local log capture policy. The announced value is a trigger, never
    /// adopted directly.
    cluster_logs_enabled: Arc<AtomicBool>,
    /// Local policy of the global conversation channel. The announced boolean is
    /// only a trigger; the source is the authenticated read on the principal.
    network_chat_probe:
        std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    network_chat_enabled: Arc<AtomicBool>,
    /// A node's UI needs the full `NetworkSettings` document, but
    /// the public directory only carries the subset needed to join the
    /// network. This flag makes the principal's first authenticated Announce
    /// trigger the fetch of the full document.
    network_settings_probe:
        std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    network_settings_sync_needed: AtomicBool,
    /// Public key of the authority whose authenticated policy is authoritative.
    /// The profile in an Announce is not enough: a secondary admin also has
    /// `profile: admin`, but must never become a policy source.
    policy_principal_pubkey: std::sync::RwLock<Option<String>>,
    /// The version of the local trust chain, announced to the peers.
    trust_version: std::sync::RwLock<Option<String>>,
    /// Avoids repeating in gossip a permanent warning, such as the global broadcast
    /// being unavailable inside a container.
    broadcast_warning_logged: AtomicBool,
    /// This node's own address, as the relays saw it (the endpoint's NAT
    /// report, `global_v4`) with this node's UDP port. `host:port`, same
    /// shape as `advertise_addr`. Set from `discovery.rs`'s policy round, so
    /// `std::sync::RwLock` rather than
    /// `tokio::sync::RwLock` — the setter never needs to hold it across an
    /// `.await`. Never overrides an operator-pinned `advertise_addr`; only
    /// fills the gap when nothing was pinned, which otherwise leaves a NAT'd
    /// node with nothing to offer remote peers but a private LAN IP from
    /// local-interface enumeration. See `set_observed_addr`.
    observed_addr: std::sync::RwLock<Option<String>>,
    /// Endpoint addresses that the p2p endpoint publishes today, maintained by
    /// `Discovery` (see `set_endpoint_addrs`). They live here, and not in `Discovery`,
    /// because whoever builds the `Announce` is `Network`.
    endpoint_addrs: std::sync::RwLock<Vec<String>>,
    /// Traversal diagnostics, maintained by `Discovery`. See `TraversalStatus`.
    traversal: std::sync::RwLock<TraversalStatus>,
    /// Peers whose address this node is rediscovering. See
    /// `request_peer_addresses`.
    rediscovery: std::sync::RwLock<HashMap<String, Rediscovery>>,
}

/// An ongoing search for a peer's current address.
struct Rediscovery {
    /// When the search started. It is what decides until when the screen may say
    /// "rediscovering" (`REDISCOVERY_GIVE_UP`).
    since: Instant,
    /// When the last question went out, so as not to repeat before
    /// `REDISCOVERY_INTERVAL`.
    last_question: Instant,
}

/// The first 12 bytes of an id, for the log, **without cutting in the middle of a
/// character and without blowing up on a short id**.
///
/// `&node_id[..12]` on a two-character id brought down the captain's cluster on
/// 2026-08-16: a `PeerList` with a short id arrived by broadcast on the LAN, the
/// `receive_loop` panicked at `byte index 12 is out of bounds of 'eu'`,
/// **the task died** and the node stopped receiving UDP forever - while HTTP,
/// gossip and the TUI stayed healthy, and the only clue was a socket with a full
/// receive queue. The id comes from the network: anything the other side sends
/// has to be treated as arbitrary text, not as a 64-hex SHA256.
pub(crate) fn short(id: &str) -> &str {
    let mut end = id.len().min(12);
    while end > 0 && !id.is_char_boundary(end) {
        end -= 1;
    }
    &id[..end]
}

const PANIC_LOG_INTERVAL: StdDuration = StdDuration::from_secs(10);

/// Limits the frequency of warnings when a defective entry makes the handler
/// panic on every datagram. The first panic is always visible;
/// the following ones are counted and show up in the next permitted warning.
#[derive(Debug, Default)]
struct PanicLogLimiter {
    last_logged: Option<Instant>,
    suppressed: u64,
}

impl PanicLogLimiter {
    fn record(&mut self, now: Instant) -> Option<u64> {
        match self.last_logged {
            None => {
                self.last_logged = Some(now);
                Some(0)
            }
            Some(last) if now.duration_since(last) >= PANIC_LOG_INTERVAL => {
                self.last_logged = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
            Some(_) => {
                self.suppressed += 1;
                None
            }
        }
    }
}

fn message_context(msg: &P2PMessage) -> String {
    match msg {
        P2PMessage::Announce { node_id, .. } => {
            format!("Announce node_id={}", short(node_id))
        }
        P2PMessage::PeerRequest { from } => format!("PeerRequest from={}", short(from)),
        P2PMessage::PeerList { peers } => format!("PeerList peers={}", peers.len()),
        P2PMessage::Heartbeat { node_id, .. } => {
            format!("Heartbeat node_id={}", short(node_id))
        }
        P2PMessage::LocalDiscover { from, port } => {
            format!("LocalDiscover from={} port={port}", short(from))
        }
        P2PMessage::LocalAutoDiscovery { message } => {
            format!("LocalAutoDiscovery {message:?}")
        }
    }
}

fn panic_context(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    "non-string panic payload".into()
}

fn peer_address_changed(previous: &PeerInfo, current: &PeerInfo) -> bool {
    previous.addr != current.addr
        || previous.addr_candidates != current.addr_candidates
        || previous.inference_port != current.inference_port
        || previous.gateway_port != current.gateway_port
}

/// Payload budget for one UDP datagram, in bytes.
///
/// The measured path from the captain's 5G tethering to his Linux box has an
/// MTU of 1450. IPv6 routers never fragment, so an oversized datagram depends on
/// the sender learning the path MTU from an ICMPv6 "Packet Too Big" - and mobile
/// networks are exactly where that message tends to be filtered, which turns the
/// reply into a silent black hole. 1200 is the conservative floor that IPv6
/// itself guarantees (RFC 8200 requires links to carry 1280), minus headroom for
/// the IPv6 + UDP headers.
pub const MAX_DATAGRAM_PAYLOAD: usize = 1200;

/// Split a peer list into datagrams that fit under [`MAX_DATAGRAM_PAYLOAD`].
///
/// A `PeerList` grows ~530 bytes per peer, so it crosses the 5G path MTU at the
/// third peer - which is the captain's own target topology (his Linux box, his
/// MacBook, his friend). Losing that reply is not fatal, because the 5 s gossip
/// `Announce` eventually carries the same peers, but it makes a new node take
/// far longer to see the cluster for no reason.
fn peer_list_chunks(peers: Vec<PeerInfo>) -> Vec<Vec<PeerInfo>> {
    let mut chunks: Vec<Vec<PeerInfo>> = Vec::new();
    let mut current: Vec<PeerInfo> = Vec::new();

    for peer in peers {
        current.push(peer);
        let size = serde_json::to_vec(&P2PMessage::PeerList {
            peers: current.clone(),
        })
        .map(|v| v.len())
        .unwrap_or(0);

        if size > MAX_DATAGRAM_PAYLOAD && current.len() > 1 {
            // The one that broke the budget starts the next datagram. A single
            // peer that does not fit on its own is still sent: dropping it would
            // hide a peer forever, and the datagram is only slightly oversized.
            let overflow = current.pop().expect("just pushed");
            chunks.push(std::mem::take(&mut current));
            current.push(overflow);
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Normalise an operator-supplied address into the `host:port` form that
/// travels in `Announce.addr` and is later parsed by `ip_host`.
///
/// The captain types the address by hand, so all of these have to work:
/// a bare IPv6 (`2804:d55::1`), a bracketed one with or without a port, an
/// IPv4, or a hostname. A bare IPv6 is the interesting case: it is *all* colons,
/// so it cannot be split on `:` to find a port - it has to be bracketed first.
pub fn advertised_addr(raw: &str, default_port: u16) -> Option<String> {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // Already a full socket address, in either family.
    if let Ok(socket) = raw.parse::<SocketAddr>() {
        return (socket.port() != 0).then(|| raw.to_string());
    }
    // "[2804:d55::1]" - bracketed, no port.
    if let Some(inner) = raw.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        if inner.parse::<Ipv6Addr>().is_ok() {
            return Some(format!("[{inner}]:{default_port}"));
        }
        return None;
    }
    // "2804:d55::1" - bare IPv6, needs brackets before a port can be appended.
    if raw.parse::<Ipv6Addr>().is_ok() {
        return Some(format!("[{raw}]:{default_port}"));
    }
    // "192.168.1.5"
    if raw.parse::<Ipv4Addr>().is_ok() {
        return Some(format!("{raw}:{default_port}"));
    }
    // A hostname, with or without a port. Anything with a colon left at this
    // point already carries one.
    if raw.contains(':') {
        if raw
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            == Some(0)
        {
            return None;
        }
        Some(raw.to_string())
    } else {
        Some(format!("{raw}:{default_port}"))
    }
}

impl Network {
    /// UDP socket that sees IPv6 and IPv4 on the same descriptor.
    ///
    /// `IPV6_V6ONLY` only takes effect if turned off **before** the `bind`, and std's
    /// `UdpSocket::bind` already turns it on. On Linux and macOS the system
    /// default is 0 and discovery worked by accident; on Windows the default
    /// is 1, and a node bound to `[::]` would receive no announce at all from an IPv4 peer
    /// - that is, no LAN peer. It is the same kind of silent,
    /// single-platform defect that the `normalize` below already guards with a test.
    fn bind_udp_dual_stack(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};

        let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_only_v6(false)?;
        // v4 broadcast leaves this socket precisely because of only_v6=false.
        let _ = sock.set_broadcast(true);
        sock.bind(&addr.into())?;
        sock.set_nonblocking(true)?;
        Ok(sock.into())
    }

    pub async fn new(
        node_id: String,
        pubkey: String,
        port: u16,
        bootstrap_nodes: Vec<String>,
        ollama_port: Option<u16>,
        inference_port: Option<u16>,
        gateway_port: Option<u16>,
    ) -> Result<Self> {
        // Discovery follows the configured UDP port. The default remains
        // DISCOVERY_PORT, but an isolated node must not broadcast to another
        // node's listener when its port is overridden.
        let discovery_port = if port == 0 { DISCOVERY_PORT } else { port };
        Self::new_with_broadcast_addr(
            node_id,
            pubkey,
            port,
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::BROADCAST), discovery_port),
            NetworkInit {
                bootstrap_nodes,
                ollama_port,
                inference_port,
                inference_quic_port: None,
                gateway_port,
                gateway_quic_port: None,
            },
        )
        .await
    }

    async fn new_with_broadcast_addr(
        node_id: String,
        pubkey: String,
        port: u16,
        broadcast_addr: SocketAddr,
        init: NetworkInit,
    ) -> Result<Self> {
        // Bind dual-stack IPv6 so peers on IPv6 (including link-local fe80::) can be reached.
        // Falls back to IPv4-only if the OS has IPv6 disabled.
        let (socket, bind_addr) = {
            let v6: SocketAddr = format!("[::]:{}", port).parse()?;
            match Self::bind_udp_dual_stack(v6) {
                Ok(s) => {
                    let socket = UdpSocket::from_std(s)?;
                    let bind_addr = socket.local_addr()?;
                    SELF_P2P_PORT.store(bind_addr.port(), std::sync::atomic::Ordering::Relaxed);
                    info!("UDP socket bound to [::]:{} (dual-stack)", bind_addr.port());
                    (socket, bind_addr)
                }
                Err(e) => {
                    info!("{}", e);
                    let v4: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
                    let s = UdpSocket::bind(v4).await.map_err(|error| {
                        anyhow::anyhow!(
                            "could not open the P2P UDP port {port}: {error}. Another program, \
                             or another POM, is using it; close it or start with {}=<free port>",
                            crate::brand::env_var("PORT")
                        )
                    })?;
                    s.set_broadcast(true)?;
                    let bind_addr = s.local_addr()?;
                    SELF_P2P_PORT.store(bind_addr.port(), std::sync::atomic::Ordering::Relaxed);
                    info!("UDP socket bound to 0.0.0.0:{} (IPv4)", bind_addr.port());
                    (s, bind_addr)
                }
            }
        };

        Ok(Self::from_bound_socket(
            node_id,
            pubkey,
            bind_addr,
            broadcast_addr,
            socket,
            init,
        ))
    }

    #[cfg(test)]
    pub(crate) async fn testkit(
        node_id: String,
        pubkey: String,
        bootstrap_nodes: Vec<String>,
        ollama_port: Option<u16>,
        inference_port: Option<u16>,
        gateway_port: Option<u16>,
    ) -> Result<Self> {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let bind_addr = socket.local_addr()?;
        Ok(Self::from_bound_socket(
            node_id,
            pubkey,
            bind_addr,
            bind_addr,
            socket,
            NetworkInit {
                bootstrap_nodes,
                ollama_port,
                inference_port,
                inference_quic_port: None,
                gateway_port,
                gateway_quic_port: None,
            },
        ))
    }

    #[cfg(test)]
    pub(crate) async fn testkit_with_broadcast_addr(
        node_id: String,
        pubkey: String,
        bootstrap_nodes: Vec<String>,
        ollama_port: Option<u16>,
        inference_port: Option<u16>,
        gateway_port: Option<u16>,
        broadcast_addr: SocketAddr,
    ) -> Result<Self> {
        if !broadcast_addr.ip().is_loopback() {
            return Err(anyhow::anyhow!(
                "the UDP testkit only accepts loopback destinations"
            ));
        }
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let bind_addr = socket.local_addr()?;
        Ok(Self::from_bound_socket(
            node_id,
            pubkey,
            bind_addr,
            broadcast_addr,
            socket,
            NetworkInit {
                bootstrap_nodes,
                ollama_port,
                inference_port,
                inference_quic_port: None,
                gateway_port,
                gateway_quic_port: None,
            },
        ))
    }

    fn from_bound_socket(
        node_id: String,
        pubkey: String,
        bind_addr: SocketAddr,
        broadcast_addr: SocketAddr,
        socket: UdpSocket,
        init: NetworkInit,
    ) -> Self {
        let socket = Arc::new(socket);
        let local_discovery =
            crate::local_discovery::LocalDiscoveryRuntime::new(socket.clone(), broadcast_addr);
        Self {
            node_id,
            pubkey,
            bind_addr,
            broadcast_addr,
            peers: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_nodes: init.bootstrap_nodes,
            ollama_port: init.ollama_port,
            inference_port: init.inference_port,
            inference_quic_port: init.inference_quic_port,
            gateway_port: init.gateway_port,
            gateway_quic_port: init.gateway_quic_port,
            public_discovery: Arc::new(RwLock::new(HashMap::new())),
            profile: Arc::new(std::sync::RwLock::new(crate::profile::NodeProfile::Node)),
            hostname: std::sync::RwLock::new(String::new()),
            advertise_addr: std::sync::RwLock::new(None),
            tp: std::sync::RwLock::new(None),
            runtime_capacity: std::sync::RwLock::new(RuntimeCapacityState::default()),
            idle: AtomicBool::new(false),
            socket,
            local_discovery,
            #[cfg(test)]
            test_panic_on_next_message: AtomicBool::new(false),
            detached: AtomicBool::new(false),
            routes: Arc::new(crate::route::RouteTable::new()),
            rtts: Arc::new(crate::route::RttTable::new()),
            endpoint_id: None,
            p2p_tunnel_ready: AtomicBool::new(false),
            mtp_ready: Arc::new(AtomicBool::new(false)),
            batching_ready: Arc::new(AtomicBool::new(false)),
            admission: None,
            admission_cache: std::sync::RwLock::new(HashMap::new()),
            pending_admissions: std::sync::Mutex::new(std::collections::HashSet::new()),
            p2p_links: std::sync::RwLock::new(HashMap::new()),
            dedicated_relays: std::sync::RwLock::new(Default::default()),
            relay_gate: Arc::new(RelayGate::new(false, true, true)),
            slot_persist_ready: Arc::new(AtomicBool::new(false)),
            external_engine: std::sync::RwLock::new(None),
            tls: Arc::new(AtomicBool::new(false)),
            tls_probe: std::sync::RwLock::new(None),
            quic_probe: std::sync::RwLock::new(None),
            cluster_logs_probe: std::sync::RwLock::new(None),
            cluster_logs_enabled: Arc::new(AtomicBool::new(false)),
            network_chat_probe: std::sync::RwLock::new(None),
            network_chat_enabled: Arc::new(AtomicBool::new(false)),
            network_settings_probe: std::sync::RwLock::new(None),
            network_settings_sync_needed: AtomicBool::new(true),
            policy_principal_pubkey: std::sync::RwLock::new(None),
            trust_version: std::sync::RwLock::new(None),
            broadcast_warning_logged: AtomicBool::new(false),
            observed_addr: std::sync::RwLock::new(None),
            endpoint_addrs: std::sync::RwLock::new(Vec::new()),
            traversal: std::sync::RwLock::new(TraversalStatus::new()),
            rediscovery: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Whom this node relays for - see `RelayGate`.
    pub fn with_relay_gate(mut self, gate: RelayGate) -> Self {
        self.relay_gate = Arc::new(gate);
        self
    }

    pub fn relay_gate(&self) -> Arc<RelayGate> {
        self.relay_gate.clone()
    }

    /// Label this node carries in its announces.
    pub fn with_identity(self, profile: crate::profile::NodeProfile, hostname: String) -> Self {
        *self.profile.write().expect("profile lock") = profile;
        self.set_hostname(hostname);
        self
    }

    /// The name this machine announces and every screen shows for it.
    pub fn hostname(&self) -> String {
        self.hostname.read().expect("hostname lock").clone()
    }

    /// Renames this machine live: the next announce carries the new name.
    pub fn set_hostname(&self, hostname: String) {
        *self.hostname.write().expect("hostname lock") = hostname;
    }

    /// Configures the proof of membership before starting the discovery
    /// tasks. A production process never uses the `testkit`'s open mode:
    /// without a valid private key or an anchored public chain, admission
    /// refuses the peer.
    pub(crate) fn with_admission(
        mut self,
        auth: crate::auth::AuthState,
        trust: Arc<crate::admin_trust::AdminTrust>,
    ) -> Self {
        self.admission = Some(Arc::new(PeerAdmission { auth, trust }));
        self.admission_cache
            .write()
            .expect("admission cache lock")
            .clear();
        self
    }

    pub async fn set_local_auto_discovery(&self, enabled: bool) -> anyhow::Result<()> {
        if !enabled {
            self.local_discovery.disable().await;
            return Ok(());
        }
        let Some(admission) = self.admission.as_ref() else {
            anyhow::bail!("local discovery requires the admission of the node");
        };
        if self.profile() != crate::profile::NodeProfile::Admin {
            anyhow::bail!("only the administrator can enable local discovery");
        }
        let cfg = admission.auth.read().await;
        let trust = admission.trust.document();
        if trust.setup.is_none() {
            anyhow::bail!(
                "local discovery requires the initial network data in the trust document"
            );
        }
        let document = serde_json::to_string_pretty(&trust)?;
        self.local_discovery
            .set_admin_material(enabled, document, cfg.connection_key.clone())
            .await
    }

    pub async fn local_auto_discovery_status(
        &self,
    ) -> crate::local_discovery::LocalDiscoveryStatus {
        self.local_discovery.status().await
    }

    pub async fn begin_local_auto_discovery_search(&self) -> anyhow::Result<String> {
        self.local_discovery
            .begin_search(Some(
                crate::local_discovery::DiscoveredMachine::from_system(
                    &crate::sysmon::sample().await,
                    Some(self.hostname()),
                ),
            ))
            .await
    }

    pub(crate) fn local_discovery(&self) -> Arc<crate::local_discovery::LocalDiscoveryRuntime> {
        self.local_discovery.clone()
    }

    /// Change the profile announced by this process after a local mutation.
    pub fn set_profile(&self, profile: crate::profile::NodeProfile) {
        *self.profile.write().expect("profile lock") = profile;
    }

    pub fn profile(&self) -> crate::profile::NodeProfile {
        *self.profile.read().expect("profile lock")
    }

    /// The trust chain version this node announces. Set by whoever changes
    /// the chain (`admin::publish_chain_version`).
    pub fn set_trust_version(&self, version: impl Into<String>) {
        if let Ok(mut current) = self.trust_version.write() {
            *current = Some(version.into());
        }
    }

    pub fn trust_version(&self) -> Option<String> {
        self.trust_version
            .read()
            .ok()
            .and_then(|current| current.clone())
    }

    /// Whether this node is the network's principal, as opposed to any
    /// admin: a secondary admin has the profile but not the principal's
    /// duties (relaying the chat, answering the lobby, the member list). The
    /// principal's key comes from the local trust chain; while none is set,
    /// the profile decides.
    pub fn is_principal(&self) -> bool {
        self.profile() == crate::profile::NodeProfile::Admin
            && self
                .policy_principal_pubkey
                .read()
                .ok()
                .and_then(|current| current.as_deref().map(|key| key == self.pubkey))
                .unwrap_or(true)
    }

    /// Whether `peer` is the principal this node's trust chain names.
    pub fn peer_is_principal(&self, peer: &PeerInfo) -> bool {
        self.policy_principal_pubkey
            .read()
            .ok()
            .and_then(|current| current.as_deref().map(|key| key == peer.pubkey))
            .unwrap_or(peer.profile == crate::profile::NodeProfile::Admin)
    }

    /// Record the endpoint id this node's endpoint ended up with, so
    /// `self_announce` and `GatewayState::all_peers`'s self-entry can
    /// advertise it. A no-op call (`None`) is valid — it just means this run
    /// has no stable p2p identity to offer peers for the NAT-tunnel path.
    pub fn with_endpoint_id(mut self, id: Option<String>) -> Self {
        self.endpoint_id = id;
        self
    }

    /// Pin the address peers should reach this node at, at construction time.
    pub fn with_advertise_addr(self, raw: Option<String>) -> Self {
        self.set_advertise_addr(raw.as_deref());
        self
    }

    /// Configures the dedicated HTTP/3 UDP ports before the node joins the
    /// cluster. They are announced together with the public address, never
    /// inferred from the TCP inference URL.
    pub fn with_quic_ports(mut self, inference: Option<u16>, gateway: Option<u16>) -> Self {
        self.inference_quic_port = inference;
        self.gateway_quic_port = gateway;
        self
    }

    /// Pin (or clear, with `None`) the address peers should reach this node
    /// at, live — no restart required. Normalizes the same way construction
    /// does (`advertised_addr`), so a malformed value is silently dropped
    /// rather than stored broken; callers that need to reject bad input
    /// outright (the web UI's save endpoint) should validate with
    /// `advertised_addr` themselves first and only call this on success.
    pub fn set_advertise_addr(&self, raw: Option<&str>) {
        let normalized = raw.and_then(|r| advertised_addr(r, self.bind_addr.port()));
        if let Some(ref a) = normalized {
            info!("Announcing the fixed address {a} (instead of the source address)");
        }
        *self.advertise_addr.write().expect("advertise_addr lock") = normalized;
    }

    /// Turns on (or off, with `None`) this node's tensor parallelism
    /// identity. `apply` calls this when it deploys the node as primary/worker of
    /// a TP group, without restarting the process.
    pub fn set_tp(&self, tp: Option<TpIdentity>) {
        *self.tp.write().expect("tp lock") = tp;
    }

    /// Tensor parallelism identity announced by this node, if any.
    pub fn tp_identity(&self) -> Option<TpIdentity> {
        self.tp.read().expect("tp lock").clone()
    }

    /// Host part of the advertised address, ready to interpolate into a URL
    /// (bracketed when it is IPv6). `None` when no address was pinned.
    pub fn advertised_host(&self) -> Option<String> {
        self.advertise_addr
            .read()
            .expect("advertise_addr lock")
            .as_deref()
            .map(|a| ip_host(a).to_string())
    }

    /// Record the public address the relays observed (the endpoint's NAT
    /// report - see `discovery.rs`'s `apply_policy`, the only caller) as
    /// evidence of this node's own public address. A no-op when
    /// `host_port`'s host is still private/loopback/link-local
    /// (`addrs::is_private_or_local`) — that's not new information, just a
    /// peer on the same LAN or a NAT rewriting within another private range.
    /// Publishes in the `Announce` the addresses the endpoint is reachable at
    /// today.
    ///
    /// Called by `Discovery` on every policy round (5 s). Returns `true` when
    /// the list changed, so the caller logs the change; the new announce goes
    /// out on the next gossip, in at most 5 s.
    pub fn set_endpoint_addrs(&self, addrs: Vec<String>) -> bool {
        let mut guard = self.endpoint_addrs.write().expect("endpoint_addrs lock");
        if *guard == addrs {
            return false;
        }
        *guard = addrs.clone();
        drop(guard);
        self.traversal.write().expect("traversal lock").addrs = addrs;
        true
    }

    /// Snapshot of the traversal for the UI and for the operator.
    pub fn traversal_status(&self) -> TraversalStatus {
        self.traversal.read().expect("traversal lock").clone()
    }

    pub fn set_nat_status(&self, nat: &str) {
        self.traversal.write().expect("traversal lock").nat = nat.to_string();
    }

    /// The relays this endpoint holds a live connection to (its home relay
    /// and any other it is attached to), by URL.
    pub fn set_relays_in_use(&self, urls: Vec<String>) {
        self.traversal.write().expect("traversal lock").reservations = urls;
    }

    pub fn remember_hole_punch(&self, result: Result<(), String>) {
        let mut guard = self.traversal.write().expect("traversal lock");
        match result {
            Ok(()) => {
                guard.hole_punch_ok += 1;
                guard.last_hole_punch_error = None;
            }
            Err(error) => {
                guard.hole_punch_failed += 1;
                guard.last_hole_punch_error = Some(error);
            }
        }
    }

    /// Announced endpoint addresses: the operator's hint (`advertise_addr`,
    /// with the endpoint port, `UDP + 10000`) first, and then what the
    /// endpoint publishes (its home relay, public direct addresses). Read on
    /// the spot, so changing the pin in the UI reaches the next `Announce`
    /// and the pkarr record without waiting for the policy round.
    pub fn endpoint_addrs(&self) -> Vec<String> {
        let mut addrs = Vec::new();
        let pinned = self
            .advertise_addr
            .read()
            .expect("advertise_addr lock")
            .clone();
        if let Some(address) = pinned.as_deref().and_then(pinned_endpoint_addr) {
            addrs.push(address);
        }
        for address in self.endpoint_addrs.read().expect("endpoint_addrs lock").iter() {
            if !addrs.contains(address) {
                addrs.push(address.clone());
            }
        }
        addrs
    }

    pub fn set_observed_addr(&self, host_port: &str) {
        if !has_nonzero_port(host_port) {
            return;
        }
        let host = ip_host(host_port)
            .trim_start_matches('[')
            .trim_end_matches(']');
        if crate::addrs::is_private_or_local(host) {
            return;
        }
        let mut guard = self.observed_addr.write().expect("observed_addr lock");
        if guard.as_deref() != Some(host_port) {
            info!("📡 Public address observed by a peer: {host_port}");
        }
        *guard = Some(host_port.to_string());
    }

    fn observed_addr(&self) -> Option<String> {
        self.observed_addr
            .read()
            .expect("observed_addr lock")
            .clone()
    }

    /// Host to use when the node needs to describe how peers can reach it.
    ///
    /// An explicit pin wins. Otherwise a current public local address wins
    /// over an older address learned through identify, while the observed
    /// address remains available for NAT'd nodes with no public interface.
    /// `pub` because `GatewayState::all_peers` uses the same choice for the
    /// node's own dashboard entry.
    pub fn effective_host(&self) -> Option<String> {
        choose_effective_host(
            self.advertised_host(),
            crate::addrs::local_addrs(),
            self.observed_addr(),
        )
    }

    /// Addresses this node announces about itself.
    ///
    /// The address pinned by the operator comes first. Without a pin, the current
    /// public addresses of the interfaces come before the one observed by
    /// identify, and the remaining local addresses close the list. That way, an
    /// IPv6 rotation on macOS does not leave an old observed address
    /// in front, but a node behind NAT still keeps its public fallback.
    pub fn self_addr_candidates(&self) -> Vec<String> {
        let pinned = self
            .advertise_addr
            .read()
            .expect("advertise_addr lock")
            .clone();
        announce_addresses(
            pinned,
            self.observed_addr(),
            crate::addrs::local_addrs(),
            self.bind_addr.port(),
        )
        .1
    }

    fn quic_addr_candidates_for(&self, port: Option<u16>) -> Vec<String> {
        let Some(port) = port else {
            return Vec::new();
        };
        self.self_addr_candidates()
            .into_iter()
            .map(|address| format!("{}:{port}", ip_host(&address)))
            .collect()
    }

    pub fn inference_quic_addr_candidates(&self) -> Vec<String> {
        self.quic_addr_candidates_for(self.inference_quic_port)
    }

    pub fn gateway_quic_addr_candidates(&self) -> Vec<String> {
        self.quic_addr_candidates_for(self.gateway_quic_port)
    }

    /// What this node puts in `Announce.addr`.
    ///
    /// `0.0.0.0:<port>` is the "ask the receiver" sentinel: `handle_message`
    /// replaces it with the source address of the packet. Without an explicit
    /// pin, a current public local address comes first. The peer-observed
    /// address remains the fallback for NAT'd nodes with no public interface.
    pub fn announce_addr(&self) -> String {
        let pinned = self
            .advertise_addr
            .read()
            .expect("advertise_addr lock")
            .clone();
        choose_announce_addr(
            pinned,
            self.observed_addr(),
            crate::addrs::local_addrs(),
            self.bind_addr.port(),
        )
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
        if !is_valid_destination(addr) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "non-routable destination",
            ));
        }
        let resolved = loop {
            match tokio::net::lookup_host(addr).await {
                Ok(mut addresses) => {
                    break addresses.next().ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::InvalidInput, "no address")
                    })?
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        };
        loop {
            match self.socket.send_to(data, self.normalize(resolved)).await {
                Ok(_) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
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

    /// Sends to the known addresses until one send is accepted by the socket.
    /// The announced address is a cache, so an IPv6 rotation cannot
    /// prevent the announce from reaching the other live paths.
    async fn send_to_candidates(&self, msg: &P2PMessage, addresses: &[String]) -> Result<()> {
        let data = serde_json::to_vec(msg)?;
        let mut last_error = None;
        for address in addresses {
            match self.send_bytes(&data, address).await {
                Ok(()) => {
                    debug!("Sent {:?} to {}", std::mem::discriminant(msg), address);
                    return Ok(());
                }
                Err(error) => last_error = Some((address, error)),
            }
        }
        if let Some((address, error)) = last_error {
            warn!("Failed to send to {}: {}", address, error);
        }
        Ok(())
    }

    /// Broadcast to the configured discovery destination.
    pub async fn broadcast_local(&self, msg: &P2PMessage) -> Result<()> {
        let data = serde_json::to_vec(msg)?;
        if let Err(e) = self
            .send_bytes(&data, &self.broadcast_addr.to_string())
            .await
        {
            if !self.broadcast_warning_logged.swap(true, Ordering::Relaxed) {
                debug!("LAN broadcast failed: {e}");
            }
        }
        Ok(())
    }

    /// Build this node's `Announce`.
    ///
    /// A node configured through the web UI has no `LayerAssignment` until a
    /// model is bound to it. It must still announce itself, otherwise it is
    /// invisible to the rest of the cluster — it just announces zero layers.
    ///
    /// This is the node's **only** `Announce` builder: UDP (bootstrap,
    /// peers, broadcast and the reply to `PeerRequest`) and the periodic publish on
    /// gossip all go through here. A second builder in `main.rs`
    /// announced fewer capabilities, with empty `role`/`hostname` and only when
    /// there was an assignment, so a peer that arrived over the internet through a
    /// different node saw something different from what the LAN saw.
    fn self_announce(&self, assignment: Option<&LayerAssignment>) -> P2PMessage {
        // The enumeration is deliberately done here, on each announce. Gossip
        // announces every 5 s; the operation is only a local read of
        // interfaces, not a network probe. The same snapshot feeds `addr`
        // and `addr_candidates`, avoiding publishing two different photographs
        // if the temporary IPv6 rotates during assembly.
        let local = crate::addrs::local_addrs();
        let pinned = self
            .advertise_addr
            .read()
            .expect("advertise_addr lock")
            .clone();
        let observed = self.observed_addr();
        let (addr, addr_candidates) =
            announce_addresses(pinned, observed, local, self.bind_addr.port());
        P2PMessage::Announce {
            node_id: self.node_id.clone(),
            pubkey: self.pubkey.clone(),
            cluster_id: self.cluster_id(),
            trust_version: self.trust_version(),
            relay: self.relay_gate.offer(),
            addr,
            addr_candidates,
            model: assignment.map(|a| a.model.clone()).unwrap_or_default(),
            deploy_id: assignment.map(|a| a.deploy_id.clone()).unwrap_or_default(),
            start_layer: assignment.map(|a| a.start_layer).unwrap_or(0),
            end_layer: assignment.map(|a| a.end_layer).unwrap_or(0),
            total_layers: assignment.map(|a| a.total_layers).unwrap_or(0),
            memory_mb: assignment.map(|a| a.memory_used_mb).unwrap_or(0.0),
            timestamp: Utc::now(),
            ollama_port: self.ollama_port,
            inference_port: self.inference_port,
            inference_quic_port: self.inference_quic_port,
            // Keep this list in sync with the other two `Announce` builders
            // (gateway.rs, infer_server.rs) and with the capability constants
            // in routing/pipeline.rs (`CAP_RELAY_CHAIN`, `CAP_SPECULATE_LOOKUP`,
            // `CAP_P2P_TUNNEL`). The last one only goes out once the endpoint
            // has actually come up (`p2p_tunnel_ready`) — claiming it any
            // earlier would send peers to dial an endpoint id that can't answer.
            inference_frame_capabilities: if let Some(caps) = self.plugin_engine_capabilities() {
                // A plugin engine serves the whole model: it takes prompts,
                // not hidden states.
                caps
            } else {
                let mut caps = vec![
                    crate::pipeline::CAP_FRAME_BATCH.into(),
                    crate::pipeline::CAP_FRAME_DTYPE_F8.into(),
                    crate::pipeline::CAP_FRAME_DTYPE_I4G64.into(),
                    crate::pipeline::CAP_FRAME_DTYPE_I4G128.into(),
                    crate::pipeline::CAP_REMOTE_TOKEN_STREAM.into(),
                    crate::pipeline::CAP_LINK_STREAM.into(),
                    "relay/v1-chain".into(),
                    "relay/v2-direct".into(),
                    "speculate/v1-lookup".into(),
                    // Every new binary speaks the sliced MTP protocol
                    // (prefabricated draft + tokens + mtp_k on the wire);
                    // `supports_mtp` requires the cap from all steps to
                    // exclude an old peer from the ring.
                    "speculate/v1-mtp-sliced".into(),
                ];
                if self.mtp_ready() {
                    caps.push("speculate/v1-mtp".into());
                }
                if self.batching_ready() {
                    caps.push(crate::pipeline::CAP_FRAME_BATCH.into());
                }
                if self.slot_persist_ready() {
                    caps.push(crate::pipeline::CAP_SLOT_PERSIST.into());
                }
                if self.p2p_tunnel_ready() {
                    caps.push("transport/iroh-tunnel-v1".into());
                }
                caps
            },
            gateway_port: self.gateway_port,
            gateway_quic_port: self.gateway_quic_port,
            profile: self.profile(),
            tls: self.tls_enabled(),
            quic: crate::peer_client::quic_enabled(),
            hostname: self.hostname(),
            capabilities: assignment
                .map(|a| {
                    vec![ModelCapability {
                        model_id: a.model.clone(),
                        start_layer: a.start_layer,
                        end_layer: a.end_layer,
                        total_layers: Some(a.total_layers),
                        supports_inference: self.inference_port.is_some(),
                    }]
                })
                .unwrap_or_default(),
            disk_total_mb: 0,
            disk_free_mb: 0,
            endpoint_id: self.endpoint_id.clone(),
            endpoint_addrs: self.endpoint_addrs(),
            tp: self.tp.read().expect("tp lock").clone(),
            runtime_capacity: self
                .runtime_capacity
                .read()
                .expect("runtime capacity lock")
                .capacity,
            cluster_logs_enabled: self.cluster_logs_enabled(),
            idle: self.is_idle(),
            network_chat_enabled: self.network_chat_enabled(),
        }
    }

    /// The serialized announce that the periodic gossip publish sends.
    ///
    /// It exists so that the p2p path has no builder of its own: it is the same
    /// `self_announce` as UDP, with the same capabilities, the same `role` and the same
    /// `hostname`, and it also goes out when there is no assignment.
    pub fn gossip_announce_bytes(&self, assignment: Option<&LayerAssignment>) -> Option<Vec<u8>> {
        serde_json::to_vec(&self.self_announce(assignment)).ok()
    }

    /// True while the operator has taken this node off the network.
    pub fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Relaxed)
    }

    pub fn is_idle(&self) -> bool {
        self.idle.load(Ordering::Relaxed)
    }

    pub fn set_idle(&self, idle: bool) {
        self.idle.store(idle, Ordering::Relaxed);
    }

    /// Called once `Discovery::new` has actually succeeded (or failed) —
    /// see the field's own doc comment for why this is tracked separately
    /// from `endpoint_id`.
    pub fn set_p2p_tunnel_ready(&self, ready: bool) {
        self.p2p_tunnel_ready.store(ready, Ordering::Relaxed);
    }

    pub fn p2p_tunnel_ready(&self) -> bool {
        self.p2p_tunnel_ready.load(Ordering::Relaxed)
    }

    pub(crate) fn cluster_id(&self) -> Option<String> {
        self.admission
            .as_ref()
            .and_then(|admission| admission.trust.cluster_id())
    }

    /// The handle the inference server writes when it did (not) manage to
    /// attach the `nextn` head. See the `mtp_ready` field.
    pub fn mtp_ready_handle(&self) -> Arc<AtomicBool> {
        self.mtp_ready.clone()
    }

    pub fn mtp_ready(&self) -> bool {
        self.mtp_ready.load(Ordering::Relaxed)
    }

    /// Handle written by the inference supervisor once the batch server is
    /// ready to receive frames.
    pub fn batching_ready_handle(&self) -> Arc<AtomicBool> {
        self.batching_ready.clone()
    }

    pub fn batching_ready(&self) -> bool {
        self.batching_ready.load(Ordering::Relaxed)
    }

    pub fn set_batching_ready(&self, ready: bool) {
        self.batching_ready.store(ready, Ordering::Relaxed);
    }

    pub fn slot_persist_ready(&self) -> bool {
        self.slot_persist_ready.load(Ordering::Relaxed)
    }

    pub fn set_slot_persist_ready(&self, ready: bool) {
        self.slot_persist_ready.store(ready, Ordering::Relaxed);
    }

    /// See `external_engine`.
    pub fn set_external_engine(&self, plugin_code: Option<String>) {
        *self
            .external_engine
            .write()
            .unwrap_or_else(|error| error.into_inner()) = plugin_code;
    }

    pub fn external_engine(&self) -> Option<String> {
        self.external_engine
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// The inference capabilities while a plugin engine serves this node's
    /// model, `None` otherwise. Every builder of this node's capabilities -
    /// the announce, the gateway's own peer entry and `/api/ui/node` - goes
    /// through it, so no view of this node offers hidden-state hops then.
    pub fn plugin_engine_capabilities(&self) -> Option<Vec<String>> {
        let code = self.external_engine()?;
        let mut caps = vec![crate::plugin_engine::capability(&code)];
        if self.p2p_tunnel_ready() {
            caps.push("transport/iroh-tunnel-v1".into());
        }
        Some(caps)
    }

    /// Changes the generation that owns the capacity and withdraws it while the new runtime
    /// has not yet answered `/health`. The lock covers generation and capacity
    /// together, so an old watcher does not resurrect an announce after an
    /// `apply`.
    pub fn set_runtime_generation(&self, generation: u64) {
        if let Ok(mut state) = self.runtime_capacity.write() {
            state.generation = generation;
            state.capacity = None;
        }
    }

    /// Updates the capacity that will be sent in the next announce. Reserved for
    /// tests and for changes that already control the generation; the health watcher uses
    /// `set_runtime_capacity_for_generation` so as not to publish an old value.
    pub fn set_runtime_capacity(&self, capacity: Option<RuntimeCapacity>) {
        if let Ok(mut state) = self.runtime_capacity.write() {
            state.capacity = capacity;
        }
    }

    /// Publishes only if this health watcher still belongs to the current runtime.
    pub fn set_runtime_capacity_for_generation(
        &self,
        generation: u64,
        capacity: Option<RuntimeCapacity>,
    ) {
        if let Ok(mut state) = self.runtime_capacity.write() {
            if state.generation == generation {
                state.capacity = capacity;
            }
        }
    }

    /// Returns the capacity confirmed by the local `/health`.
    pub fn runtime_capacity(&self) -> Option<RuntimeCapacity> {
        self.runtime_capacity
            .read()
            .ok()
            .and_then(|state| state.capacity)
    }

    pub fn tls_enabled(&self) -> bool {
        self.tls.load(Ordering::Relaxed)
    }

    /// Turned on or off by the principal's route, or by the gate after
    /// confirming with it. Never from an Announce.
    pub fn set_tls(&self, on: bool) {
        self.tls.store(on, Ordering::Relaxed);
    }

    /// Wires up the gate's channel. Without it, an announce disagreement is ignored -
    /// which is the right behavior for a test node, with no config to write.
    pub fn set_tls_probe(&self, tx: tokio::sync::mpsc::UnboundedSender<(String, bool)>) {
        if let Ok(mut slot) = self.tls_probe.write() {
            *slot = Some(tx);
        }
    }

    pub fn set_quic_probe(&self, tx: tokio::sync::mpsc::UnboundedSender<(String, bool)>) {
        if let Ok(mut slot) = self.quic_probe.write() {
            *slot = Some(tx);
        }
    }

    pub fn cluster_logs_enabled(&self) -> bool {
        self.cluster_logs_enabled.load(Ordering::Acquire)
    }

    /// The policy only changes through the principal's interface or through the gate's
    /// authenticated answer. Never through the boolean received in gossip.
    pub fn set_cluster_logs_enabled(&self, enabled: bool) {
        self.cluster_logs_enabled.store(enabled, Ordering::Release);
    }

    pub fn set_cluster_logs_probe(&self, tx: tokio::sync::mpsc::UnboundedSender<(String, bool)>) {
        if let Ok(mut slot) = self.cluster_logs_probe.write() {
            *slot = Some(tx);
        }
    }

    pub fn network_chat_enabled(&self) -> bool {
        self.network_chat_enabled.load(Ordering::Acquire)
    }

    /// The policy only changes through the principal's interface or through the gate's
    /// authenticated answer. Never through the boolean received in gossip.
    pub fn set_network_chat_enabled(&self, enabled: bool) {
        self.network_chat_enabled.store(enabled, Ordering::Release);
    }

    pub fn set_network_chat_probe(&self, tx: tokio::sync::mpsc::UnboundedSender<(String, bool)>) {
        if let Ok(mut slot) = self.network_chat_probe.write() {
            *slot = Some(tx);
        }
    }

    pub fn set_network_settings_probe(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<(String, bool)>,
    ) {
        if let Ok(mut slot) = self.network_settings_probe.write() {
            *slot = Some(tx);
        }
    }

    pub fn mark_network_settings_synced(&self) {
        self.network_settings_sync_needed
            .store(false, Ordering::Release);
    }

    pub(crate) fn trigger_network_settings_probe(
        &self,
        node_id: &str,
        authority_pubkey: &str,
        tls: bool,
    ) {
        if !self.network_settings_sync_needed.load(Ordering::Acquire) {
            return;
        }
        let from_principal = self
            .policy_principal_pubkey
            .read()
            .ok()
            .and_then(|current| current.as_deref().map(|key| key == authority_pubkey))
            .unwrap_or(false);
        if !from_principal {
            return;
        }
        if let Ok(slot) = self.network_settings_probe.read() {
            if let Some(tx) = slot.as_ref() {
                let _ = tx.send((node_id.to_string(), tls));
            }
        }
    }

    /// Identifies the real principal for the policy gates. The value comes
    /// from the local administrative chain, never from an unauthenticated field of
    /// gossip.
    pub fn set_policy_principal_pubkey(&self, pubkey: impl Into<String>) {
        let pubkey = pubkey.into();
        if let Ok(mut current) = self.policy_principal_pubkey.write() {
            *current = (!pubkey.is_empty()).then_some(pubkey);
        }
    }

    pub fn trigger_quic_probe(
        &self,
        node_id: &str,
        authority_pubkey: &str,
        announced: bool,
        tls: bool,
    ) {
        let from_principal = self
            .policy_principal_pubkey
            .read()
            .ok()
            .and_then(|current| current.as_deref().map(|key| key == authority_pubkey))
            .unwrap_or(false);
        if let crate::quic_policy::PolicyOutcome::Fetch { principal } =
            crate::quic_policy::on_announce(
                crate::peer_client::quic_enabled(),
                announced,
                from_principal,
                node_id,
            )
        {
            if let Ok(slot) = self.quic_probe.read() {
                if let Some(tx) = slot.as_ref() {
                    // The announced value remains only the trigger. The policy
                    // task fetches the value from the principal's authenticated endpoint
                    // before adopting it.
                    let _ = tx.send((principal, tls));
                }
            }
        }
    }

    pub fn trigger_network_chat_probe(
        &self,
        node_id: &str,
        authority_pubkey: &str,
        announced: bool,
        tls: bool,
    ) {
        if let crate::network_chat_policy::PolicyOutcome::Fetch { principal } =
            crate::network_chat_policy::on_announce(
                self.network_chat_enabled(),
                announced,
                self.policy_principal_pubkey
                    .read()
                    .ok()
                    .and_then(|current| current.as_deref().map(|key| key == authority_pubkey))
                    .unwrap_or(false),
                node_id,
            )
        {
            if let Ok(slot) = self.network_chat_probe.read() {
                if let Some(tx) = slot.as_ref() {
                    let _ = tx.send((principal, tls));
                }
            }
        }
    }

    /// An announce is only a prompt to ask the principal for the effective
    /// policy. The announced value is never adopted directly, whether the
    /// announce arrived over UDP or gossip.
    pub fn trigger_cluster_logs_probe(
        &self,
        node_id: &str,
        authority_pubkey: &str,
        announced: bool,
        tls: bool,
    ) {
        if let crate::cluster_logs_policy::PolicyOutcome::Fetch { principal } =
            crate::cluster_logs_policy::on_announce(
                self.cluster_logs_enabled(),
                announced,
                self.policy_principal_pubkey
                    .read()
                    .ok()
                    .and_then(|current| current.as_deref().map(|key| key == authority_pubkey))
                    .unwrap_or(false),
                node_id,
            )
        {
            if let Ok(slot) = self.cluster_logs_probe.read() {
                if let Some(tx) = slot.as_ref() {
                    let _ = tx.send((principal, tls));
                }
            }
        }
    }

    /// Returns all live admin peers and their endpoint addresses. The list is
    /// used to replicate local data to every authority, not only to the
    /// principal.
    pub async fn admin_peers(&self) -> Vec<(crate::p2p::EndpointId, Vec<String>)> {
        let peers = self.peers.read().await;
        peers
            .values()
            .filter(|peer| peer.is_alive() && peer.profile == crate::profile::NodeProfile::Admin)
            .filter_map(|peer| {
                let peer_id = crate::p2p::parse_endpoint_id(peer.endpoint_id.as_deref()?)?;
                Some((peer_id, peer.endpoint_dialable_addrs()))
            })
            .collect()
    }

    /// Returns the known principal and its endpoint addresses for flows that
    /// still need a single next hop, such as the network Chat.
    pub async fn principal_peer(&self) -> Option<(crate::p2p::EndpointId, Vec<String>)> {
        let peers = self.peers.read().await;
        peers
            .values()
            .filter(|peer| peer.is_alive() && peer.profile == crate::profile::NodeProfile::Admin)
            .find_map(|peer| {
                let peer_id = crate::p2p::parse_endpoint_id(peer.endpoint_id.as_deref()?)?;
                Some((peer_id, peer.endpoint_dialable_addrs()))
            })
    }

    /// Leave the network: stop speaking, stop listening, forget the peers.
    pub async fn detach(&self) {
        self.detached.store(true, Ordering::Relaxed);
        let forgotten = { self.peers.write().await.drain().count() };
        info!("🔌 Node outside the network — {forgotten} peers forgotten");
    }

    /// Rejoin: start announcing again and go looking for the bootstrap nodes.
    pub async fn rejoin(&self, assignment: Option<&LayerAssignment>) -> Result<()> {
        self.detached.store(false, Ordering::Relaxed);
        self.network_settings_sync_needed
            .store(true, Ordering::Release);
        info!("🔌 Node returning to the network");
        self.bootstrap_discovery().await?;
        self.announce(assignment).await
    }

    /// Announce this node's existence and layer assignment to the network
    pub async fn announce(&self, assignment: Option<&LayerAssignment>) -> Result<()> {
        if self.is_detached() {
            return Ok(());
        }
        let msg = self.self_announce(assignment);

        // Announce to bootstrap nodes
        for bootstrap in &self.bootstrap_nodes {
            self.send_to(&msg, bootstrap).await?;
        }

        // Announce to known peers
        let peers = self.peers.read().await;
        for peer in peers.values() {
            if peer.is_alive() {
                let addresses = peer.dialable_addrs();
                self.send_to_candidates(&msg, &addresses).await?;
            }
        }

        // Broadcast locally
        self.broadcast_local(&msg).await?;

        Ok(())
    }

    /// Request peer list from bootstrap nodes
    pub async fn bootstrap_discovery(&self) -> Result<()> {
        // A node that is off the network must not go looking for it either.
        // `rejoin` clears the flag before calling this, so the live path is
        // unaffected; what this closes is `start`, which reaches out to the
        // bootstrap nodes on every path including a node started detached.
        if self.is_detached() {
            return Ok(());
        }
        let msg = P2PMessage::PeerRequest {
            from: self.node_id.clone(),
        };
        for bootstrap in &self.bootstrap_nodes {
            info!("Contacting bootstrap node: {}", bootstrap);
            self.send_to(&msg, bootstrap).await?;
        }
        Ok(())
    }

    /// Periodic discovery sweep: re-asks who the other peers' peers are, to the
    /// bootstrap nodes and to a rotating subset of the live
    /// peers.
    ///
    /// Without this, transitive discovery is a snapshot of startup: `PeerRequest`
    /// only went out once, and only to the bootstrap nodes. On the LAN the IPv4 broadcast
    /// hid the defect; across sites, a node that joined the neighbor's cluster later
    /// was never learned.
    ///
    /// Returns how many peers were asked on this tick (0 outside the sweep,
    /// 0 if detached), which is what the tests observe.
    pub async fn discovery_sweep(&self, tick: u64) -> Result<usize> {
        if self.is_detached() || tick == 0 || tick % DISCOVERY_SWEEP_TICKS != 0 {
            return Ok(0);
        }
        let msg = P2PMessage::PeerRequest {
            from: self.node_id.clone(),
        };

        for bootstrap in &self.bootstrap_nodes {
            self.send_to(&msg, bootstrap).await?;
        }

        // Stable order (by node_id) so the round-robin actually advances instead
        // of drawing the same peers on every round.
        let mut targets: Vec<Vec<String>> = {
            let peers = self.peers.read().await;
            let mut v: Vec<(&String, &PeerInfo)> = peers
                .iter()
                .filter(|(_, p)| p.is_alive() && !p.dialable_addrs().is_empty())
                .collect();
            v.sort_by(|a, b| a.0.cmp(b.0));
            v.into_iter().map(|(_, p)| p.dialable_addrs()).collect()
        };
        if targets.is_empty() {
            return Ok(0);
        }

        let sweep = (tick / DISCOVERY_SWEEP_TICKS) as usize;
        let len = targets.len();
        let start = sweep.wrapping_mul(SWEEP_FANOUT) % len;
        targets.rotate_left(start);
        targets.truncate(SWEEP_FANOUT.min(len));

        for addresses in &targets {
            self.send_to_candidates(&msg, addresses).await?;
        }
        Ok(targets.len())
    }

    /// Re-asks the current address of a peer that became unreadable.
    ///
    /// Called when **every** known address of a peer is
    /// discredited. Insisting on a dead address does not resurrect it; what
    /// resurrects the peer is the network saying again where it is. Two fronts,
    /// because neither alone is enough:
    ///
    /// 1. `PeerRequest` to the bootstrap nodes and to the other live peers -
    ///    whoever answers sends its own `Announce` **and** the `PeerList`, and that is
    ///    where the address the lost peer announces today comes from;
    /// 2. the discredited candidates leave the local entry, so that the
    ///    peer's next `Announce` repopulates it from scratch instead of adding to
    ///    the corpse - `merge_observed` only appends.
    ///
    /// There is no LAN broadcast here, on purpose: the peer is still alive (it is the one
    /// in `alive_peers`) and its `Announce` arrives every
    /// `GOSSIP_INTERVAL_SECS` anyway. The broadcast would bring nothing that
    /// this `Announce` does not bring, and transmitting on the LAN from a read
    /// handler would break the rule that the route suite never speaks real
    /// UDP.
    ///
    /// Limited to once per `REDISCOVERY_INTERVAL` per peer: the Dashboard re-reads the
    /// cluster in a short cycle, and without a brake this would become a UDP storm
    /// per screen refresh. Returns `true` when the search actually went out.
    pub async fn request_peer_addresses(&self, node_id: &str) -> bool {
        if self.is_detached() {
            return false;
        }
        {
            let Ok(mut requests) = self.rediscovery.write() else {
                return false;
            };
            let now = Instant::now();
            match requests.get_mut(node_id) {
                Some(search) => {
                    if search.last_question.elapsed() < REDISCOVERY_INTERVAL {
                        return false;
                    }
                    search.last_question = now;
                }
                None => {
                    requests.insert(
                        node_id.to_string(),
                        Rediscovery {
                            since: now,
                            last_question: now,
                        },
                    );
                }
            }
        }

        // The corpse leaves the local entry. Without this the next `Announce` falls into
        // `merge_observed`, which only appends, and the dead address would stay
        // at the front of the queue forever.
        {
            let mut peers = self.peers.write().await;
            if let Some(peer) = peers.get_mut(node_id) {
                peer.addr_candidates
                    .retain(|addr| !self.routes.endpoint_is_discredited(node_id, addr));
            }
        }

        info!(
            "🔎 Rediscovering {}: no known address answers",
            short(node_id)
        );

        let question = P2PMessage::PeerRequest {
            from: self.node_id.clone(),
        };
        for bootstrap in &self.bootstrap_nodes {
            let _ = self.send_to(&question, bootstrap).await;
        }
        let neighbors: Vec<Vec<String>> = {
            let peers = self.peers.read().await;
            peers
                .values()
                .filter(|p| p.node_id != node_id && p.is_alive())
                .map(PeerInfo::dialable_addrs)
                .filter(|addresses| !addresses.is_empty())
                .collect()
        };
        for addresses in &neighbors {
            let _ = self.send_to_candidates(&question, addresses).await;
        }
        true
    }

    /// Whether this node is now rediscovering the peer: it already asked for the current address and
    /// is still inside the window in which it makes sense to wait for the answer.
    ///
    /// It is what separates "cannot be read" from "I am rediscovering". The screen
    /// needs the difference: the first is a dead end, the second is
    /// work in progress - and after `REDISCOVERY_GIVE_UP` it goes back to being the
    /// first, because calling work in progress what no longer moves would be the
    /// same dishonest warning with another word.
    pub fn is_rediscovering(&self, node_id: &str) -> bool {
        self.rediscovery
            .read()
            .ok()
            .and_then(|requests| requests.get(node_id).map(|search| search.since))
            .is_some_and(|since| since.elapsed() < REDISCOVERY_GIVE_UP)
    }

    /// Ages the ongoing search, so a test can reach the deadline without waiting.
    #[cfg(test)]
    pub fn age_rediscovery_for_test(&self, node_id: &str, amount: Duration) {
        if let Ok(mut requests) = self.rediscovery.write() {
            if let Some(search) = requests.get_mut(node_id) {
                search.since -= amount;
                search.last_question -= amount;
            }
        }
    }

    /// The peer became readable again: the search is over.
    ///
    /// Without this, a machine that recovered after a rotation would keep
    /// counting the time of the old search, and the next search would be born old.
    pub fn note_peer_read(&self, node_id: &str) {
        if let Ok(mut requests) = self.rediscovery.write() {
            requests.remove(node_id);
        }
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
            let mut tick: u64 = 0;
            loop {
                ticker.tick().await;
                tick = tick.wrapping_add(1);
                // Announce unconditionally: a node without a model still has to
                // be discoverable, otherwise a freshly configured cluster is
                // made of nodes that only listen and never speak.
                let asgn = assignment_gossip.read().await.clone();
                let _ = net_gossip.announce(asgn.as_ref()).await;
                // Continuous discovery: without this, whoever is only reachable outside the
                // LAN is never learned after startup.
                let _ = net_gossip.discovery_sweep(tick).await;
                net_gossip.cleanup_dead_peers().await;
            }
        });

        // Route measurement, in its own task: a slow probe cannot hold the
        // announce nor the cleanup of dead peers. With 9 peers and up to 4 candidates
        // each, the 3 s timeout per probe would exceed a minute - and the gossip
        // loop is what expires dead peers.
        let net_routes = self.clone();
        tokio::spawn(async move {
            // A cache of its own, and not the gateway's: this task is born inside
            // `Network`, which does not know the `GatewayState`. The short timeout is
            // the probe's, not inference's.
            let clients = crate::peer_client::PeerClients::new(Duration::from_secs(10));
            let mut ticker = interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                if net_routes.is_detached() {
                    continue;
                }
                // The new probe starts with no policy of its own. Without syncing the
                // client with the cluster's current state, the HTTPS candidates
                // fail silently on TLS and the dashboard is left without RTT of the
                // remote peers. Updating only the client keeps the URL scheme
                // under the responsibility of the already confirmed global policy.
                clients.set_tls(net_routes.tls_enabled());
                for peer in net_routes.alive_peers().await {
                    let needs_route = net_routes.routes.needs_measuring(&peer.node_id);
                    let needs_rtt = net_routes.rtts.needs_measuring(&peer.node_id);
                    if !needs_route && !needs_rtt {
                        continue;
                    }

                    if needs_route {
                        let candidates = peer.inference_candidates();
                        if !candidates.is_empty() {
                            net_routes
                                .routes
                                .measure_peer(&clients, &peer.node_id, &candidates)
                                .await;
                        }
                    }

                    if needs_rtt {
                        let peer_id = peer
                            .endpoint_id
                            .as_deref()
                            .and_then(crate::p2p::parse_endpoint_id);
                        let has_tunnel = peer_id.is_some()
                            && peer
                                .inference_frame_capabilities
                                .iter()
                                .any(|cap| cap == crate::pipeline::CAP_P2P_TUNNEL);
                        let tunnel_rtt =
                            match (has_tunnel, peer_id, crate::infer_tunnel::tunnel_sender()) {
                                (true, Some(peer_id), Some(sender)) => {
                                    crate::infer_tunnel::measure_rtt(
                                        sender,
                                        peer_id,
                                        Duration::from_secs(3),
                                    )
                                    .await
                                }
                                _ => None,
                            };

                        // The announced capability does not guarantee that the local endpoint
                        // is still ready. If the measurement through the tunnel fails,
                        // it measures the inference HTTP route itself as a fallback.
                        // This also renews the direct-health at each RTT
                        // interval, without waiting for the slower route interval.
                        let direct_health_rtt = if tunnel_rtt.is_none() {
                            net_routes
                                .routes
                                .measure_current_peer(&clients, &peer.node_id)
                                .await
                        } else {
                            None
                        };

                        if let Some((rtt_ms, transport)) =
                            crate::route::choose_rtt(tunnel_rtt, direct_health_rtt)
                        {
                            net_routes.rtts.remember(&peer.node_id, rtt_ms, transport);
                        } else {
                            net_routes.rtts.mark_failure(&peer.node_id);
                        }
                    }
                }
            }
        });

        // Per-edge route plan: measures in the background the transport to
        // each live peer, so the frame uses a pinned route instead of
        // rediscovering the ladder on every call. See `route_plan`.
        crate::route_plan::spawn_planner(self.clone());

        // Reach out to the configured bootstrap nodes right away. This belongs
        // here, not in the caller: every start path needs it, and the
        // gateway-only path used to skip it.
        self.bootstrap_discovery().await?;

        Ok(())
    }

    async fn receive_loop(&self, assignment: Arc<RwLock<Option<LayerAssignment>>>) {
        let mut buf = vec![0u8; 65535];
        let mut panic_log = PanicLogLimiter::default();
        loop {
            match self.socket.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let data = &buf[..len];
                    if let Ok(msg) = serde_json::from_slice::<P2PMessage>(data) {
                        let context = message_context(&msg);
                        if let Err(payload) =
                            AssertUnwindSafe(self.handle_message(msg, src, &assignment))
                                .catch_unwind()
                                .await
                        {
                            if let Some(suppressed) = panic_log.record(Instant::now()) {
                                warn!(
                                    source = %src,
                                    message = %context,
                                    panic = %panic_context(payload.as_ref()),
                                    suppressed,
                                    "UDP message handler panicked; receive loop continues"
                                );
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
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
        #[cfg(test)]
        if self
            .test_panic_on_next_message
            .swap(false, Ordering::SeqCst)
        {
            panic!("injected UDP message handler panic");
        }

        // A node the operator took off the network does not learn peers and does
        // not answer them. The socket stays bound so rejoining costs nothing.
        if self.is_detached() {
            return;
        }
        match msg {
            P2PMessage::Announce {
                node_id,
                pubkey,
                deploy_id,
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
                relay,
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

                let mut peer = PeerInfo {
                    deploy_id,
                    node_id: node_id.clone(),
                    pubkey,
                    cluster_id,
                    trust_version,
                    relay,
                    addr: peer_addr.clone(),
                    addr_candidates,
                    model: model.clone(),
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
                    endpoint_id,
                    endpoint_addrs,
                    tp,
                    runtime_capacity,
                    idle,
                };

                self.remember_public_discovery_peer(&peer).await;
                if !self.admit_peer(&peer).await {
                    debug!(
                        "Announce from {} dropped: peer without proof of membership",
                        short(&peer.node_id)
                    );
                    return;
                }
                if self.admission.is_some() {
                    peer.connection_verified = true;
                }

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
                        short(&node_id),
                        model,
                        start_layer,
                        end_layer,
                        ollama_tag,
                    );
                } else {
                    debug!(
                        "Peer update: {} layers={}-{}",
                        short(&node_id),
                        start_layer,
                        end_layer
                    );
                }

                // The TLS gate. The announce is a **trigger, never a value**: nothing
                // here stores the policy. Disagreeing with the principal only asks for an
                // authenticated fetch from it, and it is that answer that counts. See
                // `tls_policy`.
                if let crate::tls_policy::PolicyOutcome::Fetch { principal } =
                    crate::tls_policy::on_announce(
                        self.tls_enabled(),
                        tls,
                        peer.profile == crate::profile::NodeProfile::Admin,
                        &peer.node_id,
                    )
                {
                    if let Ok(slot) = self.tls_probe.read() {
                        if let Some(tx) = slot.as_ref() {
                            // The announced `tls` goes along, and serves **one
                            // thing only**: knowing which scheme to dial the
                            // probe with. Without it a node that still thinks the cluster is in
                            // cleartext would dial `http://` on a strict principal,
                            // be refused, and the gate could never
                            // turn on. Forging the hint does not change what is stored -
                            // it only makes the probe knock on the wrong door and fail.
                            let _ = tx.send((principal, tls));
                        }
                    }
                }

                self.trigger_quic_probe(&peer.node_id, &peer.pubkey, quic, tls);

                self.trigger_cluster_logs_probe(
                    &peer.node_id,
                    &peer.pubkey,
                    cluster_logs_enabled,
                    tls,
                );
                self.trigger_network_chat_probe(
                    &peer.node_id,
                    &peer.pubkey,
                    network_chat_enabled,
                    tls,
                );
                self.trigger_network_settings_probe(&peer.node_id, &peer.pubkey, tls);

                // The UDP source is a useful fallback only for announcements
                // without candidates, such as those of older binaries. When
                // there is an explicit list, it is the authority: adding the
                // source here would reintroduce the bridge or NAT address the
                // peer itself filtered out of its candidates.
                if peer.addr_candidates.is_empty() {
                    peer.merge_observed(&src.to_string());
                }

                let address_changed = {
                    let peers = self.peers.read().await;
                    peers
                        .get(&node_id)
                        .is_some_and(|previous| peer_address_changed(previous, &peer))
                };
                if address_changed {
                    self.routes.invalidate(&node_id);
                }

                self.insert_admitted_peer(peer).await;
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

                // Also send the list of peers we know about, split so that no
                // single datagram exceeds the path MTU.
                let peer_list: Vec<PeerInfo> = {
                    let peers = self.peers.read().await;
                    peers.values().filter(|p| p.is_alive()).cloned().collect()
                };
                for chunk in peer_list_chunks(peer_list) {
                    let reply = P2PMessage::PeerList { peers: chunk };
                    let _ = self.send_to(&reply, &src.to_string()).await;
                }
            }

            P2PMessage::PeerList { peers } => {
                for mut peer in peers {
                    if peer.node_id == self.node_id || peer.dialable_addr().is_none() {
                        continue;
                    }

                    self.remember_public_discovery_peer(&peer).await;
                    if !self.admit_peer(&peer).await {
                        debug!(
                            "PeerList dropped {}: peer without proof of membership",
                            short(&peer.node_id)
                        );
                        continue;
                    }
                    if self.admission.is_some() {
                        peer.connection_verified = true;
                    }

                    let mut known = self.peers.write().await;

                    let Some(previous) = known.get(&peer.node_id) else {
                        info!(
                            "📋 Learned peer from gossip: {} layers={}-{}",
                            short(&peer.node_id),
                            peer.start_layer,
                            peer.end_layer
                        );
                        known.insert(peer.node_id.clone(), peer);
                        continue;
                    };

                    // PeerList carries a snapshot taken by another node. Do
                    // not let an older snapshot roll back a fresher Announce,
                    // but do apply a newer one so transitively learned peers
                    // do not keep stale layer coverage forever.
                    if peer.last_seen <= previous.last_seen {
                        continue;
                    }

                    let address_changed = peer_address_changed(previous, &peer);
                    let connection_verified = previous.connection_verified;
                    let api_keys_active = previous.api_keys_active;
                    // The other node's snapshot is the list the peer announces
                    // **today**; what is inherited from here is only what this node knows and
                    // the other does not - the observed address, which sees through NAT.
                    //
                    // Inheriting the whole list made a dead candidate immortal:
                    // an address the peer stopped announcing came back in every
                    // `PeerList`, and the set only grew. With temporary IPv6,
                    // which rotates on its own, that is a new corpse per rotation.
                    // Whoever already proved the address does not answer does not bring it
                    // back.
                    if peer.addr_candidates.is_empty() {
                        for addr in &previous.addr_candidates {
                            if self.routes.endpoint_is_discredited(&peer.node_id, addr) {
                                continue;
                            }
                            peer.merge_observed(addr);
                        }
                    }
                    peer.connection_verified = connection_verified;
                    peer.api_keys_active = api_keys_active;
                    if address_changed {
                        self.routes.invalidate(&peer.node_id);
                    }
                    info!(
                        "📋 Refreshed peer from gossip: {} layers={}-{}",
                        short(&peer.node_id),
                        peer.start_layer,
                        peer.end_layer
                    );
                    known.insert(peer.node_id.clone(), peer);
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

            P2PMessage::LocalAutoDiscovery { message } => {
                self.local_discovery.handle(message, src).await;
            }
        }
    }

    async fn cleanup_dead_peers(&self) {
        let dead: Vec<String> = {
            let mut peers = self.peers.write().await;
            let dead: Vec<String> = peers
                .values()
                .filter(|p| !p.is_alive())
                .map(|p| p.node_id.clone())
                .collect();
            peers.retain(|_, p| p.is_alive());
            dead
        };
        self.public_discovery
            .write()
            .await
            .retain(|_, peer| peer.is_alive());
        if dead.is_empty() {
            return;
        }
        // The peer left: its address memory goes with it. Keeping it would make a
        // node that comes back with the same address be born discredited.
        for node_id in &dead {
            self.routes.forget(node_id);
            if let Ok(mut requests) = self.rediscovery.write() {
                requests.remove(node_id);
            }
        }
        info!("🧹 Removed {} timed-out peers", dead.len());
    }

    fn admission_cache_key(&self, peer: &PeerInfo, connection_key: &str) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}",
            peer.node_id,
            peer.pubkey,
            peer.addr,
            peer.addr_candidates.join(","),
            peer.gateway_port.unwrap_or_default(),
            hex::encode(Sha256::digest(connection_key.as_bytes()))
        )
    }

    fn node_id_matches_pubkey(node_id: &str, pubkey: &str) -> bool {
        let Ok(bytes) = hex::decode(pubkey) else {
            return false;
        };
        hex::encode(Sha256::digest(bytes)).eq_ignore_ascii_case(node_id)
    }

    /// The endpoint id is the public key itself (see `p2p.rs`): the binding
    /// is that both name the same key.
    fn peer_id_matches_pubkey(peer: &PeerInfo) -> bool {
        let Some(endpoint_id) = peer.endpoint_id.as_deref().and_then(crate::p2p::parse_endpoint_id)
        else {
            return false;
        };
        crate::p2p::parse_endpoint_id(&peer.pubkey) == Some(endpoint_id)
    }

    fn public_peer_is_admitted(
        peer: &PeerInfo,
        admission: &PeerAdmission,
        access_policy: crate::config::NetworkAccessPolicy,
    ) -> bool {
        if !Self::node_id_matches_pubkey(&peer.node_id, &peer.pubkey)
            || !Self::peer_id_matches_pubkey(peer)
        {
            return false;
        }
        let Some(cluster_id) = peer.cluster_id.as_deref() else {
            return false;
        };
        let Some(peer_id) = crate::identity::trust_peer_id_from_hex(&peer.pubkey) else {
            return false;
        };
        // `anyone_with_anchor`: the operator declared that any node that holds the
        // matching anchor participates. An ordinary worker never receives an
        // authority grant, so `admits_participant` would refuse it. Here the
        // bar is a valid local anchor for the announced `cluster_id`, added
        // to the identity binding already checked above (node_id <-> pubkey <-> authority
        // key <-> EndpointId, and the gossip Announce comes signed by that
        // EndpointId). The stricter policies keep requiring a grant.
        if matches!(
            access_policy,
            crate::config::NetworkAccessPolicy::AnyoneWithAnchor
        ) {
            return admission.trust.anchors_cluster(cluster_id);
        }
        let principal = crate::admin_trust::AuthorityPrincipal {
            node_id: peer.node_id.clone(),
            peer_id,
            pubkey: peer.pubkey.clone(),
        };
        admission.trust.admits_participant(cluster_id, &principal)
    }

    fn admission_urls(&self, peer: &PeerInfo) -> Vec<String> {
        let Some(port) = peer.gateway_port else {
            return Vec::new();
        };
        let mut addresses = vec![peer.addr.as_str()];
        addresses.extend(peer.addr_candidates.iter().map(String::as_str));
        let mut urls = Vec::new();
        for address in addresses {
            if !is_valid_destination(address) {
                continue;
            }
            let url = format!(
                "{}/handshake",
                crate::peer_client::peer_base_url(ip_host(address), port, self.tls_enabled())
            );
            if !urls.contains(&url) {
                urls.push(url);
            }
        }
        urls
    }

    async fn private_peer_is_admitted(
        &self,
        peer: &PeerInfo,
        connection_key: &str,
    ) -> AdmissionProbe {
        if !Self::node_id_matches_pubkey(&peer.node_id, &peer.pubkey) {
            return AdmissionProbe::Refused;
        }
        let tls = self.tls_enabled();
        let mut builder = reqwest::Client::builder()
            .connect_timeout(ADMISSION_TIMEOUT)
            .timeout(ADMISSION_TIMEOUT);
        if tls {
            let Ok(config) = crate::tls::client_config(&peer.node_id) else {
                return AdmissionProbe::Refused;
            };
            builder = builder.use_preconfigured_tls(config);
        }
        let Ok(client) = builder.build() else {
            return AdmissionProbe::Unreachable;
        };
        let body = serde_json::json!({
            "node_id": self.node_id,
            "pubkey": self.pubkey,
            "connection_key": connection_key,
            "nonce": null,
            "signature": null,
        });
        let approves = |status: u16, body: &serde_json::Value| {
            body.get("approved").and_then(serde_json::Value::as_bool) == Some(true)
                || (status == reqwest::StatusCode::FORBIDDEN.as_u16()
                    && body.get("reason").and_then(serde_json::Value::as_str)
                        == Some("challenge_required"))
        };
        for url in self.admission_urls(peer) {
            let Ok(response) = client.post(&url).json(&body).send().await else {
                continue;
            };
            let status = response.status().as_u16();
            let Ok(parsed) = response.json::<serde_json::Value>().await else {
                continue;
            };
            if approves(status, &parsed) {
                return AdmissionProbe::Approved;
            }
            if tls {
                return AdmissionProbe::Refused;
            }
        }
        // Last resort, and for a peer behind NAT/CGNAT the **only** one that
        // gets through: the probe through the p2p tunnel. Without it admission never concludes
        // for that peer, it does not enter the `peers` table, and routing, UI and
        // pipeline stay blind to it even with the hole punch ready.
        let Some(tunnel) = crate::peer_client::TunnelPeer::from_peer(peer) else {
            return AdmissionProbe::Unreachable;
        };
        let Ok(encoded) = serde_json::to_vec(&body) else {
            return AdmissionProbe::Unreachable;
        };
        let response = crate::peer_client::tunnel_gateway_call(
            &tunnel,
            "POST",
            "/handshake",
            encoded,
            vec![(
                reqwest::header::CONTENT_TYPE.to_string(),
                "application/json".to_string(),
            )],
            ADMISSION_TUNNEL_TIMEOUT,
        )
        .await;
        match response {
            Ok(response) => match response.json::<serde_json::Value>() {
                Ok(parsed) if approves(response.status, &parsed) => AdmissionProbe::Approved,
                Ok(_) => AdmissionProbe::Refused,
                Err(_) => AdmissionProbe::Unreachable,
            },
            Err(error) => {
                debug!(
                    "admission of {} through the tunnel did not complete: {error}",
                    short(&peer.node_id)
                );
                AdmissionProbe::Unreachable
            }
        }
    }

    /// Checks membership before any effect derived from the peer.
    ///
    /// The handshake's `challenge_required` answer already proves the shared
    /// key; completing the nonce signature would be an additional identity
    /// proof, not necessary for the current private admission.
    /// The network's signed trust document, as an admission grant carries it.
    pub(crate) fn trust_document_json(&self) -> Option<String> {
        let admission = self.admission.as_ref()?;
        if !admission.trust.is_anchored() {
            return None;
        }
        serde_json::to_string(&admission.trust.document()).ok()
    }

    pub(crate) async fn admit_peer(&self, peer: &PeerInfo) -> bool {
        if let Some(allowed) = self.admission_without_probe(peer).await {
            return allowed;
        }
        let Some(admission) = self.admission.as_ref() else {
            return true;
        };
        // The key this node presents now: during a rotation that is still
        // the old one until every member switches together.
        let connection_key = crate::auth::outbound_connection_key(&*admission.auth.read().await);
        let cache_key = self.admission_cache_key(peer, &connection_key);
        let probe = self.private_peer_is_admitted(peer, &connection_key).await;
        let Ok(mut cache) = self.admission_cache.write() else {
            return probe == AdmissionProbe::Approved;
        };
        let now = Instant::now();
        let previous_proof = cache.get(&cache_key).and_then(|entry| entry.proven_at);
        let entry = admission_verdict(probe, previous_proof, now);
        if probe == AdmissionProbe::Unreachable && entry.allowed {
            debug!(
                "admission of {}: no answer, kept by the previous proof",
                short(&peer.node_id)
            );
        }
        let allowed = entry.allowed;
        cache.insert(cache_key, entry);
        allowed
    }

    /// The selected path of connection `id` with `peer` is now `link`.
    pub(crate) fn note_link(&self, peer: &str, id: usize, link: P2pLink) {
        if let Ok(mut links) = self.p2p_links.write() {
            let entry = links.entry(peer.to_string()).or_default();
            entry.retain(|(existing, _)| *existing != id);
            entry.push((id, link));
        }
    }

    /// Records which relays are dedicated, so `peer_route` can tell a path
    /// through a dedicated relay from one through a member relay.
    pub(crate) fn set_dedicated_relays(&self, urls: impl IntoIterator<Item = String>) {
        if let Ok(mut dedicated) = self.dedicated_relays.write() {
            *dedicated = urls.into_iter().collect();
        }
    }

    /// The connection `id` with `peer` closed.
    pub(crate) fn drop_link(&self, peer: &str, id: usize) {
        if let Ok(mut links) = self.p2p_links.write() {
            if let Some(entry) = links.get_mut(peer) {
                entry.retain(|(existing, _)| *existing != id);
                if entry.is_empty() {
                    links.remove(peer);
                }
            }
        }
    }

    /// Live endpoint connections with the peer whose endpoint id is `peer_id`.
    pub fn p2p_links(&self, peer_id: &str) -> Vec<P2pLink> {
        let Ok(links) = self.p2p_links.read() else {
            return Vec::new();
        };
        links
            .get(peer_id)
            .map(|entry| entry.iter().map(|(_, link)| link.clone()).collect())
            .unwrap_or_default()
    }

    /// Gateway addresses of `peer`, in the order worth trying: first the
    /// host that last answered this node (`RouteTable::last_success`),
    /// then those that did not fail recently, in the announced order, and lastly
    /// the discredited ones.
    ///
    /// The first announced address may be an IPv6 that nobody reaches; an
    /// address that already answered is fact, not guess. `last_success` may have
    /// come from the inference port (the inference path feeds it too),
    /// which is why the comparison is by host.
    pub fn gateway_bases(&self, peer: &PeerInfo) -> Vec<String> {
        self.order_by_proven_host(&peer.node_id, peer.gateway_candidates())
    }

    /// `gateway_bases` for the peer's inference server.
    pub fn inference_bases(&self, peer: &PeerInfo) -> Vec<String> {
        self.order_by_proven_host(&peer.node_id, peer.inference_candidates())
    }

    /// The same order for a list of gateway addresses assembled by whoever
    /// calls - with the scheme announced by the peer, for example, as the
    /// policy gates do.
    pub fn order_gateway_bases(&self, node_id: &str, bases: Vec<String>) -> Vec<String> {
        self.order_by_proven_host(node_id, bases)
    }

    fn order_by_proven_host(&self, node_id: &str, mut bases: Vec<String>) -> Vec<String> {
        let proven_host = self
            .routes
            .last_success(node_id)
            .map(|url| url_host(&url).to_string());
        bases.sort_by_key(|base| {
            if proven_host.as_deref() == Some(url_host(base)) {
                0
            } else if self.routes.is_failed(node_id, base) {
                2
            } else {
                1
            }
        });
        bases
    }

    /// Whether a path through the relay at `relay_url` may carry a file
    /// transfer.
    ///
    /// Never through a public relay: n0's, one configured in `relay_peers`,
    /// or any other that is not the relay of an admitted member of this
    /// cluster. A member relay carries transfers unless it announces
    /// otherwise (`RelayOffer::transfers`).
    pub async fn relay_carries_transfers(&self, relay_url: &str) -> bool {
        if self
            .dedicated_relays
            .read()
            .is_ok_and(|dedicated| dedicated.contains(relay_url))
        {
            return false;
        }
        self.alive_peers()
            .await
            .iter()
            .filter_map(|peer| peer.relay.as_ref())
            .find(|offer| offer.url.as_deref() == Some(relay_url))
            .is_some_and(|offer| offer.transfers)
    }

    async fn link_carries_transfers(&self, link: &P2pLink) -> bool {
        if !link.relayed {
            return true;
        }
        match link.relay_url.as_deref() {
            Some(relay) => self.relay_carries_transfers(relay).await,
            None => false,
        }
    }

    /// Whether a connection whose selected path is `path` may carry a file
    /// transfer: a direct one (dialed or hole punched) always, a relayed one
    /// only through a member relay that allows it. No path, no transfer.
    pub async fn path_carries_transfers(&self, path: Option<&crate::p2p::LinkPath>) -> bool {
        match path {
            Some(crate::p2p::LinkPath::Direct(_)) => true,
            Some(crate::p2p::LinkPath::Relay(url)) => self.relay_carries_transfers(url).await,
            None => false,
        }
    }

    /// How many of this node's connections with `peer_id` may carry a file
    /// transfer right now, out of how many are alive. A relayed connection
    /// may turn direct when the hole punch closes, so the caller knows how
    /// often to expect a refusal, and that waiting may help.
    pub async fn transfer_links(&self, peer_id: &str) -> (usize, usize) {
        let links = self.p2p_links(peer_id);
        let mut allowed = 0;
        for link in &links {
            if self.link_carries_transfers(link).await {
                allowed += 1;
            }
        }
        (allowed, links.len())
    }

    /// The route in use for `peer`, in the order the control plane tries it:
    /// direct HTTP address that answered, direct endpoint path, path through
    /// a relay.
    pub fn peer_route(&self, peer: &PeerInfo) -> PeerRoute {
        let links = peer
            .endpoint_id
            .as_deref()
            .map(|id| self.p2p_links(id))
            .unwrap_or_default();
        let hole_punched = links.iter().any(|link| link.hole_punched);
        let direct = links.iter().find(|link| !link.relayed);
        let relayed = links.iter().find(|link| link.relayed);
        let (kind, address) = if let Some(url) = self.routes.last_success(&peer.node_id) {
            let address = url
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .to_string();
            ("http", Some(address))
        } else if let Some(link) = direct {
            ("p2p_direct", Some(link.address.clone()))
        } else if let Some(link) = relayed {
            (
                "relayed",
                Some(if link.address.is_empty() {
                    link.relay_url.clone().unwrap_or_default()
                } else {
                    link.address.clone()
                }),
            )
        } else {
            ("none", None)
        };
        let relay_kind = (kind == "relayed").then(|| {
            let relay = relayed.and_then(|link| link.relay_url.as_deref());
            let dedicated = self.dedicated_relays.read().ok();
            if relay.is_some_and(|relay| dedicated.is_some_and(|set| set.contains(relay))) {
                "dedicated"
            } else if relay.is_some_and(crate::p2p::is_default_relay) {
                "public"
            } else {
                "member"
            }
        });
        PeerRoute {
            kind,
            address,
            relay_kind,
            hole_punched,
            links,
        }
    }

    /// What `admit_peer` answers without opening a connection: the public network and the empty
    /// key are decided locally, and the private network from the cache. `None` when
    /// the answer depends on the `/handshake` probe.
    pub(crate) async fn admission_without_probe(&self, peer: &PeerInfo) -> Option<bool> {
        let Some(admission) = self.admission.as_ref() else {
            return Some(true);
        };
        let (connection_key, open_admission, access_policy) = {
            let config = admission.auth.read().await;
            (
                crate::auth::outbound_connection_key(&config),
                config.network.open_admission(),
                config.network.access_policy,
            )
        };
        // A peer that announces another network is not a member, and there is
        // nothing to probe. All networks share one announce topic, so without
        // this every announce of every other network cost a `/handshake`
        // probe, which falls back to a relay circuit for a peer behind NAT -
        // and it was those circuits, never going to be useful, that used up
        // the relay. A peer that announces no network is still probed.
        if let Some(theirs) = peer
            .cluster_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        {
            if admission.trust.is_anchored() && !admission.trust.anchors_cluster(theirs) {
                return Some(false);
            }
        }
        // Only a public network left in the open mode admits by the trust
        // anchor alone. Every other network, listed or not, asks for the
        // connection key, which reaches a new machine through an admin's
        // approval of its admission request.
        if open_admission {
            return Some(Self::public_peer_is_admitted(
                peer,
                admission,
                access_policy,
            ));
        }
        if connection_key.is_empty() {
            return Some(false);
        }
        let cache_key = self.admission_cache_key(peer, &connection_key);
        let cache = self.admission_cache.read().ok()?;
        cache
            .get(&cache_key)
            .filter(|entry| entry.expires_at > Instant::now())
            .map(|entry| entry.allowed)
    }

    /// Runs the admission probe on a task of its own and only warms the cache: the
    /// peer's next announce (at most 5 s later) finds the verdict and
    /// follows the normal path in `discovery.rs`.
    ///
    /// It exists because of the transport task. The probe falls back, last, to the p2p
    /// tunnel, and the tunnel request is served by that same loop. Awaited
    /// inside it, the probe waited for itself until `ADMISSION_TIMEOUT` and
    /// always failed: a peer that only the relay reaches was admitted once
    /// while some direct route worked, and left the table (`Removed
    /// timed-out peers`) when the cache expired. The direct probes, 2 s per
    /// address, also stopped the whole endpoint while they waited.
    pub(crate) fn admit_in_background(self: &Arc<Self>, peer: PeerInfo) {
        match self.pending_admissions.lock() {
            Ok(mut pending) => {
                if !pending.insert(peer.node_id.clone()) {
                    return;
                }
            }
            Err(_) => return,
        }
        let network = Arc::clone(self);
        tokio::spawn(async move {
            let allowed = network.admit_peer(&peer).await;
            if let Ok(mut pending) = network.pending_admissions.lock() {
                pending.remove(&peer.node_id);
            }
            debug!(
                "admission of {} completed outside the endpoint: {}",
                short(&peer.node_id),
                if allowed { "admitted" } else { "refused" }
            );
        });
    }

    /// The public network has an open discovery door, but that is not a
    /// grant to take part in the endpoint. We store only candidates that
    /// can prove the link between identity, authority key and
    /// EndpointId. The trust chain is still required by
    /// `admit_peer` before any member effect.
    fn is_public_discovery_candidate(peer: &PeerInfo) -> bool {
        peer.gateway_port.is_some()
            && peer
                .cluster_id
                .as_deref()
                .is_some_and(|cluster| !cluster.trim().is_empty())
            && Self::node_id_matches_pubkey(&peer.node_id, &peer.pubkey)
            && Self::peer_id_matches_pubkey(peer)
    }

    /// Keep a public announcement available to the public-network directory
    /// without adding it to the admitted peer table.
    pub(crate) async fn remember_public_discovery_peer(&self, peer: &PeerInfo) {
        if peer.node_id == self.node_id || !Self::is_public_discovery_candidate(peer) {
            return;
        }
        self.public_discovery
            .write()
            .await
            .insert(peer.node_id.clone(), peer.clone());
    }

    /// Candidates that are still fresh enough to be probed by the public
    /// directory. They are intentionally not returned by `alive_peers`.
    pub(crate) async fn public_discovered_peers(&self) -> Vec<PeerInfo> {
        self.public_discovery
            .read()
            .await
            .values()
            .filter(|peer| peer.is_alive())
            .cloned()
            .collect()
    }

    pub(crate) async fn insert_admitted_peer(&self, mut peer: PeerInfo) {
        if peer.dialable_addr().is_none() {
            debug!(
                "ignoring peer {} without a dialable UDP address",
                short(&peer.node_id)
            );
            return;
        }
        let node_id = peer.node_id.clone();
        if node_id == self.node_id {
            return;
        }
        if self.admission.is_some() {
            peer.connection_verified = true;
        }
        let (is_new, address_changed) = {
            let peers = self.peers.read().await;
            match peers.get(&node_id) {
                Some(previous) => (false, peer_address_changed(previous, &peer)),
                None => (true, false),
            }
        };
        if is_new {
            info!(
                "🆕 Peer via the endpoint: {} model={} layers={}-{}",
                short(&node_id),
                peer.model,
                peer.start_layer,
                peer.end_layer
            );
        }
        if address_changed {
            self.routes.invalidate(&node_id);
        }
        self.peers.write().await.insert(node_id, peer);
    }

    /// Insert or update a peer discovered via p2p (gossip / mDNS).
    pub async fn insert_peer(&self, peer: PeerInfo) {
        self.remember_public_discovery_peer(&peer).await;
        if !self.admit_peer(&peer).await {
            debug!(
                "peer {} refused by the admission gate",
                short(&peer.node_id)
            );
            return;
        }
        self.insert_admitted_peer(peer).await;
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

    /// A short id on the network cannot bring down the `receive_loop`.
    ///
    /// Reproduces what took the captain's cluster down on 2026-08-16: a
    /// `PeerList` arrived with a two-character `node_id`, `&id[..12]` panicked,
    /// the `receive_loop` task died and the node stopped receiving UDP
    /// while still answering HTTP - indistinguishable from healthy on the screen.
    #[test]
    fn a_short_node_id_from_the_wire_never_panics() {
        assert_eq!(short("eu"), "eu");
        assert_eq!(short(""), "");
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
        // Does not cut in the middle of a multibyte character.
        assert_eq!(short("ááááááá"), "áááááá");
    }

    #[test]
    fn repeated_handler_panics_are_rate_limited_in_the_log() {
        let first = Instant::now();
        let later = first + PANIC_LOG_INTERVAL;
        let mut limiter = PanicLogLimiter::default();

        assert_eq!(limiter.record(first), Some(0));
        assert_eq!(limiter.record(first), None);
        assert_eq!(limiter.record(first), None);
        assert_eq!(limiter.record(later), Some(2));
    }

    /// Gossip and UDP have to publish the same announce, even when the
    /// node does not have a model yet. The p2p publish once had a builder of its own,
    /// which omitted `relay/v1-chain` and `speculate/v1-lookup`, sent empty `role` and
    /// `hostname` and **only** published with an assignment: a node without a model
    /// was invisible over the internet while the LAN saw it normally.
    #[tokio::test]
    async fn gossipsub_and_udp_publish_the_same_announce() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, Some(8181), Some(8080))
            .await
            .unwrap()
            .with_identity(crate::profile::NodeProfile::Admin, "macbook".into())
            .with_quic_ports(Some(8182), Some(8081));
        net.set_advertise_addr(Some("198.51.100.9:8080"));
        // The operator's hint goes first; this machine's interface addresses
        // follow behind it.
        assert_eq!(
            net.inference_quic_addr_candidates()
                .first()
                .map(String::as_str),
            Some("198.51.100.9:8182")
        );
        assert_eq!(
            net.gateway_quic_addr_candidates()
                .first()
                .map(String::as_str),
            Some("198.51.100.9:8081")
        );

        let asgn = LayerAssignment {
            deploy_id: String::new(),
            model: "qwen35".into(),
            total_layers: 32,
            start_layer: 0,
            end_layer: 15,
            layers_count: 16,
            memory_used_mb: 4096.0,
            memory_allocated_mb: 4096.0,
        };

        for assignment in [None, Some(&asgn)] {
            let gossip: serde_json::Value = serde_json::from_slice(
                &net.gossip_announce_bytes(assignment)
                    .expect("gossipsub always publishes, with or without an assignment"),
            )
            .unwrap();
            let udp: serde_json::Value =
                serde_json::to_value(net.self_announce(assignment)).unwrap();
            // `timestamp` is the only field that changes between the two constructions.
            let mut gossip = gossip;
            let mut udp = udp;
            gossip["timestamp"] = serde_json::Value::Null;
            udp["timestamp"] = serde_json::Value::Null;
            assert_eq!(gossip, udp);

            let caps = gossip["inference_frame_capabilities"].as_array().unwrap();
            for cap in [
                crate::pipeline::CAP_FRAME_BATCH,
                crate::pipeline::CAP_REMOTE_TOKEN_STREAM,
                "relay/v1-chain",
                "relay/v2-direct",
                "speculate/v1-lookup",
            ] {
                assert!(caps.iter().any(|c| c == cap), "missing {cap} in {caps:?}");
            }
            assert_eq!(gossip["profile"], "admin");
            // Both announces (UDP and gossip) have to carry the flag: if only
            // one carried it, the switch would arrive through one path and not the other,
            // and the cluster would end up half encrypted without anyone noticing.
            assert_eq!(gossip["tls"], false);
            assert_eq!(gossip["inference_quic_port"], 8182);
            assert_eq!(gossip["gateway_quic_port"], 8081);
            assert_eq!(gossip["hostname"], "macbook");
        }

        // And the flag turned on shows up on both paths, not just one.
        net.set_tls(true);
        let udp: serde_json::Value = serde_json::to_value(net.self_announce(None)).unwrap();
        let gossip: serde_json::Value =
            serde_json::from_slice(&net.gossip_announce_bytes(None).unwrap()).unwrap();
        assert_eq!(udp["tls"], true);
        assert_eq!(gossip["tls"], true);

        // The idle state also has to be identical on both
        // transports: a peer cannot look `serving` over the LAN and `idle`
        // over gossip.
        net.set_idle(true);
        let udp: serde_json::Value = serde_json::to_value(net.self_announce(None)).unwrap();
        let gossip: serde_json::Value =
            serde_json::from_slice(&net.gossip_announce_bytes(None).unwrap()).unwrap();
        assert_eq!(udp["idle"], true);
        assert_eq!(gossip["idle"], true);
    }

    #[tokio::test]
    async fn testkit_broadcasts_only_to_an_injected_loopback_endpoint() {
        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = probe.local_addr().unwrap();
        let net = Network::testkit_with_broadcast_addr(
            "n".into(),
            "p".into(),
            vec![],
            None,
            None,
            None,
            destination,
        )
        .await
        .unwrap();

        assert!(net.bind_addr.ip().is_loopback());
        assert!(net.broadcast_addr.ip().is_loopback());
        assert_ne!(
            net.broadcast_addr,
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::BROADCAST), DISCOVERY_PORT)
        );

        net.announce(None).await.unwrap();
        assert!(recv_msg(&probe, 500).await.is_some());
    }

    #[tokio::test]
    async fn testkit_default_is_isolated_from_the_global_broadcast() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap();

        assert!(net.bind_addr.ip().is_loopback());
        assert!(net.broadcast_addr.ip().is_loopback());
        assert_eq!(net.broadcast_addr, net.bind_addr);
        assert_ne!(
            net.broadcast_addr,
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::BROADCAST), DISCOVERY_PORT)
        );
    }

    #[tokio::test]
    async fn secondary_authority_is_not_mistaken_for_the_primary() {
        let net = Network::testkit("local".into(), "pk".into(), vec![], None, None, None)
            .await
            .expect("UDP socket on an ephemeral port");
        let (probe, mut received) = tokio::sync::mpsc::unbounded_channel();
        net.set_policy_principal_pubkey("primary-key");
        net.set_cluster_logs_probe(probe);

        net.trigger_cluster_logs_probe("secondary", "secondary-key", true, false);
        assert!(received.try_recv().is_err());
        net.trigger_cluster_logs_probe("primary", "primary-key", true, false);
        assert_eq!(received.recv().await.unwrap().0, "primary");
    }

    #[tokio::test]
    async fn admin_peers_returns_all_live_admin_destinations() {
        let net = Network::testkit("local".into(), "pk".into(), vec![], None, None, None)
            .await
            .expect("UDP socket on an ephemeral port");
        let mut first = dummy_peer("admin-a", "127.0.0.1:8081");
        first.pubkey = "authority-a".into();
        first.profile = crate::profile::NodeProfile::Admin;
        first.gateway_port = Some(8081);
        first.endpoint_id = Some(crate::p2p::random_endpoint_id().to_string());
        let mut second = dummy_peer("admin-b", "127.0.0.1:8082");
        second.pubkey = "authority-b".into();
        second.profile = crate::profile::NodeProfile::Admin;
        second.gateway_port = Some(8082);
        second.endpoint_id = Some(crate::p2p::random_endpoint_id().to_string());
        let ordinary = dummy_peer("node-c", "127.0.0.1:8083");
        {
            let mut peers = net.peers.write().await;
            peers.insert(first.node_id.clone(), first);
            peers.insert(second.node_id.clone(), second);
            peers.insert(ordinary.node_id.clone(), ordinary);
        }

        let peers = net.admin_peers().await;
        assert_eq!(peers.len(), 2);
    }

    #[tokio::test]
    async fn testkit_rejects_a_global_broadcast_endpoint() {
        let result = Network::testkit_with_broadcast_addr(
            "n".into(),
            "p".into(),
            vec![],
            None,
            None,
            None,
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::BROADCAST), DISCOVERY_PORT),
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn production_network_keeps_the_global_broadcast_default() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap();

        assert_eq!(
            net.broadcast_addr,
            SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::BROADCAST), DISCOVERY_PORT)
        );
    }

    #[tokio::test]
    async fn announce_carries_the_capacity_confirmed_by_inference_health() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, Some(8181), Some(8080))
            .await
            .unwrap();
        net.set_runtime_capacity(Some(RuntimeCapacity {
            n_ctx: 100_000,
            max_new_tokens: 32_000,
        }));

        let wire: serde_json::Value = serde_json::to_value(net.self_announce(None)).unwrap();
        assert_eq!(
            wire["runtime_capacity"],
            serde_json::json!({"n_ctx": 100_000, "max_new_tokens": 32_000})
        );
    }

    #[test]
    fn removed_capacity_field_is_rejected_from_peer_wire_contract() {
        let legacy_field = ["max", "output", "tokens"].join("_");
        let mut value = serde_json::json!({"n_ctx": 100_000, "max_new_tokens": 32_000});
        value
            .as_object_mut()
            .unwrap()
            .insert(legacy_field.clone(), serde_json::json!(8_192));

        let result: Result<RuntimeCapacity, _> = serde_json::from_value(value);
        let error = result.expect_err("a removed field cannot be ignored on the wire");
        assert!(error.to_string().contains(&legacy_field));
    }

    #[tokio::test]
    async fn an_old_runtime_generation_cannot_restore_its_capacity() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, Some(8181), Some(8080))
            .await
            .unwrap();
        net.set_runtime_generation(2);

        net.set_runtime_capacity_for_generation(
            1,
            Some(RuntimeCapacity {
                n_ctx: 8_192,
                max_new_tokens: 8_192,
            }),
        );
        assert_eq!(net.runtime_capacity(), None);

        net.set_runtime_capacity_for_generation(
            2,
            Some(RuntimeCapacity {
                n_ctx: 100_000,
                max_new_tokens: 32_000,
            }),
        );
        net.set_runtime_capacity_for_generation(1, None);
        assert_eq!(
            net.runtime_capacity(),
            Some(RuntimeCapacity {
                n_ctx: 100_000,
                max_new_tokens: 32_000,
            })
        );
    }

    #[tokio::test]
    async fn an_ephemeral_p2p_socket_never_announces_port_zero() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap();

        let port = net.bind_addr.port();
        assert_ne!(port, 0, "a port-0 bind must retain the assigned UDP port");
        assert_ne!(net.announce_addr().rsplit(':').next(), Some("0"));
        assert!(net.self_addr_candidates().iter().all(|addr| addr
            .parse::<SocketAddr>()
            .unwrap()
            .port()
            != 0));
    }

    /// Sending an IPv4 destination on the dual-stack socket fails with EINVAL on
    /// macOS, so IPv4 destinations must be mapped. Regressing this kills peer
    /// discovery on one side only, which is very hard to spot.
    #[tokio::test]
    async fn ipv4_destinations_are_mapped_on_a_dual_stack_socket() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
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

    /// When the interface's IPv6 changes, the last address observed by
    /// identify cannot keep winning over the current public address.
    #[test]
    fn a_current_public_local_address_outranks_a_stale_observed_address() {
        let local = vec![
            crate::addrs::LocalAddr {
                ip: "2001:db8::2".into(),
                interface: None,
                deprecated: false,
                temporary: true,
            },
            crate::addrs::LocalAddr {
                ip: "2001:db8::1".into(),
                interface: None,
                deprecated: false,
                temporary: false,
            },
        ];
        let observed = Some("203.0.113.5:9999".to_string());

        assert_eq!(
            choose_announce_addr(None, observed.clone(), local.clone(), 42777),
            "[2001:db8::1]:42777"
        );
        assert_eq!(
            local_candidates_by_preference(observed.as_deref(), local),
            vec!["2001:db8::1", "2001:db8::2", "203.0.113.5"]
        );
    }

    /// The effective host always comes out with the IPv6 in brackets, wherever it
    /// comes from. The local interface's one came raw, and the node's own entry became
    /// `2804:...:5eb8:42777` on the participants screen and `2804` for whoever
    /// passes the host through `ip_host`.
    #[test]
    fn the_effective_host_brackets_ipv6_from_every_source() {
        let local = |ip: &str| {
            vec![crate::addrs::LocalAddr {
                ip: ip.into(),
                interface: None,
                deprecated: false,
                temporary: false,
            }]
        };
        let from_interface = choose_effective_host(None, local("2001:db8::1"), None);
        assert_eq!(from_interface.as_deref(), Some("[2001:db8::1]"));
        assert_eq!(
            ip_host(&format!("{}:42777", from_interface.unwrap())),
            "[2001:db8::1]"
        );

        assert_eq!(
            choose_effective_host(None, vec![], Some("[2001:db8::9]:9999".into())).as_deref(),
            Some("[2001:db8::9]")
        );
        assert_eq!(
            choose_effective_host(Some("[2001:db8::7]".into()), local("2001:db8::1"), None)
                .as_deref(),
            Some("[2001:db8::7]")
        );
        assert_eq!(
            choose_effective_host(None, local("203.0.113.5"), None).as_deref(),
            Some("203.0.113.5")
        );
    }

    /// The change of local address between two announces must show up in the next
    /// announce without restarting the process. The test uses two interface
    /// snapshots so as not to depend on this machine's IPv6 state.
    #[test]
    fn a_new_local_address_is_used_by_the_next_announce() {
        let first = announce_addresses(
            None,
            None,
            vec![crate::addrs::LocalAddr {
                ip: "2001:db8::1".into(),
                interface: None,
                deprecated: false,
                temporary: true,
            }],
            42777,
        );
        let second = announce_addresses(
            None,
            None,
            vec![crate::addrs::LocalAddr {
                ip: "2001:db8::2".into(),
                interface: None,
                deprecated: false,
                temporary: true,
            }],
            42777,
        );

        assert_ne!(first.0, second.0);
        assert_eq!(second.0, "[2001:db8::2]:42777");
        assert!(second.1.contains(&"[2001:db8::2]:42777".to_string()));
    }

    /// A NAT'd node with nothing pinned has, until now, only its own private
    /// LAN interfaces to offer — useless to a remote peer over gossip,
    /// where there is no packet source to rewrite `addr` with the way the
    /// UDP path does. A peer-observed public address has to fill that gap.
    #[test]
    fn an_observed_public_address_becomes_the_announced_one() {
        let port = 42777;
        assert_eq!(
            choose_announce_addr(None, None, vec![], port),
            format!("0.0.0.0:{port}"),
            "no pin, nothing observed yet"
        );
        assert!(
            local_candidates_by_preference(Some("203.0.113.5:9999"), vec![])
                .contains(&"203.0.113.5".to_string()),
            "the observed address's host must appear as a candidate too, at this node's own port"
        );
        assert_eq!(
            choose_announce_addr(None, Some("203.0.113.5:9999".into()), vec![], port),
            "203.0.113.5:9999"
        );
    }

    #[test]
    fn an_observed_public_address_outranks_private_local_interfaces() {
        let local = vec![crate::addrs::LocalAddr {
            ip: "192.168.1.50".into(),
            interface: None,
            deprecated: false,
            temporary: false,
        }];
        let observed = Some("203.0.113.5:9999".to_string());

        assert_eq!(
            choose_announce_addr(None, observed.clone(), local.clone(), 42777),
            "203.0.113.5:9999"
        );
        assert_eq!(
            local_candidates_by_preference(observed.as_deref(), local),
            vec!["203.0.113.5", "192.168.1.50"]
        );
    }

    /// The operator's own explicit pin is a stronger signal than anything
    /// this node inferred about itself — an observed address arriving later
    /// (e.g. a new peer connects) must never override it.
    #[tokio::test]
    async fn a_pinned_address_always_outranks_an_observed_one() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap()
            .with_advertise_addr(Some("198.51.100.9:42777".into()));

        net.set_observed_addr("203.0.113.5:9999");

        assert_eq!(net.announce_addr(), "198.51.100.9:42777");
    }

    /// The pin goes first, but the rest is still announced: a dynamic IP that
    /// changed must not isolate the node.
    #[test]
    fn a_pinned_address_leads_but_does_not_replace_candidates() {
        let local = vec![
            crate::addrs::LocalAddr {
                ip: "192.168.100.210".into(),
                interface: Some("en0".into()),
                deprecated: false,
                temporary: false,
            },
            crate::addrs::LocalAddr {
                ip: "172.17.0.1".into(),
                interface: Some("docker0".into()),
                deprecated: false,
                temporary: false,
            },
        ];

        let (addr, candidates) =
            announce_addresses(Some("100.122.132.97:42777".into()), None, local, 42777);

        assert_eq!(addr, "100.122.132.97:42777");
        assert_eq!(
            candidates,
            vec!["100.122.132.97:42777", "192.168.100.210:42777"],
            "docker0 stays filtered; the pin does not hide the LAN"
        );
    }

    /// What the peer published comes first and whole (a relay URL is usually
    /// the only way to a peer behind NAT), then its UDP candidates on the
    /// endpoint port, which is how a LAN or VPN peer is reached.
    #[test]
    fn the_announced_endpoint_addrs_come_first_then_the_candidates() {
        let mut peer = peer_with_candidates(vec!["192.168.100.210:42777"]);
        peer.endpoint_addrs = vec![
            "203.0.113.7:52777".to_string(),
            "https://relay.example.org/".to_string(),
        ];

        let dialable = peer.endpoint_dialable_addrs();
        assert_eq!(dialable[..2], peer.endpoint_addrs[..]);
        assert!(dialable.contains(&"192.168.100.210:52777".to_string()));
    }

    /// A peer on the same LAN, or a NAT that rewrote to another private
    /// range, is not evidence of a real public IP — must not overwrite the
    /// sentinel with something equally useless.
    #[tokio::test]
    async fn a_private_observed_address_is_ignored() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap();

        net.set_observed_addr("192.168.1.50:9999");

        assert_eq!(net.observed_addr(), None);
    }

    /// `set_advertise_addr` is the live-update path the web UI's
    /// `POST /api/ui/network/advertise-addr` drives — no restart, unlike
    /// every other way this field could be set before. A later call has to
    /// actually override an earlier pin, since that's the entire point.
    #[tokio::test]
    async fn set_advertise_addr_overrides_a_previous_pin_live() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap()
            .with_advertise_addr(Some("198.51.100.9:42777".into()));

        assert_eq!(net.announce_addr(), "198.51.100.9:42777");

        net.set_advertise_addr(Some("203.0.113.77:42777"));

        assert_eq!(net.announce_addr(), "203.0.113.77:42777");
    }

    /// The operator's hint enters the endpoint addresses on the spot, on the
    /// endpoint port, ahead of what the endpoint publishes. A DNS name stays
    /// out: iroh dials addresses.
    #[tokio::test]
    async fn the_advertise_hint_leads_the_endpoint_addrs_live() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap();
        net.set_endpoint_addrs(vec!["https://relay.example.org/".into()]);

        net.set_advertise_addr(Some("198.51.100.9:42777"));
        assert_eq!(
            net.endpoint_addrs(),
            vec![
                "198.51.100.9:52777".to_string(),
                "https://relay.example.org/".to_string(),
            ]
        );

        net.set_advertise_addr(Some("node.example:42777"));
        assert_eq!(
            net.endpoint_addrs(),
            vec!["https://relay.example.org/".to_string()]
        );
        net.set_advertise_addr(None);
        assert_eq!(
            net.endpoint_addrs(),
            vec!["https://relay.example.org/".to_string()]
        );
    }

    /// Clearing the pin (`None`, from the UI's "empty means auto" convention)
    /// has to fall back through the same precedence `announce_addr` applies
    /// back to an observed address if no current public local address exists.
    #[tokio::test]
    async fn clearing_the_pin_falls_back_to_an_observed_address() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap()
            .with_advertise_addr(Some("198.51.100.9:42777".into()));
        net.set_observed_addr("203.0.113.5:9999");

        net.set_advertise_addr(None);

        assert_eq!(net.advertised_host(), None);
        assert_eq!(
            choose_announce_addr(None, net.observed_addr(), vec![], net.bind_addr.port()),
            "203.0.113.5:9999"
        );
    }

    /// A malformed value passed to the live setter must not silently corrupt
    /// the previously-pinned address — `set_advertise_addr` normalizes the
    /// same way construction does, and an unparseable input normalizes to
    /// `None`, which (per the test above) means "cleared," not "ignored."
    /// The web UI's save endpoint is responsible for rejecting garbage
    /// *before* calling this — this test documents what happens if it didn't.
    #[tokio::test]
    async fn an_unparseable_value_clears_rather_than_corrupts() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap();
        net.set_advertise_addr(Some(""));

        assert_eq!(net.advertised_host(), None);
    }

    /// The captain types the address by hand and hands it to a friend, so every
    /// shape he might reasonably type has to land on the same `host:port`.
    #[test]
    fn a_hand_typed_address_becomes_host_port() {
        let stable = "2804:d55:5579:1000:89f3:3f94:7c07:e183";
        assert_eq!(
            advertised_addr(stable, 42777).as_deref(),
            Some("[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777")
        );
        assert_eq!(
            advertised_addr(&format!("[{stable}]"), 42777).as_deref(),
            Some("[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777")
        );
        assert_eq!(
            advertised_addr(&format!("[{stable}]:42999"), 42777).as_deref(),
            Some("[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42999")
        );
        assert_eq!(
            advertised_addr("192.168.100.138", 42777).as_deref(),
            Some("192.168.100.138:42777")
        );
        assert_eq!(
            advertised_addr("192.168.100.138:1234", 42777).as_deref(),
            Some("192.168.100.138:1234")
        );
        assert_eq!(advertised_addr("192.168.100.138:0", 42777), None);
        assert_eq!(advertised_addr("peer.example:0", 42777), None);
        assert_eq!(advertised_addr("  ", 42777), None);
    }

    /// The address only helps if it survives the trip into a URL.
    #[test]
    fn an_advertised_ipv6_survives_into_the_peer_urls() {
        let peer = PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: "n".into(),
            pubkey: "p".into(),
            cluster_id: None,
            trust_version: None,
            relay: None,
            addr: "[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777".into(),
            addr_candidates: Vec::new(),
            model: "qwen35".into(),
            start_layer: 4,
            end_layer: 31,
            total_layers: 32,
            memory_mb: 0.0,
            last_seen: Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            inference_quic_port: Some(8182),
            gateway_port: Some(8090),
            gateway_quic_port: None,
            inference_frame_capabilities: Vec::new(),
            profile: crate::profile::NodeProfile::Node,
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            endpoint_id: None,
            endpoint_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
        };
        assert_eq!(
            peer.inference_url().as_deref(),
            Some("http://[2804:d55:5579:1000:89f3:3f94:7c07:e183]:8181")
        );
        assert_eq!(
            peer.quic_addr_candidates(),
            vec!["[2804:d55:5579:1000:89f3:3f94:7c07:e183]:8182"]
        );
        assert_eq!(
            peer.gateway_url().as_deref(),
            Some("http://[2804:d55:5579:1000:89f3:3f94:7c07:e183]:8090")
        );
    }

    /// The 5G path to the captain's box has an MTU of 1450 and IPv6 does not
    /// fragment in transit, so no reply may exceed the budget.
    #[test]
    fn peer_lists_are_split_to_fit_the_path_mtu() {
        let peer = PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: "7b09e99c538b0f79daf0cf9111d9e5e912094c7c999273b8d2df3a446f6630ec".into(),
            pubkey: "a".repeat(64),
            cluster_id: None,
            trust_version: None,
            relay: None,
            addr: "[2804:d55:5579:1000:89f3:3f94:7c07:e183]:42777".into(),
            addr_candidates: Vec::new(),
            model: "qwen35".into(),
            start_layer: 4,
            end_layer: 31,
            total_layers: 32,
            memory_mb: 6780.0,
            last_seen: Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            inference_quic_port: None,
            gateway_port: Some(8090),
            gateway_quic_port: None,
            inference_frame_capabilities: Vec::new(),
            profile: crate::profile::NodeProfile::Admin,
            hostname: "ksia".into(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 500000,
            disk_free_mb: 200000,
            api_keys_active: 0,
            endpoint_id: None,
            endpoint_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
        };

        for count in [0usize, 1, 2, 3, 5, 12] {
            let chunks = peer_list_chunks(vec![peer.clone(); count]);
            let sent: usize = chunks.iter().map(|c| c.len()).sum();
            assert_eq!(sent, count, "no peer can disappear in the slicing");
            for chunk in &chunks {
                let bytes = serde_json::to_vec(&P2PMessage::PeerList {
                    peers: chunk.clone(),
                })
                .unwrap()
                .len();
                assert!(
                    bytes <= MAX_DATAGRAM_PAYLOAD || chunk.len() == 1,
                    "datagram of {bytes} bytes with {} peers exceeds the budget",
                    chunk.len()
                );
            }
        }
    }

    /// A transitive list is also an update, not just discovery. If the old
    /// record stays stuck, two gateways that learned of the same peer
    /// at different times compute different holes for the same slice.
    #[tokio::test]
    async fn a_peer_list_refreshes_existing_peer_coverage() {
        let net = Network::new("local".into(), "pubkey".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let assignment = Arc::new(RwLock::new(None));

        let mut old = peer_with_candidates(vec![]);
        old.node_id = "peer".into();
        old.end_layer = 15;
        old.total_layers = 44;
        let old_seen = old.last_seen;
        net.insert_peer(old).await;

        let mut refreshed = peer_with_candidates(vec![]);
        refreshed.node_id = "peer".into();
        refreshed.end_layer = 28;
        refreshed.total_layers = 44;
        refreshed.last_seen = old_seen + chrono::Duration::seconds(1);
        net.handle_message(
            P2PMessage::PeerList {
                peers: vec![refreshed],
            },
            "127.0.0.1:42777".parse().unwrap(),
            &assignment,
        )
        .await;

        let peers = net.peers.read().await;
        let peer = peers.get("peer").expect("the old peer stays known");
        assert_eq!(peer.end_layer, 28);
        assert_eq!(peer.total_layers, 44);
    }

    fn peer_with_candidates(candidates: Vec<&str>) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: "n1".into(),
            pubkey: "p".into(),
            cluster_id: None,
            trust_version: None,
            relay: None,
            addr: "192.168.1.9:42777".into(),
            addr_candidates: candidates.into_iter().map(String::from).collect(),
            model: "qwen35".into(),
            start_layer: 0,
            end_layer: 19,
            total_layers: 40,
            memory_mb: 1.0,
            last_seen: Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            inference_quic_port: None,
            inference_frame_capabilities: Vec::new(),
            gateway_port: Some(8080),
            gateway_quic_port: None,
            profile: crate::profile::NodeProfile::Node,
            hostname: "h".into(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            endpoint_id: None,
            endpoint_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
        }
    }

    /// The `PeerList` of another node brings the list the peer announces today. Inheriting the
    /// whole local list on top of it made a dead candidate **immortal**: the
    /// address the peer stopped announcing came back on every gossip round, and
    /// the set only grew. With temporary IPv6, which rotates on its own, that is a
    /// new corpse per rotation - and that is how the captain's cluster ended up
    /// dialing an address that existed on no machine.
    #[tokio::test]
    async fn gossip_does_not_resurrect_an_address_already_proved_dead() {
        let net = Network::new("local".into(), "pubkey".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let assignment = Arc::new(RwLock::new(None));
        let dead = "[2804:d55:5579:1000:1c20:e551:6a95:9a0d]:42777";
        let alive = "[2804:d55:5579:1000:18cc:e839:7c2:dd6a]:42777";

        let mut old = peer_with_candidates(vec![dead]);
        old.node_id = "peer".into();
        old.addr = dead.into();
        let seen_at = old.last_seen;
        net.insert_peer(old).await;
        // A real request already proved that this address does not answer.
        net.routes.mark_failure(
            "peer",
            "http://[2804:d55:5579:1000:1c20:e551:6a95:9a0d]:8080",
        );

        let mut fresco = peer_with_candidates(vec![alive]);
        fresco.node_id = "peer".into();
        fresco.addr = alive.into();
        fresco.last_seen = seen_at + chrono::Duration::seconds(1);
        net.handle_message(
            P2PMessage::PeerList {
                peers: vec![fresco],
            },
            "127.0.0.1:42777".parse().unwrap(),
            &assignment,
        )
        .await;

        let stored = net.peers.read().await["peer"].clone();
        assert!(
            !stored.addr_candidates.iter().any(|c| c == dead),
            "the dead address came back through gossip: {:?}",
            stored.addr_candidates
        );
        assert!(
            stored.addr_candidates.iter().any(|c| c == alive),
            "the current address must stay: {:?}",
            stored.addr_candidates
        );
    }

    /// An explicit list announced by the peer replaces the previous snapshot;
    /// the observed address is not inherited, because that would reintroduce LAN, bridge
    /// or an old address that the operator already stopped announcing.
    #[tokio::test]
    async fn gossip_drops_an_observed_candidate_when_the_peer_announces_a_set() {
        let net = Network::new("local".into(), "pubkey".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let assignment = Arc::new(RwLock::new(None));
        let observed = "203.0.113.9:42777";
        let announced = "198.51.100.7:42777";

        let mut old = peer_with_candidates(vec![observed]);
        old.node_id = "peer".into();
        old.addr = observed.into();
        let seen_at = old.last_seen;
        net.insert_peer(old).await;

        let mut fresco = peer_with_candidates(vec![announced]);
        fresco.node_id = "peer".into();
        fresco.addr = announced.into();
        fresco.last_seen = seen_at + chrono::Duration::seconds(1);
        net.handle_message(
            P2PMessage::PeerList {
                peers: vec![fresco],
            },
            "127.0.0.1:42777".parse().unwrap(),
            &assignment,
        )
        .await;

        let stored = net.peers.read().await["peer"].clone();
        assert_eq!(stored.addr_candidates, vec![announced]);
    }

    /// When the peer sends explicit candidates, the packet's effective source
    /// does not enter the list: it may be LAN, bridge or another address that is not
    /// the `advertise_addr` the peer wants to publish.
    #[tokio::test]
    async fn an_announce_does_not_add_the_socket_source_to_an_explicit_set() {
        let net = Network::new(
            "local".into(),
            "pubkey".into(),
            0,
            vec![],
            None,
            Some(8181),
            None,
        )
        .await
        .unwrap();
        let assignment = Arc::new(RwLock::new(None));
        let stale = "[2804:d55:5579:1000:e6c8:310f:1a19:6e92]:42777";
        let current = "[2804:d55:5579:1000:2d50:fe2c:c06c:fc07]:42777";

        net.handle_message(
            P2PMessage::Announce {
                node_id: "peer".into(),
                pubkey: "peer-key".into(),
                cluster_id: None,
                trust_version: None,
                relay: None,
                addr: stale.into(),
                addr_candidates: vec![stale.into()],
                model: "qwen".into(),
                deploy_id: String::new(),
                start_layer: 28,
                end_layer: 31,
                total_layers: 32,
                memory_mb: 0.0,
                timestamp: Utc::now(),
                ollama_port: None,
                inference_port: Some(8181),
                inference_quic_port: None,
                inference_frame_capabilities: Vec::new(),
                gateway_port: Some(8080),
                gateway_quic_port: None,
                profile: crate::profile::NodeProfile::Node,
                tls: false,
                quic: false,
                hostname: "salem".into(),
                capabilities: Vec::new(),
                disk_total_mb: 0,
                disk_free_mb: 0,
                endpoint_id: None,
                endpoint_addrs: Vec::new(),
                tp: None,
                runtime_capacity: None,
                idle: false,
                cluster_logs_enabled: false,
                network_chat_enabled: false,
            },
            current.parse().unwrap(),
            &assignment,
        )
        .await;

        let peers = net.peers.read().await;
        let peer = peers
            .get("peer")
            .expect("the announce must create the peer");
        assert_eq!(peer.addr_candidates, vec![stale]);
    }

    /// `gateway_url()` was just `peer.addr`, so the control plane had one
    /// address and no plan B. `inference_url()` already preferred candidates for
    /// a long time; the gateway never learned.
    #[test]
    fn the_gateway_has_one_url_per_known_address_with_the_proven_one_first() {
        let mut peer = peer_with_candidates(vec![
            "10.0.0.5:42777",
            "[2804:f30::1]:42777",
            "192.168.1.9:42777",
        ]);
        peer.addr = "192.168.1.9:42777".into();
        assert_eq!(
            peer.gateway_candidates(),
            vec![
                // `addr` first: it is where the peer's packets came from.
                "http://192.168.1.9:8080".to_string(),
                "http://10.0.0.5:8080".to_string(),
                "http://[2804:f30::1]:8080".to_string(),
            ],
            "the first is a known fact, and no URL comes out repeated"
        );
        assert_eq!(
            peer.gateway_url(),
            Some("http://192.168.1.9:8080".to_string())
        );
    }

    /// An IPv6 rotation changes `addr_candidates` and calls `invalidate`. Erasing
    /// the failure memory there would erase the proof exactly at the event that kills
    /// the address: the cluster would go back to dialing the corpse on the next round.
    #[test]
    fn an_address_rotation_does_not_erase_the_proof_that_the_old_one_died() {
        let table = crate::route::RouteTable::new();
        table.mark_failure("peer", "http://[2804:d55::9]:8080");
        table.invalidate("peer");
        assert!(
            table.is_failed("peer", "http://[2804:d55::9]:8080"),
            "the rotation cannot erase the proof that the old address died"
        );
        table.forget("peer");
        assert!(
            !table.is_failed("peer", "http://[2804:d55::9]:8080"),
            "when the peer is gone for good, its memory goes along"
        );
    }

    /// The packet's source address is appended, it does not replace: it is the only
    /// path proven to work, but it is not the only one that exists.
    /// Replacing was what made two nodes on the same LAN, who met over the
    /// internet, never discover that they were also neighbors.
    #[test]
    fn the_observed_address_joins_the_candidate_set() {
        let mut peer = peer_with_candidates(vec!["192.168.1.9:42777"]);
        peer.merge_observed("[2804:f30::1]:42777");
        assert_eq!(
            peer.addr_candidates,
            vec![
                "192.168.1.9:42777".to_string(),
                "[2804:f30::1]:42777".to_string()
            ],
        );
    }

    #[test]
    fn an_already_known_observed_address_is_not_duplicated() {
        let mut peer = peer_with_candidates(vec!["192.168.1.9:42777"]);
        peer.merge_observed("192.168.1.9:42777");
        assert_eq!(peer.addr_candidates.len(), 1);
    }

    /// A node running an earlier binary does not send `addr_candidates`. The field falls
    /// to the default and its single `addr` becomes the only candidate: the
    /// mixed-version cluster keeps working.
    #[test]
    fn a_peer_without_candidates_falls_back_to_its_single_addr() {
        let peer = peer_with_candidates(vec![]);
        assert_eq!(
            peer.inference_candidates(),
            vec!["http://192.168.1.9:8181".to_string()]
        );
    }

    #[test]
    fn wildcard_addresses_are_not_destinations() {
        for addr in [
            "0.0.0.0:42778",
            "[::]:42778",
            "0.0.0.0:0",
            "[::]:0",
            "224.0.0.1:42778",
            "255.255.255.255:42778",
            "[ff02::1]:42778",
        ] {
            let mut peer = peer_with_candidates(vec![]);
            peer.addr = addr.into();
            assert_eq!(peer.dialable_addr(), None, "{addr} must not be dialed");
            assert!(
                peer.inference_candidates().is_empty(),
                "{addr} must not become a URL"
            );
        }

        let peer = peer_with_candidates(vec!["0.0.0.0:42778", "[2804:d55::1]:42778"]);
        assert_eq!(peer.dialable_addr(), Some("[2804:d55::1]:42778"));
        assert_eq!(
            peer.inference_candidates(),
            vec![
                "http://[2804:d55::1]:8181".to_string(),
                "http://192.168.1.9:8181".to_string(),
            ]
        );
    }

    #[test]
    fn a_peer_with_only_a_zero_port_is_not_dialable() {
        let mut peer = peer_with_candidates(vec![]);
        peer.addr = "[2804:d55::1]:0".into();
        assert_eq!(peer.dialable_addr(), None);

        peer.addr_candidates.push("[2804:d55::2]:42777".into());
        assert_eq!(peer.dialable_addr(), Some("[2804:d55::2]:42777"));
    }

    /// `inference_url()` used to read only `addr` - a value that could be
    /// behind the best address already available in `addr_candidates` (the one pinned
    /// by the operator, or the public one observed by `identify`). The Dashboard and the
    /// `PipelineStep` fallback field use exactly this method, so
    /// a stale `addr` was enough to show/dial a private IP
    /// even with the right address already known.
    #[test]
    fn inference_url_prefers_the_first_candidate_over_the_legacy_addr() {
        let peer = peer_with_candidates(vec!["203.0.113.5:42777", "192.168.1.9:42777"]);
        assert_eq!(
            peer.addr, "192.168.1.9:42777",
            "addr is left outdated on purpose in this test"
        );
        assert_eq!(
            peer.inference_url().as_deref(),
            Some("http://203.0.113.5:8181")
        );
    }

    /// Each candidate becomes an inference URL, with the IPv6 in brackets.
    #[test]
    fn every_candidate_becomes_an_inference_url() {
        let peer = peer_with_candidates(vec!["192.168.1.9:42777", "[2804:f30::1]:42777"]);
        assert_eq!(
            peer.inference_candidates(),
            vec![
                "http://192.168.1.9:8181".to_string(),
                "http://[2804:f30::1]:8181".to_string(),
            ],
        );
    }

    /// An old node sends an Announce without the new field. If that stopped
    /// parsing, the cluster would split in half during a partial upgrade.
    #[test]
    fn an_announce_without_candidates_still_parses() {
        let old = r#"{"type":"Announce","node_id":"n","pubkey":"p","addr":"1.2.3.4:42777",
            "model":"m","start_layer":0,"end_layer":1,"total_layers":2,"memory_mb":1.0,
            "timestamp":"2026-08-13T00:00:00Z"}"#;
        let msg: P2PMessage = serde_json::from_str(old).expect("an old Announce must parse");
        match msg {
            P2PMessage::Announce {
                addr_candidates, ..
            } => assert!(addr_candidates.is_empty()),
            _ => panic!("expected Announce"),
        }
    }

    /// Same shape of regression as `an_announce_without_candidates_still_parses`,
    /// specifically for `endpoint_id`: a peer on a binary that predates the
    /// NAT-tunnel work sends `Announce` with no such field at all.
    #[test]
    fn an_announce_without_a_endpoint_id_still_parses() {
        let old = r#"{"type":"Announce","node_id":"n","pubkey":"p","addr":"1.2.3.4:42777",
            "model":"m","start_layer":0,"end_layer":1,"total_layers":2,"memory_mb":1.0,
            "timestamp":"2026-08-13T00:00:00Z"}"#;
        let msg: P2PMessage =
            serde_json::from_str(old).expect("an Announce without endpoint_id must parse");
        match msg {
            P2PMessage::Announce { endpoint_id, .. } => assert_eq!(endpoint_id, None),
            _ => panic!("expected Announce"),
        }
    }

    /// An `endpoint_id` present on the wire has to survive the round trip
    /// unchanged — this is the one field a peer actually needs to dial the
    /// NAT-tunnel fallback, so a lossy encode/decode would silently break it.
    #[test]
    fn an_announce_with_a_endpoint_id_round_trips() {
        let with_id = r#"{"type":"Announce","node_id":"n","pubkey":"p","addr":"1.2.3.4:42777",
            "model":"m","start_layer":0,"end_layer":1,"total_layers":2,"memory_mb":1.0,
            "timestamp":"2026-08-13T00:00:00Z",
            "endpoint_id":"12D3KooWAbCdEfGhIjKlMnOpQrStUvWxYz1234567890AbCdEfGh"}"#;
        let msg: P2PMessage = serde_json::from_str(with_id).expect("an Announce must parse");
        match &msg {
            P2PMessage::Announce { endpoint_id, .. } => assert_eq!(
                endpoint_id.as_deref(),
                Some("12D3KooWAbCdEfGhIjKlMnOpQrStUvWxYz1234567890AbCdEfGh")
            ),
            _ => panic!("expected Announce"),
        }
        let re_encoded = serde_json::to_string(&msg).unwrap();
        let re_decoded: P2PMessage = serde_json::from_str(&re_encoded).unwrap();
        match re_decoded {
            P2PMessage::Announce { endpoint_id, .. } => assert_eq!(
                endpoint_id.as_deref(),
                Some("12D3KooWAbCdEfGhIjKlMnOpQrStUvWxYz1234567890AbCdEfGh")
            ),
            _ => panic!("expected Announce"),
        }
    }

    /// `PeerInfo` (the type stored after an `Announce` is accepted, and the
    /// one `PipelineStep` reads `endpoint_id` from) needs the same
    /// backward-compatible default.
    #[test]
    fn a_peer_info_without_a_endpoint_id_still_parses() {
        let old = r#"{"node_id":"n","pubkey":"p","addr":"1.2.3.4:42777","model":"m",
            "start_layer":0,"end_layer":1,"total_layers":2,"memory_mb":1.0,
            "last_seen":"2026-08-13T00:00:00Z"}"#;
        let peer: PeerInfo =
            serde_json::from_str(old).expect("a PeerInfo without endpoint_id must parse");
        assert_eq!(peer.endpoint_id, None);
    }

    /// A detached node must not announce or query bootstrap peers. This keeps
    /// the `--no-discovery` contract explicit even when its socket remains
    /// bound for the later `rejoin`.
    #[tokio::test]
    async fn a_detached_node_neither_announces_nor_looks_for_bootstrap() {
        let net = Network::testkit(
            "n".into(),
            "p".into(),
            vec!["127.0.0.1:1".into()],
            None,
            None,
            None,
        )
        .await
        .unwrap();

        net.detach().await;
        assert!(net.is_detached());

        // Both would fail loudly against `127.0.0.1:1` if anything were sent:
        // announce walks the same bootstrap list, and the broadcast is inside it.
        net.announce(None).await.unwrap();
        net.bootstrap_discovery().await.unwrap();
        assert!(net.peers.read().await.is_empty());
    }

    /// A probe: brings up a local UDP socket pretending to be a peer, and returns the
    /// first datagram that arrives within the deadline.
    async fn probe() -> (Arc<UdpSocket>, String) {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap().to_string();
        (Arc::new(sock), addr)
    }

    async fn recv_msg(sock: &UdpSocket, ms: u64) -> Option<P2PMessage> {
        let mut buf = vec![0u8; 65535];
        match tokio::time::timeout(Duration::from_millis(ms), sock.recv_from(&mut buf)).await {
            Ok(Ok((len, _))) => serde_json::from_slice(&buf[..len]).ok(),
            _ => None,
        }
    }

    fn dummy_peer(node_id: &str, addr: &str) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: node_id.into(),
            pubkey: "p".into(),
            cluster_id: None,
            trust_version: None,
            relay: None,
            addr: addr.into(),
            addr_candidates: Vec::new(),
            model: String::new(),
            start_layer: 0,
            end_layer: 0,
            total_layers: 0,
            memory_mb: 0.0,
            last_seen: Utc::now(),
            ollama_port: None,
            inference_port: None,
            inference_quic_port: None,
            inference_frame_capabilities: Vec::new(),
            gateway_port: None,
            gateway_quic_port: None,
            profile: crate::profile::NodeProfile::Node,
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            endpoint_id: None,
            endpoint_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
        }
    }

    fn admission_config(
        key: &str,
        visibility: crate::config::NetworkVisibility,
    ) -> crate::auth::AuthState {
        use crate::config::Config;
        Arc::new(RwLock::new(Config {
            profile: crate::profile::NodeProfile::Node,
            setup_pending: false,
            local_auto_discovery: false,
            connection_key: key.into(),
            gateway_port: 8080,
            admin_exposure: Default::default(),
            admin_access_token: None,
            admin_password_default: false,
            p2p_port: 42777,
            inference_port: None,
            bootstrap_nodes: vec![],
            relay_peers: vec![],
            relay: Default::default(),
            advertise_addr: None,
            network: crate::config::NetworkSettings {
                visibility,
                ..Default::default()
            },
            api_keys: vec![],
            blocked_nodes: vec![],
            tls_required: false,
            quic_enabled: false,
            cluster_logs_enabled: false,
            network_chat_enabled: false,
            cluster_logs_min_level: Default::default(),
            model_dirs: None,
            shard_dir: None,
            workspace_root: None,
            shard_max_gb: crate::storage::DEFAULT_SHARD_MAX_GB,
            resources: Default::default(),
            setup_model: None,
            first_run_guide: None,
            gpus: None,
            thermal: Default::default(),
            cache_type_k: crate::config::KvCacheType::F16,
            cache_type_v: crate::config::KvCacheType::F16,
            node_name: None,
            huggingface_token: None,
            slot_save_path: None,
            slot_persist_prefix_cache: false,
            slot_auto_max_bytes: None,
            slot_auto_ttl_secs: None,
            cache_ram_mib: None,
        }))
    }

    /// Peer whose `node_id`, `pubkey` and `endpoint_id` come from the same
    /// key, as on every real node.
    fn admission_peer_for(gateway_port: u16, key: &ed25519_dalek::SigningKey) -> PeerInfo {
        let mut peer = admission_peer(gateway_port);
        let identity = crate::identity::Identity {
            signing_key: key.clone(),
            node_id: crate::identity::NodeId::from_pubkey(&key.verifying_key()),
        };
        peer.node_id = identity.node_id.0.clone();
        peer.pubkey = identity.public_key_hex();
        peer.endpoint_id = Some(identity.secret_key().public().to_string());
        peer
    }

    fn admission_peer(gateway_port: u16) -> PeerInfo {
        let identity = crate::identity::Identity::generate();
        let node_id = identity.node_id.0.clone();
        let pubkey = identity.public_key_hex();
        PeerInfo {
            node_id,
            pubkey,
            cluster_id: None,
            trust_version: None,
            relay: None,
            addr: "127.0.0.1:42777".into(),
            addr_candidates: vec!["127.0.0.1:42777".into()],
            model: String::new(),
            deploy_id: String::new(),
            start_layer: 0,
            end_layer: 0,
            total_layers: 0,
            memory_mb: 0.0,
            last_seen: Utc::now(),
            ollama_port: None,
            inference_port: None,
            inference_quic_port: None,
            inference_frame_capabilities: Vec::new(),
            gateway_port: Some(gateway_port),
            gateway_quic_port: None,
            profile: crate::profile::NodeProfile::Node,
            tls: false,
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            endpoint_id: None,
            endpoint_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
        }
    }

    #[tokio::test]
    async fn private_admission_rejects_wrong_key_and_accepts_the_shared_key() {
        let remote = admission_config("correct", crate::config::NetworkVisibility::Private);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_port = listener.local_addr().unwrap().port();
        let app = axum::Router::new()
            .route(
                "/handshake",
                axum::routing::post(crate::session::handle_handshake),
            )
            .with_state(remote);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let identity = crate::identity::Identity::generate();
        let wrong = Network::testkit(
            identity.node_id.0.clone(),
            identity.public_key_hex(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_admission(
            admission_config("wrong", crate::config::NetworkVisibility::Private),
            Arc::new(crate::admin_trust::AdminTrust::empty()),
        );
        wrong.insert_peer(admission_peer(gateway_port)).await;
        assert_eq!(wrong.peer_count().await, 0);

        let valid = Network::testkit(
            identity.node_id.0.clone(),
            identity.public_key_hex(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_admission(
            admission_config("correct", crate::config::NetworkVisibility::Private),
            Arc::new(crate::admin_trust::AdminTrust::empty()),
        );
        valid.insert_peer(admission_peer(gateway_port)).await;
        assert_eq!(valid.peer_count().await, 1);

        server.abort();
    }

    /// The address that already answered comes before the first announced one, even if
    /// it answered on the inference port; those that failed go to the end.
    #[tokio::test]
    async fn gateway_bases_put_the_proven_host_first() {
        let _policy = crate::peer_client::GLOBAL_POLICY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let network = Network::testkit("eu".into(), String::new(), vec![], None, None, None)
            .await
            .unwrap();
        let mut peer = admission_peer(8081);
        peer.addr = "[2804:d55::1]:42777".into();
        peer.addr_candidates = vec![
            "[2804:d55::1]:42777".into(),
            "10.0.0.9:42777".into(),
            "192.168.0.37:42777".into(),
        ];
        let host_of = |bases: Vec<String>| {
            bases
                .iter()
                .map(|base| url_host(base).to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            host_of(network.gateway_bases(&peer)),
            vec!["[2804:d55::1]", "10.0.0.9", "192.168.0.37"]
        );

        network
            .routes
            .mark_success(&peer.node_id, "http://192.168.0.37:8181");
        let scheme = crate::peer_client::peer_scheme();
        network
            .routes
            .mark_failure(&peer.node_id, &format!("{scheme}://[2804:d55::1]:8081"));
        assert_eq!(
            host_of(network.gateway_bases(&peer)),
            vec!["192.168.0.37", "10.0.0.9", "[2804:d55::1]"]
        );
    }

    #[test]
    fn url_host_keeps_ipv6_brackets_and_drops_port_and_path() {
        assert_eq!(url_host("http://[2804:d55::1]:8080"), "[2804:d55::1]");
        assert_eq!(url_host("https://192.168.0.37:8181/health"), "192.168.0.37");
        assert_eq!(url_host("http://node.example:8080"), "node.example");
    }

    #[test]
    fn a_link_names_the_far_end_or_the_relay() {
        let direct = P2pLink::new(
            crate::p2p::LinkPath::Direct("189.75.167.63:28515".parse().unwrap()),
            false,
        );
        assert_eq!(direct.address, "189.75.167.63:28515 (UDP)");
        assert!(!direct.relayed && direct.relay_url.is_none());

        let relayed = P2pLink::new(
            crate::p2p::LinkPath::Relay("http://82.25.69.187:52777/".into()),
            false,
        );
        assert_eq!(relayed.address, "82.25.69.187:52777");
        assert!(relayed.relayed);
        assert_eq!(
            relayed.relay_url.as_deref(),
            Some("http://82.25.69.187:52777/")
        );
    }

    /// Two nodes on a Docker bridge get the same internal IP and the same port:
    /// the candidate the other announces with that IP is this very process and
    /// cannot become a control, inference or QUIC destination.
    #[test]
    fn a_peer_candidate_that_is_this_node_is_never_a_destination() {
        let Some(local_ip) = local_interface_ips()
            .into_iter()
            .find(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
        else {
            return; // machine without an interface IPv4: nothing to prove here
        };
        let previous = SELF_P2P_PORT.swap(59_931, std::sync::atomic::Ordering::Relaxed);
        let mut peer = admission_peer(8081);
        peer.node_id = "outro-no".into();
        peer.addr = "203.0.113.9:59931".into();
        peer.addr_candidates = vec!["203.0.113.9:59931".into(), format!("{local_ip}:59931")];
        peer.inference_port = Some(8181);
        peer.inference_quic_port = Some(43778);

        let gateway = peer.gateway_candidates();
        let inference = peer.inference_candidates();
        let quic = peer.quic_addr_candidates();
        // The same local IP on another port is another process on this machine.
        let mut neighbour = peer.clone();
        neighbour.addr_candidates = vec![format!("{local_ip}:59932")];
        let neighbour_ok = neighbour.dialable_addrs();
        SELF_P2P_PORT.store(previous, std::sync::atomic::Ordering::Relaxed);

        assert!(
            gateway.iter().all(|url| !url.contains(&local_ip)),
            "{gateway:?}"
        );
        assert!(gateway.iter().any(|url| url.contains("203.0.113.9")));
        assert!(
            inference.iter().all(|url| !url.contains(&local_ip)),
            "{inference:?}"
        );
        assert!(
            quic.iter().all(|addr| !addr.contains(&local_ip)),
            "{quic:?}"
        );
        assert!(neighbour_ok.contains(&format!("{local_ip}:59932")));
    }

    /// The rule of file transfers: a direct path carries them, a relay only
    /// when it is a member relay that allows it, a public relay never -
    /// whether it is configured as one, one of n0's, or just not a member.
    #[tokio::test]
    async fn a_file_crosses_a_member_relay_but_never_a_public_one() {
        use crate::p2p::LinkPath;
        let network = Network::testkit("eu".into(), String::new(), vec![], None, None, None)
            .await
            .unwrap();
        let remote = crate::p2p::random_endpoint_id().to_string();
        let member_url = "http://203.0.113.9:52777/".to_string();
        let outside_url = "https://relay.example.org/".to_string();

        let mut relay_peer = admission_peer(8080);
        relay_peer.relay = Some(RelayOffer {
            transfers: true,
            url: Some(member_url.clone()),
        });
        network.insert_peer(relay_peer.clone()).await;

        // A relay that is not a member's is a public one.
        let outside = LinkPath::Relay(outside_url.clone());
        network.note_link(&remote, 1, P2pLink::new(outside.clone(), false));
        assert!(!network.path_carries_transfers(Some(&outside)).await);
        assert_eq!(network.transfer_links(&remote).await, (0, 1));

        // A member relay carries the file.
        let member = LinkPath::Relay(member_url.clone());
        network.note_link(&remote, 2, P2pLink::new(member.clone(), false));
        assert!(network.path_carries_transfers(Some(&member)).await);
        assert_eq!(network.transfer_links(&remote).await, (1, 2));

        // Unless it says it does not...
        relay_peer.relay = Some(RelayOffer {
            transfers: false,
            url: Some(member_url.clone()),
        });
        network.insert_peer(relay_peer.clone()).await;
        assert!(!network.path_carries_transfers(Some(&member)).await);

        // ...or it is one of the configured public relays, member or not.
        relay_peer.relay = Some(RelayOffer {
            transfers: true,
            url: Some(member_url.clone()),
        });
        network.insert_peer(relay_peer).await;
        network.set_dedicated_relays([member_url]);
        assert!(!network.path_carries_transfers(Some(&member)).await);

        // A direct path, dialed or hole punched, always does.
        let direct = LinkPath::Direct("189.75.167.63:28515".parse().unwrap());
        assert!(network.path_carries_transfers(Some(&direct)).await);
        // A connection with no path does not.
        assert!(!network.path_carries_transfers(None).await);
    }

    /// Whom a node relays for: everyone on a public relay; the members of
    /// its cluster on a node the internet reaches; nobody behind NAT.
    #[test]
    fn a_node_relays_for_its_cluster_only_when_the_internet_reaches_it() {
        let member = crate::p2p::random_endpoint_id().to_string();
        let stranger = crate::p2p::random_endpoint_id().to_string();
        let gate = RelayGate::new(false, true, true);
        gate.set_members([member.clone()]);
        assert!(!gate.serving());
        assert_eq!(gate.offer(), None);
        assert!(!gate.allows(&member), "behind NAT");

        gate.set_reachable(true);
        gate.set_url(Some("http://203.0.113.9:52777".into()));
        assert_eq!(
            gate.offer(),
            Some(RelayOffer {
                transfers: true,
                url: Some("http://203.0.113.9:52777".into())
            })
        );
        assert!(gate.allows(&member));
        assert!(!gate.allows(&stranger));

        // No relay server (its port was taken): nothing is offered.
        gate.set_available(false);
        assert_eq!(gate.offer(), None);
        assert!(!gate.allows(&member));
        gate.set_available(true);

        let off = RelayGate::new(false, false, true);
        off.set_reachable(true);
        off.set_members([member.clone()]);
        assert!(!off.allows(&member), "cluster relay turned off");

        let public = RelayGate::new(true, true, false);
        assert!(public.allows(&stranger));
        assert_eq!(
            public.offer(),
            Some(RelayOffer {
                transfers: false,
                url: None
            })
        );

        // The screen changes the options live; what the command line fixed
        // stays whatever is saved.
        gate.set(false, false, true);
        assert!(!gate.allows(&member));
        let fixed = RelayGate::new(false, true, true).with_overrides(RelayOverrides {
            everyone: true,
            no_cluster: false,
            no_transfers: true,
        });
        fixed.set(false, true, true);
        let status = fixed.status();
        assert!(status.everyone && !status.transfers && status.fixed.everyone);
    }

    /// The route shown is the observed one: HTTP that answered, then a direct
    /// endpoint path, then a relay - never the announced address.
    #[tokio::test]
    async fn peer_route_prefers_http_then_direct_then_relay() {
        use crate::p2p::LinkPath;
        let network = Network::testkit("eu".into(), String::new(), vec![], None, None, None)
            .await
            .unwrap();
        let remote = crate::p2p::random_endpoint_id().to_string();
        let mut peer = admission_peer(8080);
        peer.addr = "[2804:d55::1]:42777".into();
        peer.endpoint_id = Some(remote.clone());
        assert_eq!(network.peer_route(&peer).kind, "none");

        let relay_url = "http://82.25.69.187:52777/".to_string();
        network.note_link(
            &remote,
            1,
            P2pLink::new(LinkPath::Relay(relay_url.clone()), false),
        );
        let route = network.peer_route(&peer);
        assert_eq!(route.kind, "relayed");
        assert_eq!(route.address.as_deref(), Some("82.25.69.187:52777"));
        assert_eq!(
            route.relay_kind,
            Some("member"),
            "relay outside relay_peers"
        );
        assert!(!route.hole_punched);
        network.set_dedicated_relays([relay_url]);
        assert_eq!(network.peer_route(&peer).relay_kind, Some("dedicated"));

        // The same connection moves to a direct path: a hole punch.
        network.note_link(
            &remote,
            1,
            P2pLink::new(
                LinkPath::Direct("189.75.167.63:28515".parse().unwrap()),
                true,
            ),
        );
        let route = network.peer_route(&peer);
        assert_eq!(route.kind, "p2p_direct");
        assert_eq!(route.address.as_deref(), Some("189.75.167.63:28515 (UDP)"));
        assert!(route.hole_punched);
        assert_eq!(route.links.len(), 1);

        network
            .routes
            .mark_success(&peer.node_id, "http://192.168.0.37:8080");
        let route = network.peer_route(&peer);
        assert_eq!(route.kind, "http");
        assert_eq!(route.address.as_deref(), Some("192.168.0.37:8080"));
        assert_eq!(route.relay_kind, None);

        // The failing HTTP route drops out; the connection closes.
        network
            .routes
            .mark_failure(&peer.node_id, "http://192.168.0.37:8080");
        network.drop_link(&remote, 1);
        assert_eq!(network.peer_route(&peer).kind, "none");
    }

    /// A path through one of n0's relays is a public one.
    #[tokio::test]
    async fn a_path_through_an_n0_relay_is_public() {
        let network = Network::testkit("eu".into(), String::new(), vec![], None, None, None)
            .await
            .unwrap();
        let Some(n0) = iroh::RelayMode::Default
            .relay_map()
            .urls::<Vec<iroh::RelayUrl>>()
            .into_iter()
            .next()
        else {
            return;
        };
        let remote = crate::p2p::random_endpoint_id().to_string();
        let mut peer = admission_peer(8080);
        peer.endpoint_id = Some(remote.clone());
        let path = crate::p2p::LinkPath::Relay(n0.to_string());
        network.note_link(&remote, 1, P2pLink::new(path.clone(), false));
        assert_eq!(network.peer_route(&peer).relay_kind, Some("public"));
        assert!(!network.path_carries_transfers(Some(&path)).await);
    }

    /// With no verdict in the cache the answer is `None`, with no probe: it is what the
    /// transport task consults, and there a probe would wait for the loop itself (see
    /// `admit_in_background`).
    #[tokio::test]
    async fn a_cache_miss_is_answered_without_probing() {
        let identity = crate::identity::Identity::generate();
        let network = Network::testkit(
            identity.node_id.0.clone(),
            identity.public_key_hex(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_admission(
            admission_config("correct", crate::config::NetworkVisibility::Private),
            Arc::new(crate::admin_trust::AdminTrust::empty()),
        );
        // Port 1: nothing listens there, the direct probe fails.
        let peer = admission_peer(1);

        assert_eq!(network.admission_without_probe(&peer).await, None);
        assert!(!network.admit_peer(&peer).await);
        assert_eq!(network.admission_without_probe(&peer).await, Some(false));
    }

    /// With no answer, only recent proof holds the peer; a refusal never does.
    #[test]
    fn an_unreachable_recheck_keeps_only_a_recently_proven_peer() {
        let now = Instant::now();
        let recent = now - StdDuration::from_secs(60);
        let stale = now - ADMISSION_UNREACHABLE_GRACE - StdDuration::from_secs(1);

        let kept = admission_verdict(AdmissionProbe::Unreachable, Some(recent), now);
        assert!(kept.allowed);
        assert_eq!(
            kept.proven_at,
            Some(recent),
            "the slack does not renew the proof"
        );
        assert_eq!(kept.expires_at, now + REJECTED_ADMISSION_CACHE_TTL);

        assert!(!admission_verdict(AdmissionProbe::Unreachable, Some(stale), now).allowed);
        assert!(!admission_verdict(AdmissionProbe::Unreachable, None, now).allowed);
        assert!(!admission_verdict(AdmissionProbe::Refused, Some(recent), now).allowed);

        let approved = admission_verdict(AdmissionProbe::Approved, None, now);
        assert!(approved.allowed);
        assert_eq!(approved.proven_at, Some(now));
    }

    /// End to end: approved by `/handshake`, the gateway disappears and the cache
    /// expires; the new probe reaches no one and the peer stays admitted.
    #[tokio::test]
    async fn a_proven_peer_survives_a_recheck_that_reaches_nobody() {
        let remote = admission_config("correct", crate::config::NetworkVisibility::Private);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_port = listener.local_addr().unwrap().port();
        let app = axum::Router::new()
            .route(
                "/handshake",
                axum::routing::post(crate::session::handle_handshake),
            )
            .with_state(remote);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let identity = crate::identity::Identity::generate();
        let network = Network::testkit(
            identity.node_id.0.clone(),
            identity.public_key_hex(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_admission(
            admission_config("correct", crate::config::NetworkVisibility::Private),
            Arc::new(crate::admin_trust::AdminTrust::empty()),
        );
        let peer = admission_peer(gateway_port);
        assert!(network.admit_peer(&peer).await);

        server.abort();
        let _ = server.await;
        for entry in network.admission_cache.write().unwrap().values_mut() {
            entry.expires_at = Instant::now() - StdDuration::from_secs(1);
        }
        assert!(
            network.admit_peer(&peer).await,
            "without an answer, the recent proof keeps the peer"
        );
    }

    /// The background probe only warms the cache, once per peer, and does not
    /// touch the table: whoever inserts is the next announce.
    #[tokio::test]
    async fn background_admission_warms_the_cache_once() {
        let remote = admission_config("correct", crate::config::NetworkVisibility::Private);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_port = listener.local_addr().unwrap().port();
        let app = axum::Router::new()
            .route(
                "/handshake",
                axum::routing::post(crate::session::handle_handshake),
            )
            .with_state(remote);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let identity = crate::identity::Identity::generate();
        let network = Arc::new(
            Network::testkit(
                identity.node_id.0.clone(),
                identity.public_key_hex(),
                vec![],
                None,
                None,
                None,
            )
            .await
            .unwrap()
            .with_admission(
                admission_config("correct", crate::config::NetworkVisibility::Private),
                Arc::new(crate::admin_trust::AdminTrust::empty()),
            ),
        );
        let peer = admission_peer(gateway_port);

        network.admit_in_background(peer.clone());
        network.admit_in_background(peer.clone());
        assert_eq!(network.pending_admissions.lock().unwrap().len(), 1);

        let verdict = tokio::time::timeout(StdDuration::from_secs(5), async {
            loop {
                if let Some(allowed) = network.admission_without_probe(&peer).await {
                    return allowed;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the background probe must complete");
        assert!(verdict);
        assert_eq!(network.peer_count().await, 0);
        tokio::time::timeout(StdDuration::from_secs(1), async {
            while !network.pending_admissions.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the pending probe mark must go away");

        server.abort();
    }

    #[tokio::test]
    async fn public_admission_rejects_unknown_without_trust_and_accepts_anchored_principal() {
        let identity = crate::identity::Identity::generate();
        let node_id = identity.node_id.0.clone();
        let pubkey = identity.public_key_hex();
        let unknown = Network::testkit(node_id.clone(), pubkey.clone(), vec![], None, None, None)
            .await
            .unwrap()
            .with_admission(
                admission_config("", crate::config::NetworkVisibility::Public),
                Arc::new(crate::admin_trust::AdminTrust::empty()),
            );
        unknown.insert_peer(admission_peer(8080)).await;
        assert_eq!(unknown.peer_count().await, 0);

        let assignment = Arc::new(RwLock::new(None));
        let announce: P2PMessage = serde_json::from_value(serde_json::json!({
            "type": "Announce",
            "node_id": "unknown",
            "pubkey": "not-a-node-key",
            "addr": "127.0.0.1:42777",
            "model": "qwen",
            "start_layer": 0,
            "end_layer": 1,
            "total_layers": 2,
            "memory_mb": 0.0,
            "timestamp": Utc::now(),
        }))
        .unwrap();
        unknown
            .handle_message(announce, "127.0.0.1:42777".parse().unwrap(), &assignment)
            .await;
        assert_eq!(unknown.peer_count().await, 0);

        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let peer_id = crate::p2p::secret_key(&signing_key).public().to_string();
        let mut peer = admission_peer_for(8080, &signing_key);
        let principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            peer.node_id.clone(),
            peer_id.clone(),
            &signing_key,
        );
        let path =
            std::env::temp_dir().join(format!("pom-admission-{}.json", uuid::Uuid::new_v4()));
        let principal_copy = principal.clone();
        let trust = crate::admin_trust::AdminTrust::bootstrap_primary(
            &path,
            "cluster",
            principal.clone(),
            &signing_key,
        )
        .unwrap();
        let trusted = Network::testkit(node_id, pubkey, vec![], None, None, None)
            .await
            .unwrap()
            .with_admission(
                // The open mode: only a public network left on
                // `anyone_with_anchor` admits by the trust anchor alone.
                admission_config_with_policy(
                    "local-admin-credential",
                    crate::config::NetworkVisibility::Public,
                    crate::config::NetworkAccessPolicy::AnyoneWithAnchor,
                ),
                Arc::new(trust),
            );
        peer.pubkey = principal.pubkey;
        peer.endpoint_id = Some(peer_id);
        peer.cluster_id = Some("cluster".into());
        trusted.insert_peer(peer.clone()).await;
        assert_eq!(trusted.peer_count().await, 1);

        // The same anchored peer on a listed network that requires approval:
        // the anchor is not enough, the connection key decides, and with no
        // key in common the peer stays out.
        let path =
            std::env::temp_dir().join(format!("pom-admission-gated-{}.json", uuid::Uuid::new_v4()));
        let gated_trust = crate::admin_trust::AdminTrust::bootstrap_primary(
            &path,
            "cluster",
            principal_copy,
            &signing_key,
        )
        .unwrap();
        let gated = Network::testkit(
            identity.node_id.0.clone(),
            identity.public_key_hex(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_admission(
            admission_config_with_policy(
                "",
                crate::config::NetworkVisibility::Public,
                crate::config::NetworkAccessPolicy::Authorized,
            ),
            Arc::new(gated_trust),
        );
        gated.insert_peer(peer).await;
        assert_eq!(gated.peer_count().await, 0);
        let _ = std::fs::remove_file(&path);

        let _ = std::fs::remove_file(path);
    }

    fn admission_config_with_policy(
        key: &str,
        visibility: crate::config::NetworkVisibility,
        access_policy: crate::config::NetworkAccessPolicy,
    ) -> crate::auth::AuthState {
        let state = admission_config(key, visibility);
        state
            .try_write()
            .expect("fresh admission config lock is uncontended")
            .network
            .access_policy = access_policy;
        state
    }

    /// A co-located worker only has the anchor and the `connection_key`, never an
    /// authority grant. Under `anyone_with_anchor` its Announce - with the
    /// identity bound (node_id <-> pubkey <-> authority key <-> EndpointId) and the
    /// local anchor's `cluster_id` - is enough to enter `peers`. This is what
    /// `6c3b19db` broke: only the primary/secondary got through.
    #[tokio::test]
    async fn public_anyone_with_anchor_admits_anchored_worker_without_grant() {
        let local = crate::identity::Identity::generate();

        let primary_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let primary = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            "admin-node",
            "admin-peer",
            &primary_key,
        );
        let trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
            "cluster",
            primary,
            &primary_key,
        )
        .unwrap();

        // Worker peer: its own authority key, distinct from the primary's.
        let worker_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let worker_peer_id = crate::p2p::secret_key(&worker_key).public().to_string();
        let mut worker = admission_peer_for(8081, &worker_key);
        worker.endpoint_id = Some(worker_peer_id);
        worker.cluster_id = Some("cluster".into());

        let net = Network::testkit(
            local.node_id.0.clone(),
            local.public_key_hex(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_admission(
            admission_config_with_policy(
                "shared",
                crate::config::NetworkVisibility::Public,
                crate::config::NetworkAccessPolicy::AnyoneWithAnchor,
            ),
            Arc::new(trust),
        );
        net.insert_peer(worker.clone()).await;
        assert_eq!(net.peer_count().await, 1);
    }

    /// The relaxation only applies to `anyone_with_anchor`. With `authorized` (the default) the
    /// same worker without a grant stays out, and a `cluster_id` that is not the
    /// anchor's never enters under any policy.
    #[tokio::test]
    async fn public_authorized_still_requires_a_grant_and_anchor_rejects_wrong_cluster() {
        let local = crate::identity::Identity::generate();
        let primary_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);

        let worker_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let worker_peer_id = crate::p2p::secret_key(&worker_key).public().to_string();
        let mut worker = admission_peer_for(8081, &worker_key);
        worker.endpoint_id = Some(worker_peer_id);
        worker.cluster_id = Some("cluster".into());

        let build = |cluster: Option<&'static str>| {
            let trust = crate::admin_trust::AdminTrust::bootstrap_primary_in_memory(
                "cluster",
                crate::admin_trust::AuthorityPrincipal::from_signing_key(
                    "admin-node",
                    "admin-peer",
                    &primary_key,
                ),
                &primary_key,
            )
            .unwrap();
            let mut peer = worker.clone();
            peer.cluster_id = cluster.map(str::to_string);
            (trust, peer)
        };

        // authorized + worker without a grant => refused.
        let (trust, peer) = build(Some("cluster"));
        let net = Network::testkit(
            local.node_id.0.clone(),
            local.public_key_hex(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_admission(
            admission_config_with_policy(
                "shared",
                crate::config::NetworkVisibility::Public,
                crate::config::NetworkAccessPolicy::Authorized,
            ),
            Arc::new(trust),
        );
        net.insert_peer(peer).await;
        assert_eq!(net.peer_count().await, 0);

        // anyone_with_anchor + cluster_id that is not the anchor's => refused.
        let (trust, peer) = build(Some("outro-enxame"));
        let net = Network::testkit(
            local.node_id.0.clone(),
            local.public_key_hex(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_admission(
            admission_config_with_policy(
                "shared",
                crate::config::NetworkVisibility::Public,
                crate::config::NetworkAccessPolicy::AnyoneWithAnchor,
            ),
            Arc::new(trust),
        );
        net.insert_peer(peer).await;
        assert_eq!(net.peer_count().await, 0);
    }

    #[tokio::test]
    async fn public_directory_keeps_unadmitted_public_candidates_out_of_pipeline() {
        let identity = crate::identity::Identity::generate();
        let network = Network::testkit(
            identity.node_id.0.clone(),
            identity.public_key_hex(),
            vec![],
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .with_admission(
            admission_config("", crate::config::NetworkVisibility::Public),
            Arc::new(crate::admin_trust::AdminTrust::empty()),
        );

        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let mut peer = admission_peer_for(8080, &signing_key);
        let peer_id = crate::p2p::secret_key(&signing_key).public().to_string();
        let principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            peer.node_id.clone(),
            peer_id.clone(),
            &signing_key,
        );
        peer.pubkey = principal.pubkey;
        peer.endpoint_id = Some(peer_id);
        peer.cluster_id = Some("public-network".into());

        assert!(!network.admit_peer(&peer).await);
        network.remember_public_discovery_peer(&peer).await;

        assert_eq!(network.peer_count().await, 0);
        assert_eq!(network.public_discovered_peers().await.len(), 1);
    }

    /// Transitive discovery was one-shot: `PeerRequest` only went out at
    /// startup and only to the bootstrap nodes. Outside the LAN (without the IPv4 broadcast
    /// to mask it) a known peer was never asked again, so *its*
    /// peers were never learned. The gossip loop has to go back to
    /// asking, periodically, the live peers too.
    #[tokio::test]
    async fn a_known_peer_is_asked_for_its_peers_periodically() {
        let (sock, addr) = probe().await;
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap();
        net.peers
            .write()
            .await
            .insert("peer-a".into(), dummy_peer("peer-a", &addr));

        // A tick that falls on a discovery sweep.
        net.discovery_sweep(DISCOVERY_SWEEP_TICKS).await.unwrap();

        let msg = recv_msg(&sock, 500)
            .await
            .expect("nothing arrived at the peer");
        assert!(
            matches!(msg, P2PMessage::PeerRequest { .. }),
            "expected PeerRequest, got {msg:?}"
        );
    }

    /// The sweep cannot ask everyone on every tick: that is
    /// O(peers^2) of PeerList. Each sweep tick talks to at most
    /// `SWEEP_FANOUT` peers, and the round-robin covers the whole cluster over
    /// time.
    #[tokio::test]
    async fn the_sweep_rotates_over_a_bounded_subset() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap();
        let mut probes = Vec::new();
        {
            let mut peers = net.peers.write().await;
            for i in 0..8 {
                let (sock, addr) = probe().await;
                let id = format!("peer-{i}");
                peers.insert(id.clone(), dummy_peer(&id, &addr));
                probes.push((id, sock));
            }
        }

        let mut asked: std::collections::HashSet<String> = Default::default();
        let mut sweeps = 0u64;
        // Runs enough ticks for the round-robin to go all the way around.
        for tick in 1..=(DISCOVERY_SWEEP_TICKS * 8) {
            let sent = net.discovery_sweep(tick).await.unwrap();
            if tick % DISCOVERY_SWEEP_TICKS == 0 {
                sweeps += 1;
                assert!(
                    sent <= SWEEP_FANOUT,
                    "the sweep talked to {sent} peers, above the limit {SWEEP_FANOUT}"
                );
            } else {
                assert_eq!(sent, 0, "a tick outside the sweep must ask nothing");
            }
        }
        assert_eq!(sweeps, 8);

        for (id, sock) in &probes {
            if recv_msg(sock, 50).await.is_some() {
                asked.insert(id.clone());
            }
        }
        assert_eq!(asked.len(), 8, "the rotation must cover all the peers");
    }

    /// Same guard as `a_detached_node_neither_announces_nor_looks_for_bootstrap`,
    /// now for the new path.
    #[tokio::test]
    async fn a_detached_node_does_not_sweep() {
        let (sock, addr) = probe().await;
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap();
        net.peers
            .write()
            .await
            .insert("peer-a".into(), dummy_peer("peer-a", &addr));
        net.detach().await;

        assert_eq!(net.discovery_sweep(DISCOVERY_SWEEP_TICKS).await.unwrap(), 0);
        assert!(recv_msg(&sock, 300).await.is_none(), "detached node spoke");
    }

    /// A panic while handling a datagram cannot kill the task that drains
    /// the socket: the next datagram still needs to update the known peer.
    #[tokio::test]
    async fn receive_loop_survives_a_panicking_message_and_processes_the_next() {
        let net = Arc::new(
            Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
                .await
                .unwrap(),
        );
        let assignment = Arc::new(RwLock::new(None));
        let initial = Utc::now();
        net.peers
            .write()
            .await
            .insert("peer".into(), dummy_peer("peer", "127.0.0.1:42777"));

        net.test_panic_on_next_message.store(true, Ordering::SeqCst);
        let loop_task = {
            let net = net.clone();
            let assignment = assignment.clone();
            tokio::spawn(async move { net.receive_loop(assignment).await })
        };
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            net.bind_addr.port(),
        );

        let panic_message = P2PMessage::Heartbeat {
            node_id: "peer".into(),
            timestamp: initial,
        };
        sender
            .send_to(&serde_json::to_vec(&panic_message).unwrap(), destination)
            .await
            .unwrap();

        let next_timestamp = initial + chrono::Duration::seconds(1);
        let next_message = P2PMessage::Heartbeat {
            node_id: "peer".into(),
            timestamp: next_timestamp,
        };
        sender
            .send_to(&serde_json::to_vec(&next_message).unwrap(), destination)
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if net.peers.read().await["peer"].last_seen == next_timestamp {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("receive_loop did not process the message after the panic");

        loop_task.abort();
    }

    #[tokio::test]
    async fn ipv6_destinations_are_left_alone() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
            .await
            .unwrap();
        let v6: SocketAddr = "[fe80::1]:42777".parse().unwrap();
        assert_eq!(net.normalize(v6), v6);
    }

    /// The node binds on `[::]` and needs to receive from an IPv4 peer - which is how the whole
    /// LAN talks. Turning off `IPV6_V6ONLY` after the bind has no effect, and on
    /// Windows the default is on: without this hand-assembled socket, a node there would
    /// see no announce at all and discovery would go silent without saying why.
    #[tokio::test]
    async fn a_dual_stack_socket_receives_from_an_ipv4_sender() {
        let target: SocketAddr = "[::]:0".parse().unwrap();
        let Ok(std_sock) = Network::bind_udp_dual_stack(target) else {
            return; // machine without IPv6: the pure v4 path covers that case
        };
        let port = std_sock.local_addr().unwrap().port();
        let sock = UdpSocket::from_std(std_sock).unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"oi", ("127.0.0.1", port)).await.unwrap();

        let mut buf = [0u8; 8];
        let received =
            tokio::time::timeout(std::time::Duration::from_secs(2), sock.recv_from(&mut buf))
                .await
                .expect("nothing arrived from an IPv4 sender on the [::] socket")
                .unwrap();

        assert_eq!(&buf[..received.0], b"oi");
    }
}
