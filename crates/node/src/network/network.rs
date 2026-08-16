use crate::model::LayerAssignment;
use anyhow::Result;
use chrono::{DateTime, Utc};
use routing_contract::ModelCapability;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};
use tracing::{debug, info, warn};

pub const DISCOVERY_PORT: u16 = 42777;
pub const GOSSIP_INTERVAL_SECS: u64 = 5;
pub const PEER_TIMEOUT_SECS: i64 = 30;

/// De quantos em quantos tiques de gossip sai uma varredura de descoberta.
///
/// 6 tiques x 5 s = 30 s. Não pode ser todo tique: responder a um `PeerRequest`
/// custa 1 `Announce` + os `PeerList` fatiados por MTU, então perguntar a todo
/// peer a cada 5 s é tráfego O(peers²) num intervalo curto.
pub const DISCOVERY_SWEEP_TICKS: u64 = 6;

/// Quantos peers vivos são perguntados por varredura.
///
/// É um rodízio, não uma amostra: a varredura de índice `k` começa em
/// `k * SWEEP_FANOUT` sobre a lista de peers ordenada por `node_id`, de modo que
/// o cluster inteiro é coberto em `ceil(peers / SWEEP_FANOUT)` varreduras
/// (2 minutos num cluster de 8) sem nunca passar de 3 perguntas por tique.
///
/// Custo num cluster de 8 nós: cada nó manda 3 `PeerRequest` a cada 30 s; cada
/// resposta é 1 `Announce` (~700 B) + ~4 `PeerList` de 1200 B, ou seja ~5,5 kB.
/// Por nó: ~16,5 kB / 30 s. Cluster todo: ~132 kB / 30 s ≈ 4,4 kB/s. Não há
/// amplificação: a resposta a um `PeerRequest` são `Announce` e `PeerList`, e
/// nenhuma das duas provoca outro `PeerRequest`.
pub const SWEEP_FANOUT: usize = 3;

/// Message types exchanged between nodes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum P2PMessage {
    /// Node announces itself and its layer assignment
    Announce {
        node_id: String,
        pubkey: String,
        addr: String,
        /// Todos os enderecos deste no. `serde(default)` mantem um no antigo
        /// parseavel: sem candidatos, o `addr` unico continua valendo.
        #[serde(default)]
        addr_candidates: Vec<String>,
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
        /// Inference frame encodings this peer can receive. Missing means an
        /// older peer and therefore requires the v1 f16 fallback.
        #[serde(default)]
        inference_frame_capabilities: Vec<String>,
        /// HTTP port serving the API and web UI. `serde(default)` keeps peers
        /// running an older build parseable.
        #[serde(default)]
        gateway_port: Option<u16>,
        #[serde(default)]
        role: String,
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
        /// This node's libp2p `PeerId` (base58), when a stable one could be
        /// derived (see `Discovery::new`). `serde(default)` keeps an older
        /// peer parseable; absent means this node can't be reached over the
        /// libp2p NAT-tunnel fallback (`transport/libp2p-tunnel-v1`).
        #[serde(default)]
        libp2p_peer_id: Option<String>,
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
    /// Todos os enderecos em que este peer pode ser encontrado.
    ///
    /// `addr` continua sendo o preferido historico e o unico que um no antigo
    /// entende. Este conjunto e o que permite a quem vai discar escolher, em
    /// vez de aceitar a escolha de quem planejou de outro lugar da rede.
    #[serde(default)]
    pub addr_candidates: Vec<String>,
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
    #[serde(default)]
    pub inference_frame_capabilities: Vec<String>,
    /// Port serving this peer's HTTP API and web UI, so the dashboard can pull
    /// load figures straight from each peer.
    #[serde(default)]
    pub gateway_port: Option<u16>,
    #[serde(default)]
    pub role: String,
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
    /// See `P2PMessage::Announce::libp2p_peer_id`.
    #[serde(default)]
    pub libp2p_peer_id: Option<String>,
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
        self.ollama_port
            .map(|port| format!("http://{}:{}", ip_host(&self.addr), port))
    }

    /// Acrescenta o endereco de onde o pacote realmente veio.
    ///
    /// Nao substitui: um caminho que funcionou nao invalida os outros, e e este
    /// o unico candidato que enxerga NAT.
    pub fn merge_observed(&mut self, observado: &str) {
        if !self.addr_candidates.iter().any(|c| c == observado) {
            self.addr_candidates.push(observado.to_string());
        }
    }

    /// URLs do servidor de inferencia, uma por candidato.
    ///
    /// Cai no `addr` unico quando o peer nao anunciou candidatos, que e o caso
    /// de um no rodando binario anterior.
    pub fn inference_candidates(&self) -> Vec<String> {
        let Some(porta) = self.inference_port else {
            return Vec::new();
        };
        let base: Vec<&String> = if self.addr_candidates.is_empty() {
            vec![&self.addr]
        } else {
            self.addr_candidates.iter().collect()
        };
        base.iter()
            .map(|a| format!("http://{}:{}", ip_host(a), porta))
            .collect()
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
    socket: Arc<UdpSocket>,
    /// Set while the operator has taken this node off the network.
    ///
    /// A detached node keeps serving its own UI and API — that is how the
    /// operator brings it back — but it stops announcing, stops accepting
    /// anything peers say to it, and forgets everyone it knew. Without the
    /// second half it would be re-discovered on the next gossip tick and the
    /// button would look like it did nothing.
    detached: AtomicBool,
    /// Por onde este no fala com cada peer, medido por este processo.
    pub routes: Arc<crate::route::RouteTable>,
    /// This node's libp2p `PeerId` (base58), when the swarm derived a stable
    /// one from the pkarr identity — see `Discovery::new`. Set once, after
    /// construction, by whoever builds the libp2p keypair (`main.rs`), since
    /// `Network` itself has no opinion on libp2p and is built first. Set
    /// whenever *a* keypair exists (always, in practice — `main.rs` falls
    /// back to an ephemeral one), independent of whether the swarm actually
    /// came up — see `libp2p_tunnel_ready` for that.
    pub libp2p_peer_id: Option<String>,
    /// Whether the libp2p swarm actually started and can serve inbound
    /// NAT-tunnel requests right now. Separate from `libp2p_peer_id` on
    /// purpose: a keypair can be derived even when `Discovery::new` itself
    /// then fails (e.g. the libp2p TCP port is already taken) — advertising
    /// a capability with no swarm behind it would send peers to dial a
    /// PeerId that can never answer, worse than just not claiming it.
    /// Mutable through `&self` (mirrors `detached`) because it's known only
    /// after `Network` is already behind an `Arc` and shared.
    libp2p_tunnel_ready: AtomicBool,
    /// A cabeça de rascunho `nextn` (MTP) está mesmo montada no motor deste
    /// nó? Só o servidor de inferência sabe - depende do GGUF carregado, não
    /// da configuração: um modelo sem bloco `nextn`, ou qualquer fatia que não
    /// seja a final, não tem o que anexar. Por isso o `Arc`: o motor mora em
    /// outra task e é ele quem escreve aqui, enquanto o anúncio (`self_announce`)
    /// e a entrada local do gateway leem. Anunciar `speculate/v1-mtp` sem
    /// cabeça mandaria o gateway pedir um rascunho que nunca vem.
    mtp_ready: Arc<AtomicBool>,
    /// This node's own address, as reported by a peer's libp2p `identify`
    /// response (`info.observed_addr` — the address *that peer* saw us
    /// connecting from). `host:port`, same shape as `advertise_addr`. Set
    /// from `discovery.rs`'s event loop, so `std::sync::RwLock` rather than
    /// `tokio::sync::RwLock` — the setter never needs to hold it across an
    /// `.await`. Never overrides an operator-pinned `advertise_addr`; only
    /// fills the gap when nothing was pinned, which otherwise leaves a NAT'd
    /// node with nothing to offer remote peers but a private LAN IP from
    /// local-interface enumeration. See `set_observed_addr`.
    observed_addr: std::sync::RwLock<Option<String>>,
}

/// Os primeiros 12 bytes de um id, para o log, **sem cortar no meio de um
/// caractere e sem estourar em id curto**.
///
/// `&node_id[..12]` num id de dois caracteres derrubou o cluster do captain em
/// 2026-08-16: um `PeerList` com um id curto chegou por broadcast na LAN, o
/// `receive_loop` entrou em pânico em `byte index 12 is out of bounds of 'eu'`,
/// **a task morreu** e o nó parou de receber UDP para sempre - enquanto o HTTP,
/// o gossip e a TUI seguiam saudáveis, e a única pista era um socket com a fila
/// de recepção cheia. O id vem da rede: qualquer coisa que o outro lado mande
/// tem que ser tratada como texto arbitrário, não como um SHA256 de 64 hexas.
fn short(id: &str) -> &str {
    let mut fim = id.len().min(12);
    while fim > 0 && !id.is_char_boundary(fim) {
        fim -= 1;
    }
    &id[..fim]
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
const MAX_DATAGRAM_PAYLOAD: usize = 1200;

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
    if raw.parse::<SocketAddr>().is_ok() {
        return Some(raw.to_string());
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
        Some(raw.to_string())
    } else {
        Some(format!("{raw}:{default_port}"))
    }
}

impl Network {
    /// Socket UDP que enxerga IPv6 e IPv4 no mesmo descritor.
    ///
    /// `IPV6_V6ONLY` só tem efeito se for desligado **antes** do `bind`, e o
    /// `UdpSocket::bind` da std já liga. No Linux e no macOS o padrão do
    /// sistema é 0 e a descoberta funcionava por acidente; no Windows o padrão
    /// é 1, e um nó ligado em `[::]` não receberia anúncio nenhum de peer IPv4
    /// - ou seja, nenhum peer da LAN. É o mesmo tipo de defeito silencioso e
    /// só-numa-plataforma que o `normalize` abaixo já guarda com teste.
    fn bind_udp_dual_stack(addr: SocketAddr) -> std::io::Result<std::net::UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};

        let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_only_v6(false)?;
        // Broadcast v4 sai deste socket justamente por causa do only_v6=false.
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
        // Bind dual-stack IPv6 so peers on IPv6 (including link-local fe80::) can be reached.
        // Falls back to IPv4-only if the OS has IPv6 disabled.
        let (socket, bind_addr) = {
            let v6: SocketAddr = format!("[::]:{}", port).parse()?;
            match Self::bind_udp_dual_stack(v6) {
                Ok(s) => {
                    info!("UDP socket bound to [::]:{} (dual-stack)", port);
                    (UdpSocket::from_std(s)?, v6)
                }
                Err(e) => {
                    info!("{}", e);
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
            advertise_addr: std::sync::RwLock::new(None),
            socket: Arc::new(socket),
            detached: AtomicBool::new(false),
            routes: Arc::new(crate::route::RouteTable::new()),
            libp2p_peer_id: None,
            libp2p_tunnel_ready: AtomicBool::new(false),
            mtp_ready: Arc::new(AtomicBool::new(false)),
            observed_addr: std::sync::RwLock::new(None),
        })
    }

    /// Label this node carries in its announces.
    pub fn with_identity(mut self, role: String, hostname: String) -> Self {
        self.role = role;
        self.hostname = hostname;
        self
    }

    /// Record the libp2p `PeerId` this node's swarm ended up with, so
    /// `self_announce` and `GatewayState::all_peers`'s self-entry can
    /// advertise it. A no-op call (`None`) is valid — it just means this run
    /// has no stable libp2p identity to offer peers for the NAT-tunnel path.
    pub fn with_libp2p_peer_id(mut self, id: Option<String>) -> Self {
        self.libp2p_peer_id = id;
        self
    }

    /// Pin the address peers should reach this node at, at construction time.
    pub fn with_advertise_addr(self, raw: Option<String>) -> Self {
        self.set_advertise_addr(raw.as_deref());
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
            info!("Anunciando o endereço fixo {a} (em vez do endereço de origem)");
        }
        *self.advertise_addr.write().expect("advertise_addr lock") = normalized;
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

    /// Record a peer's libp2p `identify` response as evidence of this node's
    /// own public address (`info.observed_addr` — see `discovery.rs`'s
    /// `identify::Event::Received` handler, the only caller). A no-op when
    /// `host_port`'s host is still private/loopback/link-local
    /// (`addrs::is_private_or_local`) — that's not new information, just a
    /// peer on the same LAN or a NAT rewriting within another private range.
    pub fn set_observed_addr(&self, host_port: &str) {
        let host = ip_host(host_port).trim_start_matches('[').trim_end_matches(']');
        if crate::addrs::is_private_or_local(host) {
            return;
        }
        let mut guard = self.observed_addr.write().expect("observed_addr lock");
        if guard.as_deref() != Some(host_port) {
            info!("📡 Endereço público observado por um peer: {host_port}");
        }
        *guard = Some(host_port.to_string());
    }

    fn observed_addr(&self) -> Option<String> {
        self.observed_addr.read().expect("observed_addr lock").clone()
    }

    /// Host to fall back on when nothing was explicitly pinned: the
    /// operator's own choice (`advertise_addr`) always wins over anything
    /// this node merely inferred about itself.
    ///
    /// `pub` (not just used by `self_addr_candidates`/`announce_addr`
    /// internally) because `GatewayState::all_peers`'s self-entry
    /// (`gateway.rs`) needs the exact same pinned-then-observed preference
    /// when it builds this node's own `.addr` — without it, that entry fell
    /// straight to `advertised_host()` alone and skipped the peer-observed
    /// public address entirely, so a NAT'd node's own dashboard card kept
    /// showing its private LAN IP even after `Announce`/`self_addr_candidates`
    /// were already fixed.
    pub fn effective_host(&self) -> Option<String> {
        self.advertised_host()
            .or_else(|| self.observed_addr().map(|a| ip_host(&a).to_string()))
    }

    /// Enderecos que este no anuncia de si.
    ///
    /// O endereco fixado pelo operador vem primeiro; na falta dele, o
    /// endereco publico observado por algum peer via libp2p `identify` (ver
    /// `set_observed_addr`) - so entao vem o resto, das interfaces desta
    /// maquina, ja filtrado. Um no sem nenhum endereco enumeravel devolve
    /// lista vazia, e o peer do outro lado cai no endereco observado do
    /// pacote - que e o comportamento de sempre.
    pub fn self_addr_candidates(&self) -> Vec<String> {
        let porta = self.bind_addr.port();
        crate::addrs::filter_candidates(crate::addrs::local_addrs(), self.effective_host().as_deref())
            .into_iter()
            .map(|ip| {
                if ip.contains(':') && !ip.starts_with('[') {
                    format!("[{ip}]:{porta}")
                } else {
                    format!("{ip}:{porta}")
                }
            })
            .collect()
    }

    /// What this node puts in `Announce.addr`.
    ///
    /// `0.0.0.0:<port>` is the "ask the receiver" sentinel: `handle_message`
    /// replaces it with the source address of the packet. Falls back to a
    /// peer-observed public address (`set_observed_addr`) before that
    /// sentinel — a NAT'd node with no `--advertise-addr`/`--public-addr` set
    /// otherwise has nothing better to offer a remote peer reached over
    /// gossipsub, where there is no packet source to rewrite `addr` with the
    /// way the UDP path does.
    pub fn announce_addr(&self) -> String {
        self.advertise_addr
            .read()
            .expect("advertise_addr lock")
            .clone()
            .or_else(|| self.observed_addr())
            .unwrap_or_else(|| format!("0.0.0.0:{}", self.bind_addr.port()))
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
    ///
    /// Este é o **único** construtor de `Announce` do nó: o UDP (bootstrap,
    /// peers, broadcast e a resposta a `PeerRequest`) e o publish periódico no
    /// gossipsub passam todos por aqui. Um segundo construtor em `main.rs`
    /// anunciava menos capabilities, com `role`/`hostname` vazios e só quando
    /// havia assignment, então um peer que chegasse pela internet via um nó
    /// diferente do que a LAN via.
    fn self_announce(&self, assignment: Option<&LayerAssignment>) -> P2PMessage {
        P2PMessage::Announce {
            node_id: self.node_id.clone(),
            pubkey: self.pubkey.clone(),
            addr: self.announce_addr(),
            addr_candidates: self.self_addr_candidates(),
            model: assignment.map(|a| a.model.clone()).unwrap_or_default(),
            start_layer: assignment.map(|a| a.start_layer).unwrap_or(0),
            end_layer: assignment.map(|a| a.end_layer).unwrap_or(0),
            total_layers: assignment.map(|a| a.total_layers).unwrap_or(0),
            memory_mb: assignment.map(|a| a.memory_used_mb).unwrap_or(0.0),
            timestamp: Utc::now(),
            ollama_port: self.ollama_port,
            inference_port: self.inference_port,
            // Keep this list in sync with the other two `Announce` builders
            // (gateway.rs, infer_server.rs) and with the capability constants
            // in routing/pipeline.rs (`CAP_RELAY_CHAIN`, `CAP_SPECULATE_LOOKUP`,
            // `CAP_LIBP2P_TUNNEL`). The last one only goes out once the swarm
            // has actually come up (`libp2p_tunnel_ready`) — claiming it any
            // earlier would send peers to dial a PeerId that can't answer.
            inference_frame_capabilities: {
                let mut caps = vec![
                    "frame/v1-f16".into(),
                    "frame/v2-zstd-f16".into(),
                    "relay/v1-chain".into(),
                    "relay/v2-direct".into(),
                    "speculate/v1-lookup".into(),
                    // Todo binário novo fala o protocolo do MTP fatiado
                    // (rascunho pré-fabricado + tokens + mtp_k na wire);
                    // `supports_mtp` exige a cap de todos os passos para
                    // excluir peer velho do anel.
                    "speculate/v1-mtp-sliced".into(),
                ];
                if self.mtp_ready() {
                    caps.push("speculate/v1-mtp".into());
                }
                if self.libp2p_tunnel_ready() {
                    caps.push("transport/libp2p-tunnel-v1".into());
                }
                caps
            },
            gateway_port: self.gateway_port,
            role: self.role.clone(),
            hostname: self.hostname.clone(),
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
            libp2p_peer_id: self.libp2p_peer_id.clone(),
        }
    }

    /// O anúncio serializado que o publish periódico do gossipsub manda.
    ///
    /// Existe para que o caminho libp2p não tenha construtor próprio: é o mesmo
    /// `self_announce` do UDP, com as mesmas capabilities, o mesmo `role` e o
    /// mesmo `hostname`, e sai também quando não há assignment.
    pub fn gossip_announce_bytes(&self, assignment: Option<&LayerAssignment>) -> Option<Vec<u8>> {
        serde_json::to_vec(&self.self_announce(assignment)).ok()
    }

    /// True while the operator has taken this node off the network.
    pub fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Relaxed)
    }

    /// Called once `Discovery::new` has actually succeeded (or failed) —
    /// see the field's own doc comment for why this is tracked separately
    /// from `libp2p_peer_id`.
    pub fn set_libp2p_tunnel_ready(&self, ready: bool) {
        self.libp2p_tunnel_ready.store(ready, Ordering::Relaxed);
    }

    pub fn libp2p_tunnel_ready(&self) -> bool {
        self.libp2p_tunnel_ready.load(Ordering::Relaxed)
    }

    /// A alça que o servidor de inferência escreve quando (não) conseguiu
    /// anexar a cabeça `nextn`. Ver o campo `mtp_ready`.
    pub fn mtp_ready_handle(&self) -> Arc<AtomicBool> {
        self.mtp_ready.clone()
    }

    pub fn mtp_ready(&self) -> bool {
        self.mtp_ready.load(Ordering::Relaxed)
    }

    /// Leave the network: stop speaking, stop listening, forget the peers.
    pub async fn detach(&self) {
        self.detached.store(true, Ordering::Relaxed);
        let forgotten = { self.peers.write().await.drain().count() };
        info!("🔌 Nó fora da rede — {forgotten} peers esquecidos");
    }

    /// Rejoin: start announcing again and go looking for the bootstrap nodes.
    pub async fn rejoin(&self, assignment: Option<&LayerAssignment>) -> Result<()> {
        self.detached.store(false, Ordering::Relaxed);
        info!("🔌 Nó voltando para a rede");
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
                self.send_to(&msg, &peer.addr).await?;
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

    /// Varredura periódica de descoberta: repergunta quem são os peers dos
    /// outros, para os bootstrap nodes e para um subconjunto rotativo dos peers
    /// vivos.
    ///
    /// Sem isto a descoberta transitiva é um retrato do arranque: `PeerRequest`
    /// só saía uma vez, e só para os bootstrap nodes. Na LAN o broadcast IPv4
    /// escondia o defeito; entre sites, um nó que entrou depois no cluster do
    /// vizinho nunca era aprendido.
    ///
    /// Devolve quantos peers foram perguntados neste tique (0 fora da varredura,
    /// 0 se destacado), o que é o que os testes observam.
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

        // Ordem estável (por node_id) para que o rodízio avance de fato em vez
        // de sortear os mesmos peers a cada volta.
        let mut targets: Vec<String> = {
            let peers = self.peers.read().await;
            let mut v: Vec<(&String, &PeerInfo)> =
                peers.iter().filter(|(_, p)| p.is_alive()).collect();
            v.sort_by(|a, b| a.0.cmp(b.0));
            v.into_iter().map(|(_, p)| p.addr.clone()).collect()
        };
        if targets.is_empty() {
            return Ok(0);
        }

        let sweep = (tick / DISCOVERY_SWEEP_TICKS) as usize;
        let len = targets.len();
        let start = sweep.wrapping_mul(SWEEP_FANOUT) % len;
        targets.rotate_left(start);
        targets.truncate(SWEEP_FANOUT.min(len));

        for addr in &targets {
            self.send_to(&msg, addr).await?;
        }
        Ok(targets.len())
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
            let mut tick: u64 = 0;
            loop {
                ticker.tick().await;
                tick = tick.wrapping_add(1);
                // Announce unconditionally: a node without a model still has to
                // be discoverable, otherwise a freshly configured cluster is
                // made of nodes that only listen and never speak.
                let asgn = assignment_gossip.read().await.clone();
                let _ = net_gossip.announce(asgn.as_ref()).await;
                // Descoberta contínua: sem isto, quem só é alcançável fora da
                // LAN nunca é aprendido depois do arranque.
                let _ = net_gossip.discovery_sweep(tick).await;
                net_gossip.cleanup_dead_peers().await;
            }
        });

        // Medicao de rotas, em task propria: uma sonda lenta nao pode segurar o
        // anuncio nem a limpeza de peers mortos. Com 9 peers e ate 4 candidatos
        // cada, o timeout de 3 s por sonda passaria de um minuto - e o laco de
        // gossip e quem expira peer morto.
        let net_routes = self.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::new();
            let mut ticker = interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                if net_routes.is_detached() {
                    continue;
                }
                for peer in net_routes.alive_peers().await {
                    if !net_routes.routes.precisa_medir(&peer.node_id) {
                        continue;
                    }
                    let candidatos = peer.inference_candidates();
                    if candidatos.is_empty() {
                        continue;
                    }
                    net_routes
                        .routes
                        .measure_peer(&http, &peer.node_id, &candidatos)
                        .await;
                }
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
        // A node the operator took off the network does not learn peers and does
        // not answer them. The socket stays bound so rejoining costs nothing.
        if self.is_detached() {
            return;
        }
        match msg {
            P2PMessage::Announce {
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
                hostname,
                capabilities,
                disk_total_mb,
                disk_free_mb,
                libp2p_peer_id,
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
                    node_id: node_id.clone(),
                    pubkey,
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
                    inference_frame_capabilities,
                    gateway_port,
                    role,
                    hostname,
                    capabilities,
                    connection_verified: false,
                    disk_total_mb,
                    disk_free_mb,
                    api_keys_active: 0,
                    libp2p_peer_id,
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

                // O endereco de onde o pacote veio e o unico comprovadamente
                // funcional, mas nao invalida os outros: por isso entra no
                // conjunto em vez de substitui-lo.
                peer.merge_observed(&peer_addr);

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
                let mut known = self.peers.write().await;
                for peer in peers {
                    if peer.node_id != self.node_id && !known.contains_key(&peer.node_id) {
                        info!(
                            "📋 Learned peer from gossip: {} layers={}-{}",
                            short(&peer.node_id),
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
                short(&node_id),
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

    /// Um id curto na rede não pode derrubar o `receive_loop`.
    ///
    /// Reproduz o que tirou o cluster do captain do ar em 2026-08-16: um
    /// `PeerList` chegou com `node_id` de dois caracteres, `&id[..12]` entrou em
    /// pânico, a task do `receive_loop` morreu e o nó parou de receber UDP
    /// enquanto continuava respondendo HTTP - indistinguível de saudável na tela.
    #[test]
    fn a_short_node_id_from_the_wire_never_panics() {
        assert_eq!(short("eu"), "eu");
        assert_eq!(short(""), "");
        assert_eq!(short("0123456789abcdef"), "0123456789ab");
        // Não corta no meio de um caractere multibyte.
        assert_eq!(short("ááááááá"), "áááááá");
    }

    /// O gossipsub e o UDP têm que publicar o mesmo anúncio, inclusive quando o
    /// nó ainda não tem modelo. O publish do libp2p já teve construtor próprio,
    /// que omitia `relay/v1-chain` e `speculate/v1-lookup`, mandava `role` e
    /// `hostname` vazios e **só** publicava com assignment: um nó sem modelo
    /// ficava invisível pela internet enquanto a LAN o via normalmente.
    #[tokio::test]
    async fn gossipsub_and_udp_publish_the_same_announce() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, Some(8181), Some(8080))
            .await
            .unwrap()
            .with_identity("Principal".into(), "macbook".into());

        let asgn = LayerAssignment {
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
                    .expect("o gossipsub publica sempre, com ou sem assignment"),
            )
            .unwrap();
            let udp: serde_json::Value =
                serde_json::to_value(net.self_announce(assignment)).unwrap();
            // `timestamp` é o único campo que muda entre as duas construções.
            let mut gossip = gossip;
            let mut udp = udp;
            gossip["timestamp"] = serde_json::Value::Null;
            udp["timestamp"] = serde_json::Value::Null;
            assert_eq!(gossip, udp);

            let caps = gossip["inference_frame_capabilities"].as_array().unwrap();
            for cap in [
                "frame/v1-f16",
                "frame/v2-zstd-f16",
                "relay/v1-chain",
                "relay/v2-direct",
                "speculate/v1-lookup",
            ] {
                assert!(caps.iter().any(|c| c == cap), "faltou {cap} em {caps:?}");
            }
            assert_eq!(gossip["role"], "Principal");
            assert_eq!(gossip["hostname"], "macbook");
        }
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

    /// A NAT'd node with nothing pinned has, until now, only its own private
    /// LAN interfaces to offer — useless to a remote peer over gossipsub,
    /// where there is no packet source to rewrite `addr` with the way the
    /// UDP path does. A peer-observed public address has to fill that gap.
    #[tokio::test]
    async fn an_observed_public_address_becomes_the_announced_one() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let port = net.bind_addr.port();

        assert_eq!(
            net.announce_addr(),
            format!("0.0.0.0:{port}"),
            "no pin, nothing observed yet"
        );

        net.set_observed_addr("203.0.113.5:9999");

        assert_eq!(net.announce_addr(), "203.0.113.5:9999");
        assert!(
            net.self_addr_candidates().contains(&format!("203.0.113.5:{port}")),
            "the observed address's host must appear as a candidate too, at this node's own port"
        );
    }

    /// The operator's own explicit pin is a stronger signal than anything
    /// this node inferred about itself — an observed address arriving later
    /// (e.g. a new peer connects) must never override it.
    #[tokio::test]
    async fn a_pinned_address_always_outranks_an_observed_one() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap()
            .with_advertise_addr(Some("198.51.100.9:42777".into()));

        net.set_observed_addr("203.0.113.5:9999");

        assert_eq!(net.announce_addr(), "198.51.100.9:42777");
    }

    /// A peer on the same LAN, or a NAT that rewrote to another private
    /// range, is not evidence of a real public IP — must not overwrite the
    /// sentinel with something equally useless.
    #[tokio::test]
    async fn a_private_observed_address_is_ignored() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let port = net.bind_addr.port();

        net.set_observed_addr("192.168.1.50:9999");

        assert_eq!(net.announce_addr(), format!("0.0.0.0:{port}"));
    }

    /// `set_advertise_addr` is the live-update path the web UI's
    /// `POST /api/ui/network/advertise-addr` drives — no restart, unlike
    /// every other way this field could be set before. A later call has to
    /// actually override an earlier pin, since that's the entire point.
    #[tokio::test]
    async fn set_advertise_addr_overrides_a_previous_pin_live() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap()
            .with_advertise_addr(Some("198.51.100.9:42777".into()));

        assert_eq!(net.announce_addr(), "198.51.100.9:42777");

        net.set_advertise_addr(Some("203.0.113.77:42777"));

        assert_eq!(net.announce_addr(), "203.0.113.77:42777");
    }

    /// Clearing the pin (`None`, from the UI's "empty means auto" convention)
    /// has to fall back through the same precedence `announce_addr` already
    /// applies — back to an observed address if one exists, not straight to
    /// the `0.0.0.0` sentinel while a perfectly good observed address sits
    /// unused.
    #[tokio::test]
    async fn clearing_the_pin_falls_back_to_an_observed_address() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap()
            .with_advertise_addr(Some("198.51.100.9:42777".into()));
        net.set_observed_addr("203.0.113.5:9999");

        net.set_advertise_addr(None);

        assert_eq!(net.announce_addr(), "203.0.113.5:9999");
    }

    /// A malformed value passed to the live setter must not silently corrupt
    /// the previously-pinned address — `set_advertise_addr` normalizes the
    /// same way construction does, and an unparseable input normalizes to
    /// `None`, which (per the test above) means "cleared," not "ignored."
    /// The web UI's save endpoint is responsible for rejecting garbage
    /// *before* calling this — this test documents what happens if it didn't.
    #[tokio::test]
    async fn an_unparseable_value_clears_rather_than_corrupts() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let port = net.bind_addr.port();

        net.set_advertise_addr(Some(""));

        assert_eq!(net.announce_addr(), format!("0.0.0.0:{port}"));
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
        assert_eq!(advertised_addr("  ", 42777), None);
    }

    /// The address only helps if it survives the trip into a URL.
    #[test]
    fn an_advertised_ipv6_survives_into_the_peer_urls() {
        let peer = PeerInfo {
            node_id: "n".into(),
            pubkey: "p".into(),
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
            gateway_port: Some(8090),
            inference_frame_capabilities: Vec::new(),
            role: String::new(),
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            libp2p_peer_id: None,
        };
        assert_eq!(
            peer.inference_url().as_deref(),
            Some("http://[2804:d55:5579:1000:89f3:3f94:7c07:e183]:8181")
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
            node_id: "7b09e99c538b0f79daf0cf9111d9e5e912094c7c999273b8d2df3a446f6630ec".into(),
            pubkey: "a".repeat(64),
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
            gateway_port: Some(8090),
            inference_frame_capabilities: Vec::new(),
            role: "Principal".into(),
            hostname: "ksia".into(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 500000,
            disk_free_mb: 200000,
            api_keys_active: 0,
            libp2p_peer_id: None,
        };

        for count in [0usize, 1, 2, 3, 5, 12] {
            let chunks = peer_list_chunks(vec![peer.clone(); count]);
            let sent: usize = chunks.iter().map(|c| c.len()).sum();
            assert_eq!(sent, count, "nenhum peer pode sumir no fatiamento");
            for chunk in &chunks {
                let bytes = serde_json::to_vec(&P2PMessage::PeerList {
                    peers: chunk.clone(),
                })
                .unwrap()
                .len();
                assert!(
                    bytes <= MAX_DATAGRAM_PAYLOAD || chunk.len() == 1,
                    "datagrama de {bytes} bytes com {} peers passa do orcamento",
                    chunk.len()
                );
            }
        }
    }

    fn peer_com_candidatos(candidatos: Vec<&str>) -> PeerInfo {
        PeerInfo {
            node_id: "n1".into(),
            pubkey: "p".into(),
            addr: "192.168.1.9:42777".into(),
            addr_candidates: candidatos.into_iter().map(String::from).collect(),
            model: "qwen35".into(),
            start_layer: 0,
            end_layer: 19,
            total_layers: 40,
            memory_mb: 1.0,
            last_seen: Utc::now(),
            ollama_port: None,
            inference_port: Some(8181),
            inference_frame_capabilities: Vec::new(),
            gateway_port: Some(8080),
            role: "node".into(),
            hostname: "h".into(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            libp2p_peer_id: None,
        }
    }

    /// O endereco de origem do pacote e acrescentado, nao substitui: e o unico
    /// caminho comprovadamente funcional, mas nao e o unico que existe.
    /// Substituir era o que fazia dois nos da mesma LAN, que se conheceram pela
    /// internet, nunca descobrirem que tambem eram vizinhos.
    #[test]
    fn the_observed_address_joins_the_candidate_set() {
        let mut peer = peer_com_candidatos(vec!["192.168.1.9:42777"]);
        peer.merge_observed("[2804:f30::1]:42777");
        assert_eq!(
            peer.addr_candidates,
            vec!["192.168.1.9:42777".to_string(), "[2804:f30::1]:42777".to_string()],
        );
    }

    #[test]
    fn an_already_known_observed_address_is_not_duplicated() {
        let mut peer = peer_com_candidatos(vec!["192.168.1.9:42777"]);
        peer.merge_observed("192.168.1.9:42777");
        assert_eq!(peer.addr_candidates.len(), 1);
    }

    /// Um no rodando binario anterior nao manda `addr_candidates`. O campo cai
    /// no default e o `addr` unico dele vira o unico candidato: o cluster de
    /// versoes misturadas continua funcionando.
    #[test]
    fn a_peer_without_candidates_falls_back_to_its_single_addr() {
        let peer = peer_com_candidatos(vec![]);
        assert_eq!(peer.inference_candidates(), vec!["http://192.168.1.9:8181".to_string()]);
    }

    /// `inference_url()` costumava ler só `addr` — um valor que podia estar
    /// atrás do melhor endereço já disponível em `addr_candidates` (o pinado
    /// pelo operador, ou o público observado por `identify`). Painel e o
    /// campo de fallback de `PipelineStep` usam exatamente este método, então
    /// um `addr` desatualizado bastava para exibir/discar um IP privado
    /// mesmo com o endereço certo já conhecido.
    #[test]
    fn inference_url_prefers_the_first_candidate_over_the_legacy_addr() {
        let peer = peer_com_candidatos(vec!["203.0.113.5:42777", "192.168.1.9:42777"]);
        assert_eq!(peer.addr, "192.168.1.9:42777", "addr fica desatualizado de propósito neste teste");
        assert_eq!(peer.inference_url().as_deref(), Some("http://203.0.113.5:8181"));
    }

    /// Cada candidato vira uma URL de inferencia, com o IPv6 entre colchetes.
    #[test]
    fn every_candidate_becomes_an_inference_url() {
        let peer = peer_com_candidatos(vec!["192.168.1.9:42777", "[2804:f30::1]:42777"]);
        assert_eq!(
            peer.inference_candidates(),
            vec![
                "http://192.168.1.9:8181".to_string(),
                "http://[2804:f30::1]:8181".to_string(),
            ],
        );
    }

    /// Um no antigo manda Announce sem o campo novo. Se isso deixasse de
    /// parsear, o cluster partiria ao meio numa atualizacao parcial.
    #[test]
    fn an_announce_without_candidates_still_parses() {
        let antigo = r#"{"type":"Announce","node_id":"n","pubkey":"p","addr":"1.2.3.4:42777",
            "model":"m","start_layer":0,"end_layer":1,"total_layers":2,"memory_mb":1.0,
            "timestamp":"2026-08-13T00:00:00Z"}"#;
        let msg: P2PMessage = serde_json::from_str(antigo).expect("Announce antigo tem que parsear");
        match msg {
            P2PMessage::Announce { addr_candidates, .. } => assert!(addr_candidates.is_empty()),
            _ => panic!("esperava Announce"),
        }
    }

    /// Same shape of regression as `an_announce_without_candidates_still_parses`,
    /// specifically for `libp2p_peer_id`: a peer on a binary that predates the
    /// NAT-tunnel work sends `Announce` with no such field at all.
    #[test]
    fn an_announce_without_a_libp2p_peer_id_still_parses() {
        let antigo = r#"{"type":"Announce","node_id":"n","pubkey":"p","addr":"1.2.3.4:42777",
            "model":"m","start_layer":0,"end_layer":1,"total_layers":2,"memory_mb":1.0,
            "timestamp":"2026-08-13T00:00:00Z"}"#;
        let msg: P2PMessage =
            serde_json::from_str(antigo).expect("Announce sem libp2p_peer_id tem que parsear");
        match msg {
            P2PMessage::Announce { libp2p_peer_id, .. } => assert_eq!(libp2p_peer_id, None),
            _ => panic!("esperava Announce"),
        }
    }

    /// A `libp2p_peer_id` present on the wire has to survive the round trip
    /// unchanged — this is the one field a peer actually needs to dial the
    /// NAT-tunnel fallback, so a lossy encode/decode would silently break it.
    #[test]
    fn an_announce_with_a_libp2p_peer_id_round_trips() {
        let com_id = r#"{"type":"Announce","node_id":"n","pubkey":"p","addr":"1.2.3.4:42777",
            "model":"m","start_layer":0,"end_layer":1,"total_layers":2,"memory_mb":1.0,
            "timestamp":"2026-08-13T00:00:00Z",
            "libp2p_peer_id":"12D3KooWAbCdEfGhIjKlMnOpQrStUvWxYz1234567890AbCdEfGh"}"#;
        let msg: P2PMessage = serde_json::from_str(com_id).expect("Announce tem que parsear");
        match &msg {
            P2PMessage::Announce { libp2p_peer_id, .. } => assert_eq!(
                libp2p_peer_id.as_deref(),
                Some("12D3KooWAbCdEfGhIjKlMnOpQrStUvWxYz1234567890AbCdEfGh")
            ),
            _ => panic!("esperava Announce"),
        }
        let re_encoded = serde_json::to_string(&msg).unwrap();
        let re_decoded: P2PMessage = serde_json::from_str(&re_encoded).unwrap();
        match re_decoded {
            P2PMessage::Announce { libp2p_peer_id, .. } => assert_eq!(
                libp2p_peer_id.as_deref(),
                Some("12D3KooWAbCdEfGhIjKlMnOpQrStUvWxYz1234567890AbCdEfGh")
            ),
            _ => panic!("esperava Announce"),
        }
    }

    /// `PeerInfo` (the type stored after an `Announce` is accepted, and the
    /// one `PipelineStep` reads `libp2p_peer_id` from) needs the same
    /// backward-compatible default.
    #[test]
    fn a_peer_info_without_a_libp2p_peer_id_still_parses() {
        let antigo = r#"{"node_id":"n","pubkey":"p","addr":"1.2.3.4:42777","model":"m",
            "start_layer":0,"end_layer":1,"total_layers":2,"memory_mb":1.0,
            "last_seen":"2026-08-13T00:00:00Z"}"#;
        let peer: PeerInfo =
            serde_json::from_str(antigo).expect("PeerInfo sem libp2p_peer_id tem que parsear");
        assert_eq!(peer.libp2p_peer_id, None);
    }

    /// The LAN broadcast goes to `255.255.255.255:42777` no matter which port
    /// this node runs on, so a node brought up to try the interface out lands
    /// in whatever cluster is on the wire. Starting detached (`--no-discovery`)
    /// is the only way to run one safely next to a live cluster, and it only
    /// holds if neither the announce nor the bootstrap request goes out.
    #[tokio::test]
    async fn a_detached_node_neither_announces_nor_looks_for_bootstrap() {
        let net = Network::new(
            "n".into(),
            "p".into(),
            0,
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

    /// Uma sonda: sobe um socket UDP local fingindo ser um peer, e devolve o
    /// primeiro datagrama que chegar dentro do prazo.
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
            node_id: node_id.into(),
            pubkey: "p".into(),
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
            inference_frame_capabilities: Vec::new(),
            gateway_port: None,
            role: String::new(),
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            libp2p_peer_id: None,
        }
    }

    /// A descoberta transitiva era de tiro único: `PeerRequest` só saía no
    /// arranque e só para os bootstrap nodes. Fora da LAN (sem o broadcast IPv4
    /// para mascarar) um peer conhecido nunca era perguntado de novo, então os
    /// peers *dele* nunca eram aprendidos. O laço de gossip tem que voltar a
    /// perguntar, periodicamente, também aos peers vivos.
    #[tokio::test]
    async fn a_known_peer_is_asked_for_its_peers_periodically() {
        let (sock, addr) = probe().await;
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        net.peers
            .write()
            .await
            .insert("peer-a".into(), dummy_peer("peer-a", &addr));

        // Tique que cai numa varredura de descoberta.
        net.discovery_sweep(DISCOVERY_SWEEP_TICKS).await.unwrap();

        let msg = recv_msg(&sock, 500).await.expect("nada chegou no peer");
        assert!(
            matches!(msg, P2PMessage::PeerRequest { .. }),
            "esperava PeerRequest, veio {msg:?}"
        );
    }

    /// A varredura não pode perguntar a todo mundo em todo tique: isso é
    /// O(peers²) de PeerList. Cada tique de varredura fala com no máximo
    /// `SWEEP_FANOUT` peers, e o rodízio cobre o cluster inteiro ao longo do
    /// tempo.
    #[tokio::test]
    async fn the_sweep_rotates_over_a_bounded_subset() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
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
        // Percorre tiques suficientes para o rodízio dar a volta.
        for tick in 1..=(DISCOVERY_SWEEP_TICKS * 8) {
            let sent = net.discovery_sweep(tick).await.unwrap();
            if tick % DISCOVERY_SWEEP_TICKS == 0 {
                sweeps += 1;
                assert!(
                    sent <= SWEEP_FANOUT,
                    "varredura falou com {sent} peers, acima do limite {SWEEP_FANOUT}"
                );
            } else {
                assert_eq!(sent, 0, "tique fora da varredura não deve perguntar nada");
            }
        }
        assert_eq!(sweeps, 8);

        for (id, sock) in &probes {
            if recv_msg(sock, 50).await.is_some() {
                asked.insert(id.clone());
            }
        }
        assert_eq!(asked.len(), 8, "o rodízio tem que cobrir todos os peers");
    }

    /// Mesma guarda de `a_detached_node_neither_announces_nor_looks_for_bootstrap`,
    /// agora para o caminho novo.
    #[tokio::test]
    async fn a_detached_node_does_not_sweep() {
        let (sock, addr) = probe().await;
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        net.peers
            .write()
            .await
            .insert("peer-a".into(), dummy_peer("peer-a", &addr));
        net.detach().await;

        assert_eq!(net.discovery_sweep(DISCOVERY_SWEEP_TICKS).await.unwrap(), 0);
        assert!(recv_msg(&sock, 300).await.is_none(), "nó destacado falou");
    }

    #[tokio::test]
    async fn ipv6_destinations_are_left_alone() {
        let net = Network::new("n".into(), "p".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let v6: SocketAddr = "[fe80::1]:42777".parse().unwrap();
        assert_eq!(net.normalize(v6), v6);
    }

    /// O nó liga em `[::]` e precisa receber de um peer IPv4 - que é como toda
    /// a LAN fala. Desligar `IPV6_V6ONLY` depois do bind não tem efeito, e no
    /// Windows o padrão é ligado: sem este socket montado à mão, um nó lá não
    /// veria anúncio nenhum e a descoberta ficaria muda sem dizer por quê.
    #[tokio::test]
    async fn socket_dual_stack_recebe_de_remetente_ipv4() {
        let alvo: SocketAddr = "[::]:0".parse().unwrap();
        let Ok(std_sock) = Network::bind_udp_dual_stack(alvo) else {
            return; // máquina sem IPv6: o caminho v4 puro cobre esse caso
        };
        let port = std_sock.local_addr().unwrap().port();
        let sock = UdpSocket::from_std(std_sock).unwrap();

        let emissor = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        emissor.send_to(b"oi", ("127.0.0.1", port)).await.unwrap();

        let mut buf = [0u8; 8];
        let recebido = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sock.recv_from(&mut buf),
        )
        .await
        .expect("nada chegou de um remetente IPv4 no socket [::]")
        .unwrap();

        assert_eq!(&buf[..recebido.0], b"oi");
    }
}
