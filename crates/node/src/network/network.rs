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

#[cfg(not(test))]
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
const ADMISSION_TIMEOUT: Duration = Duration::from_millis(200);

/// A sonda pelo túnel pode ter de montar um circuito por um relé na internet
/// antes de mandar o pedido, e roda fora do laço do swarm
/// (`admit_in_background`): não precisa do orçamento curto das diretas.
#[cfg(not(test))]
const ADMISSION_TUNNEL_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
const ADMISSION_TUNNEL_TIMEOUT: Duration = Duration::from_millis(500);

const ADMISSION_CACHE_TTL: Duration = Duration::from_secs(30);
const REJECTED_ADMISSION_CACHE_TTL: Duration = Duration::from_secs(5);

/// Quanto tempo um peer que já provou pertencimento continua admitido enquanto
/// as sondas seguintes não chegam a ele. Um par que só o relé alcança perde a
/// rota a cada circuito que cai; sem esta folga, uma sonda nesse intervalo o
/// tirava da tabela (`Removed timed-out peers`) e ele voltava minutos depois.
/// Recusa explícita não usa a folga.
const ADMISSION_UNREACHABLE_GRACE: Duration = Duration::from_secs(10 * 60);

/// Uma conexão libp2p viva com um par.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Libp2pLink {
    /// `host:porta (transporte)` da outra ponta; num circuito, o do relé.
    /// Vazio quando o circuito entrou por um relé que este nó não conhece.
    pub address: String,
    pub relayed: bool,
    /// PeerId do relé, quando a conexão é um circuito.
    pub relay_peer_id: Option<String>,
    /// Conexão direta nascida de um hole punch (DCuTR) bem-sucedido.
    pub hole_punched: bool,
}

/// Por onde este nó fala de fato com um peer - observado, não anunciado.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PeerRoute {
    /// `http`: um endereço direto respondeu há pouco (`RouteTable::last_success`).
    /// `p2p_direct`: conexão libp2p direta, discada ou por hole punch.
    /// `relayed`: só um circuito pelo relé - o hole punch não fechou.
    /// `none`: nenhuma das três.
    pub kind: &'static str,
    /// O endereço da rota; em `relayed`, o do relé.
    pub address: Option<String>,
    /// Alguma conexão direta com o par saiu de um hole punch.
    pub hole_punched: bool,
    /// Todas as conexões libp2p vivas, para o detalhe da tela.
    pub links: Vec<Libp2pLink>,
}

/// Host de `http(s)://host:porta[/...]`, com os colchetes do IPv6, para
/// comparar endereços do mesmo peer em portas diferentes.
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

/// `host:porta (transporte)` de um multiaddress, e o relé quando é circuito.
///
/// Num circuito o host e a porta são os do relé, que vêm antes de
/// `/p2p-circuit`; o que vem depois é o próprio par.
pub(crate) fn describe_multiaddr(address: &libp2p::Multiaddr) -> (String, bool, Option<String>) {
    use libp2p::multiaddr::Protocol;
    let mut host: Option<String> = None;
    let mut port: Option<(u16, &'static str)> = None;
    let mut last_peer: Option<String> = None;
    let mut relay: Option<String> = None;
    let mut relayed = false;
    for protocol in address.iter() {
        if relayed {
            continue;
        }
        match protocol {
            Protocol::Ip4(ip) => host = Some(ip.to_string()),
            Protocol::Ip6(ip) => host = Some(format!("[{ip}]")),
            Protocol::Dns(name) | Protocol::Dns4(name) | Protocol::Dns6(name) => {
                host = Some(name.to_string())
            }
            Protocol::Tcp(p) => port = Some((p, "TCP")),
            Protocol::Udp(p) => port = Some((p, "UDP")),
            Protocol::QuicV1 => port = port.map(|(p, _)| (p, "QUIC")),
            Protocol::P2p(peer) => last_peer = Some(peer.to_string()),
            Protocol::P2pCircuit => {
                relayed = true;
                relay = last_peer.take();
            }
            _ => {}
        }
    }
    let text = match (host, port) {
        (Some(host), Some((port, transport))) => format!("{host}:{port} ({transport})"),
        (Some(host), None) => host,
        _ => String::new(),
    };
    (text, relayed, relay)
}

/// Resultado de uma sonda `/handshake`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionProbe {
    Approved,
    /// O próprio peer disse não: pelo túnel (PeerId autenticado) ou por HTTPS
    /// com o certificado fixado no `node_id`. Uma resposta HTTP em claro não
    /// conta - qualquer máquina naquele endereço poderia tê-la dado.
    Refused,
    /// Nenhuma resposta autêntica chegou.
    Unreachable,
}

/// De quanto em quanto tempo, no máximo, se repergunta o endereço de um peer
/// ilegível.
///
/// O `Announce` sai a cada `GOSSIP_INTERVAL_SECS`, então uma janela de 30 s dá
/// tempo de a resposta chegar e ainda mantém o Painel, que relê o cluster em
/// ciclo curto, longe de virar gerador de tráfego UDP.
pub const REDISCOVERY_INTERVAL: Duration = Duration::from_secs(30);

/// Até quando a tela pode dizer "reencontrando".
///
/// Três perguntas à rede sem resposta não são mais trabalho em curso: são um
/// peer que anuncia por UDP e não atende no HTTP. Chamar isso de "reencontrando"
/// para sempre seria a mesma desonestidade do aviso permanente que este trabalho
/// veio remover, só com outra palavra - a partir daqui a tela volta a dizer que
/// não consegue ler o par, e diz por quê.
pub const REDISCOVERY_GIVE_UP: Duration = Duration::from_secs(90);

/// Identidade de um nó num grupo de tensor parallelism (caminho C).
///
/// `role` é `"primary"` (dono do GGUF e hop do pipeline) ou `"worker"`
/// (compartilha o compute daquele estágio). `group_id` identifica o grupo;
/// `size`/`rank` dizem quantos somos e qual é a minha posição. Um nó sem
/// TP anuncia `None`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TpIdentity {
    pub role: String,
    pub group_id: String,
    pub size: u32,
    pub rank: u32,
    /// URL do primary (rank 0) da coletiva, por onde todos os ranks fazem o
    /// POST /v1/tp/reduce. O deploy preenche para todos os membros do grupo.
    #[serde(default)]
    pub rank0_url: Option<String>,
    /// `node_id` do primary, que é **o que pina o certificado** do cliente da
    /// coletiva (`SHA256(pubkey) == node_id`).
    ///
    /// Sem isto o `tp.rs` usava um `reqwest::Client::new()` padrão contra um
    /// `rank0_url` https com certificado autoassinado: todo POST da coletiva
    /// morria em 13 ms por certificado inválido, o erro era engolido, e a
    /// parcial seguia como se fosse a soma (18/08).
    #[serde(default)]
    pub rank0_node_id: Option<String>,
    /// Todos os membros do grupo, **indexados pelo rank** (0 = primary).
    ///
    /// É o que permite o rank 0 pôr os workers para rodar o mesmo decode: TP
    /// exige que todos os ranks executem o mesmo grafo sobre os mesmos tokens
    /// ao mesmo tempo, e sem esta lista o rank 0 não tem para quem transmitir.
    /// Sem lockstep as parciais nunca se encontram na barreira e o decode
    /// trava - o estado de 18/08.
    #[serde(default)]
    pub members: Vec<TpMember>,
}

/// Um membro de um grupo de tensor parallelism.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TpMember {
    /// Pina o certificado deste membro (`SHA256(pubkey) == node_id`).
    pub node_id: String,
    /// Base da URL do servidor de inferência (onde o decode é disparado).
    pub url: String,
    /// Endereços alternativos do servidor de inferência. O primeiro é mantido
    /// em `url` para o formato antigo; quem disca tenta os demais quando o
    /// endereço preferido não é alcançável a partir deste rank.
    #[serde(default)]
    pub url_candidates: Vec<String>,
    /// Endereços UDP anunciados pelo membro, para o túnel reconstruir as
    /// multiaddresses TCP mesmo quando o swarm local ainda não viu o anúncio.
    #[serde(default)]
    pub addr_candidates: Vec<String>,
    /// PeerId usado pelo fallback de transporte libp2p.
    #[serde(default)]
    pub libp2p_peer_id: Option<String>,
    /// O membro anunciou que seu swarm atende o túnel de inferência.
    #[serde(default)]
    pub supports_tunnel: bool,
}

/// Capacidade efetiva anunciada por um servidor de inferência pronto.
///
/// `max_new_tokens` é o tamanho de uma perna do backend. `n_ctx` é a janela
/// efetiva que limita a soma de prompt e saída neste peer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCapacity {
    pub n_ctx: u32,
    pub max_new_tokens: u32,
}

/// O que este nó sabe sobre a própria travessia de NAT.
///
/// Existe porque diagnosticar "os dois nós não se falam" dependia de
/// `RUST_LOG=debug` na máquina certa no momento certo. Estado do AutoNAT,
/// reservas de circuito e resultado do DCuTR são exatamente o que diz em que
/// degrau a travessia parou.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraversalStatus {
    /// `unknown`, `public` ou `private`, como o AutoNAT reportou.
    pub nat: String,
    /// Multiaddresses que este nó publica hoje (inclui circuito).
    pub addrs: Vec<String>,
    /// `PeerId` dos relés que aceitaram reserva.
    pub reservations: Vec<String>,
    /// Hole punches concluídos e falhados desde o arranque.
    pub hole_punch_ok: u64,
    pub hole_punch_failed: u64,
    /// Última falha de DCuTR, para dizer *por que* parou.
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
    /// Última aprovação de verdade, para `ADMISSION_UNREACHABLE_GRACE`.
    proven_at: Option<Instant>,
}

/// O que entra no cache depois de uma sonda. Aprovado vale por
/// `ADMISSION_CACHE_TTL`; recusado sai. Sem resposta, um peer aprovado há menos
/// de `ADMISSION_UNREACHABLE_GRACE` continua admitido, com validade curta para
/// a sonda tentar de novo logo; sem prova recente, sai.
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
        pubkey: String,
        /// Public key of the libp2p identity that signed the gossip message.
        /// It is authority material, not the legacy node identity key.
        #[serde(default)]
        authority_pubkey: String,
        addr: String,
        /// Todos os enderecos deste no. `serde(default)` mantem um no antigo
        /// parseavel: sem candidatos, o `addr` unico continua valendo.
        #[serde(default)]
        addr_candidates: Vec<String>,
        model: String,
        /// A implantação que este nó serve. Vazio num peer de versão anterior,
        /// que faz o agrupamento cair de volta no rótulo do modelo.
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
        /// Se este nó fala TLS com os peers.
        ///
        /// **Gatilho, não valor**: quem recebe não grava isto. Ver
        /// `tls_policy` - o gossip é em claro e não autenticado, então obedecer
        /// a ele seria oferecer o rebaixamento que o TLS existe para impedir.
        #[serde(default)]
        tls: bool,
        /// Gatilho da catraca autenticada da política QUIC.
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
        /// This node's libp2p `PeerId` (base58), when a stable one could be
        /// derived (see `Discovery::new`). `serde(default)` keeps an older
        /// peer parseable; absent means this node can't be reached over the
        /// libp2p NAT-tunnel fallback (`transport/libp2p-tunnel-v1`).
        #[serde(default)]
        libp2p_peer_id: Option<String>,
        /// Multiaddresses libp2p que este nó publica, já com `/p2p/<peer-id>`.
        ///
        /// É a única forma capaz de expressar um circuito de relay
        /// (`.../p2p/<relé>/p2p-circuit/p2p/<self>`): a convenção `host:port`
        /// com porta `UDP+10000` não tem como carregar um, e era por isso que
        /// uma reserva aceita nunca chegava ao outro lado e o DCuTR nunca
        /// tinha conexão relayed para coordenar o hole punch. Vazio num peer
        /// antigo, e aí quem disca volta a derivar o endereço do `addr`.
        #[serde(default)]
        libp2p_addrs: Vec<String>,
        /// Identidade de tensor parallelism, quando este nó é primary/worker de
        /// um grupo. Ausente = nó sem TP (o caso de hoje).
        #[serde(default)]
        tp: Option<TpIdentity>,
        /// Capacidade presente somente depois de `/health` provar que o
        /// modelo carregou. Ausente em anúncios antigos ou durante o boot.
        #[serde(default)]
        runtime_capacity: Option<RuntimeCapacity>,
        /// Gatilho para a catraca autenticada da coleta de logs.
        #[serde(default)]
        cluster_logs_enabled: bool,
        /// Modelo descarregado por idle, mantendo a implantação anunciada.
        #[serde(default)]
        idle: bool,
        /// Gatilho para a catraca autenticada do Chat da rede.
        #[serde(default)]
        network_chat_enabled: bool,
        /// Identificador da cadeia de confiança do enxame público.
        /// O valor sozinho não autentica o peer.
        #[serde(default)]
        cluster_id: Option<String>,
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
    pub pubkey: String,
    #[serde(default)]
    pub authority_pubkey: String,
    /// Cadeia de confiança alegada pelo peer. Só é aceita junto com a tupla
    /// de autoridade validada localmente.
    #[serde(default)]
    pub cluster_id: Option<String>,
    pub addr: String,
    /// Todos os enderecos em que este peer pode ser encontrado.
    ///
    /// `addr` continua sendo o preferido historico e o unico que um no antigo
    /// entende. Este conjunto e o que permite a quem vai discar escolher, em
    /// vez de aceitar a escolha de quem planejou de outro lugar da rede.
    #[serde(default)]
    pub addr_candidates: Vec<String>,
    pub model: String,
    /// A implantação que este nó serve. É por ele que `plan_pipeline` agrupa,
    /// e é o que permite o mesmo modelo estar no ar duas vezes em conjuntos de
    /// máquinas diferentes. Vazio num peer de versão anterior, e aí o
    /// agrupamento cai de volta no rótulo. Ver `models/deployment.rs`.
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
    /// Ver `P2PMessage::Announce::tls`: gatilho para a catraca, nunca valor.
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
    /// See `P2PMessage::Announce::libp2p_peer_id`.
    #[serde(default)]
    pub libp2p_peer_id: Option<String>,
    /// Ver `P2PMessage::Announce::libp2p_addrs`.
    #[serde(default)]
    pub libp2p_addrs: Vec<String>,
    /// Ver `P2PMessage::Announce::tp`.
    #[serde(default)]
    pub tp: Option<TpIdentity>,
    /// Capacidade do runtime depois que o servidor de inferência ficou pronto.
    /// `None` não é zero: significa que a capacidade ainda não foi provada.
    #[serde(default)]
    pub runtime_capacity: Option<RuntimeCapacity>,
    /// O peer está vivo e anunciado, mas sem os pesos na memória.
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
    pinned_host: Option<&str>,
    observed_addr: Option<&str>,
    local: Vec<crate::addrs::LocalAddr>,
) -> Vec<String> {
    let local = crate::addrs::filter_candidates(local, None);
    let mut preferred = Vec::with_capacity(local.len() + 2);

    if let Some(pinned) = pinned_host {
        preferred.push(pinned.to_string());
        preferred.extend(local);
        return deduplicate_hosts(preferred);
    }

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
    // An explicit pin is an operator contract, not just a preference. Keep it
    // exclusive so the UDP announce, libp2p tunnel address book, and gateway
    // replies cannot leak LAN, Docker, or stale interface addresses alongside
    // the configured Tailscale endpoint.
    if let Some(pinned) = pinned {
        return (pinned.clone(), vec![pinned]);
    }
    let addr = choose_announce_addr(None, observed.clone(), local.clone(), port);
    let candidates =
        local_candidates_by_preference(pinned.as_deref().map(ip_host), observed.as_deref(), local)
            .into_iter()
            .map(|ip| host_port(&ip, port))
            .collect();
    (addr, candidates)
}

impl PeerInfo {
    pub fn layer_range(&self) -> String {
        format!("{}-{}", self.start_layer, self.end_layer)
    }

    pub fn layers_count(&self) -> u32 {
        self.end_layer - self.start_layer + 1
    }

    /// Este peer é um worker de tensor parallelism (não é hop do pipeline)?
    /// O planejador exclui workers dos candidatos: só o primary de um grupo
    /// entra na cadeia.
    pub fn is_tp_worker(&self) -> bool {
        self.tp.as_ref().is_some_and(|t| t.role == "worker")
    }

    pub fn is_alive(&self) -> bool {
        let now = Utc::now();
        let diff = now.signed_duration_since(self.last_seen);
        diff.num_seconds() < PEER_TIMEOUT_SECS
    }

    /// Full Ollama HTTP URL for this peer (None if no ollama_port advertised).
    ///
    /// **Continua em `http://` mesmo com o cluster em TLS, de propósito**: o
    /// Ollama é um servidor de terceiros rodando na máquina do peer, não um nó
    /// deste cluster. Ele não tem o nosso certificado nem a nossa identidade, e
    /// pedir TLS a ele só produziria conexão recusada.
    pub fn ollama_url(&self) -> Option<String> {
        self.ollama_port
            .map(|port| format!("http://{}:{}", ip_host(&self.addr), port))
    }

    /// Acrescenta o endereco de onde o pacote realmente veio quando a origem
    /// ainda e a unica pista. Um `Announce` com `addr_candidates` explicitos
    /// continua sendo a autoridade, para nao reintroduzir LAN ou bridge junto
    /// de um `advertise_addr` fixado.
    pub fn merge_observed(&mut self, observado: &str) {
        if !self.addr_candidates.iter().any(|c| c == observado) {
            self.addr_candidates.push(observado.to_string());
        }
    }

    pub fn dialable_addr(&self) -> Option<&str> {
        self.addr_candidates
            .iter()
            .find(|addr| is_valid_destination(addr))
            .map(String::as_str)
            .or_else(|| is_valid_destination(&self.addr).then_some(self.addr.as_str()))
    }

    /// Todos os destinos UDP conhecidos, sem transformar o primeiro endereço
    /// anunciado em verdade permanente. A ordem mantém os candidatos
    /// anunciados e usa `addr` como fallback para peers antigos ou para um
    /// snapshot em que ele já foi atualizado.
    pub fn dialable_addrs(&self) -> Vec<String> {
        let mut addresses = Vec::with_capacity(self.addr_candidates.len() + 1);
        for address in self
            .addr_candidates
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.addr.as_str()))
        {
            if is_valid_destination(address) && !addresses.iter().any(|known| known == address) {
                addresses.push(address.to_string());
            }
        }
        addresses
    }

    /// Canonical libp2p endpoint for a peer. Libp2p identify exposes every
    /// listen interface, while the pipeline must dial the endpoint selected
    /// by the peer's Announce. Keep the multi-address fallback policy for UDP
    /// control traffic, but use one announced endpoint for the tunnel.
    /// Endereços que o swarm pode discar para alcançar este peer.
    ///
    /// Os multiaddresses anunciados vêm primeiro e são devolvidos **todos**:
    /// é onde mora o endereço de circuito, e devolver só o primeiro endereço
    /// deixava um peer atrás de NAT sem nenhum destino alcançável. A derivação
    /// por `addr`/`addr_candidates` fica como compatibilidade com um peer que
    /// ainda não publica `libp2p_addrs`.
    pub fn libp2p_dialable_addrs(&self) -> Vec<String> {
        if !self.libp2p_addrs.is_empty() {
            return self.libp2p_addrs.clone();
        }
        is_valid_destination(&self.addr)
            .then_some(self.addr.as_str())
            .or_else(|| {
                self.addr_candidates
                    .iter()
                    .find(|address| is_valid_destination(address))
                    .map(String::as_str)
            })
            .map(str::to_owned)
            .into_iter()
            .collect()
    }

    /// URLs do servidor de inferencia, uma por candidato.
    ///
    /// Cai no `addr` unico quando o peer nao anunciou candidatos, que e o caso
    /// de um no rodando binario anterior.
    pub fn inference_candidates(&self) -> Vec<String> {
        let Some(porta) = self.inference_port else {
            return Vec::new();
        };
        let base = self
            .addr_candidates
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.addr.as_str()));
        let mut urls = Vec::with_capacity(self.addr_candidates.len() + 1);
        for address in base {
            if !is_valid_destination(address) {
                continue;
            }
            let url = format!(
                "{}://{}:{}",
                crate::peer_client::peer_scheme(),
                ip_host(address),
                porta
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

    /// Endereços `host:porta` do endpoint QUIC de inferência, construídos a
    /// partir do endereço público anunciado pelo peer e da porta UDP dedicada.
    /// Este caminho não lê nem faz parse de `inference_url`.
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
    /// É o **primeiro** candidato, não o único. Quem tem como tentar mais de
    /// um endereço deve chamar `gateway_candidates()` e consultar a memória de
    /// falha (`RouteTable::usable`): um endereço que deixou de responder não
    /// pode ser o fim da linha do plano de controle só porque é o preferido.
    pub fn gateway_url(&self) -> Option<String> {
        self.gateway_candidates().into_iter().next()
    }

    /// URLs do gateway, uma por endereço conhecido, em ordem de preferência.
    ///
    /// `addr` vem primeiro porque é **fato conhecido**: é o endereço de onde os
    /// pacotes deste peer chegaram, ou o que ele anunciou de si. Os demais
    /// candidatos vêm depois, na ordem em que o peer os anunciou. É a mesma
    /// disciplina de `pipeline::resolve_step_url` - palpite não passa na frente
    /// de fato conhecido, porque `merge_observed` acrescenta o observado ao
    /// **fim** da lista e o primeiro candidato costuma ser a LAN do peer.
    ///
    /// Ter mais de uma entrada aqui é o que separa esta lista do que ela era:
    /// enquanto `gateway_url()` era só `addr`, um endereço que morria deixava o
    /// Painel, o `deploy` e a transferência sem para onde cair, mesmo com um
    /// endereço vivo já anunciado em `addr_candidates`.
    pub fn gateway_candidates(&self) -> Vec<String> {
        self.gateway_candidates_with_tls(crate::peer_client::tls_ativo())
    }

    /// URLs do gateway usando explicitamente o esquema de um anúncio.
    ///
    /// As catracas de TLS, logs e Chat consultam o principal antes de adotar o
    /// valor anunciado localmente, então não podem depender da flag global que
    /// ainda está em processo de confirmação.
    pub fn gateway_candidates_with_tls(&self, tls: bool) -> Vec<String> {
        let Some(porta) = self.gateway_port else {
            return Vec::new();
        };
        let mut base: Vec<&str> = vec![self.addr.as_str()];
        base.extend(self.addr_candidates.iter().map(String::as_str));
        let mut urls: Vec<String> = Vec::with_capacity(base.len());
        for a in base {
            if !is_valid_destination(a) {
                continue;
            }
            let url = crate::peer_client::peer_base_url(&ip_host(a), porta, tls);
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
    /// Ed25519 public key of the libp2p identity used by gossipsub.
    pub authority_pubkey: Arc<std::sync::RwLock<String>>,
    pub bind_addr: SocketAddr,
    /// Destino do anúncio local. Em produção é o broadcast IPv4 global; o
    /// testkit injeta um destino loopback para nunca alcançar a LAN.
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
    /// Anúncios públicos que ainda não provaram pertencimento à rede local.
    ///
    /// Esta tabela é deliberadamente separada de `peers`: candidatos públicos
    /// podem ser consultados pelo diretório e pelo pkarr, mas nunca entram no
    /// Painel, no planejamento de pipeline ou em qualquer superfície de
    /// controle de membro antes da admissão normal.
    public_discovery: Arc<RwLock<HashMap<String, PeerInfo>>>,
    /// Advertised in every `Announce` so peers can name this machine without a
    /// second HTTP round-trip.
    pub profile: Arc<std::sync::RwLock<crate::profile::NodeProfile>>,
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
    /// Identidade de tensor parallelism deste nó (caminho C). `None` fora de
    /// um grupo TP. Mutável como `advertise_addr`: o `apply` pode ligar o
    /// nó como primary/worker sem reiniciar o processo.
    tp: std::sync::RwLock<Option<TpIdentity>>,
    /// Capacidade que o último `/health` local confirmou. Fit de memória pode
    /// reduzir o contexto efetivo depois que o llama.cpp carrega o GGUF.
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
    /// Por onde este no fala com cada peer, medido por este processo.
    pub routes: Arc<crate::route::RouteTable>,
    /// RTT direcional deste nó para cada peer, medido pelo transporte de
    /// inferência libp2p quando disponível.
    pub rtts: Arc<crate::route::RttTable>,
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
    /// O servidor de inferência carregou um modelo e está com o scheduler
    /// multi-item ativo. Fica falso durante setup, carregamento, TP e queda;
    /// anunciar a capability antes disso faria um peer enviar v3 para uma
    /// porta que só sabe responder o protocolo antigo.
    batching_ready: Arc<AtomicBool>,
    /// Catraca de admissão. Ausente apenas no `testkit` legado; processos
    /// reais sempre a configuram antes de iniciar a Network.
    admission: Option<Arc<PeerAdmission>>,
    admission_cache: std::sync::RwLock<HashMap<String, AdmissionCacheEntry>>,
    /// Peers com sonda de admissão em andamento fora do laço do swarm (ver
    /// `admit_in_background`), para um anúncio a cada 5 s não empilhar sondas.
    pending_admissions: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Conexões libp2p vivas por PeerId, alimentadas pelo laço do swarm
    /// (`discovery.rs`). É o que diz se um par é alcançado direto ou só pelo
    /// relé - ver `peer_route`.
    libp2p_links:
        std::sync::RwLock<HashMap<String, Vec<(libp2p::swarm::ConnectionId, Libp2pLink)>>>,
    /// Conexões que o DCuTR anunciou como fruto de hole punch. O evento pode
    /// chegar antes ou depois do `ConnectionEstablished` da mesma conexão,
    /// então a marca fica guardada até a conexão fechar.
    hole_punched_links: std::sync::Mutex<std::collections::HashSet<libp2p::swarm::ConnectionId>>,
    /// Este nó tem `slot_save_path` configurado e serve `POST
    /// /v1/session/slot`. Vira verdadeiro no `apply` de uma implantação e volta
    /// a falso ao parar; o gateway exige a cap de todos os hops antes de
    /// orquestrar um snapshot de implantação.
    slot_persist_ready: Arc<AtomicBool>,
    /// Se este nó fala TLS com os peers, na forma que o anúncio lê.
    ///
    /// A fonte de verdade é `Config.tls_required`; isto é a cópia que o gossip
    /// e o laço de accept consultam sem tocar no lock da configuração.
    tls: Arc<AtomicBool>,
    /// Por onde a catraca é acionada: recebe o node_id do principal quando um
    /// anúncio dele discorda da política local.
    ///
    /// É um canal, e não uma chamada direta, porque o `Network` não conhece a
    /// configuração nem a chave de conexão - quem sabe buscar e gravar é a task
    /// montada no `main.rs`. Ver `tls_policy`.
    tls_probe: std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    /// Canal da catraca autenticada da política QUIC.
    quic_probe: std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    /// Canal da catraca autenticada da política de logs.
    cluster_logs_probe:
        std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    /// Política local de captura de logs. O valor do anúncio é gatilho, nunca
    /// é adotado diretamente.
    cluster_logs_enabled: Arc<AtomicBool>,
    /// Política local do canal global de conversa. O booleano anunciado é
    /// apenas gatilho; a fonte é a leitura autenticada no principal.
    network_chat_probe:
        std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    network_chat_enabled: Arc<AtomicBool>,
    /// A UI de um nó precisa do documento completo de `NetworkSettings`, mas
    /// o diretório público só carrega o subconjunto necessário para entrar na
    /// rede. Este flag faz o primeiro Announce autenticado do principal
    /// disparar a busca do documento completo.
    network_settings_probe:
        std::sync::RwLock<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
    network_settings_sync_needed: AtomicBool,
    /// Public key of the authority whose authenticated policy is authoritative.
    /// The profile in an Announce is not enough: a secondary admin also has
    /// `profile: admin`, but must never become a policy source.
    policy_principal_pubkey: std::sync::RwLock<Option<String>>,
    /// Evita repetir no gossip um aviso permanente, como o broadcast global
    /// indisponível dentro de um container.
    broadcast_warning_logged: AtomicBool,
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
    /// Multiaddresses que o swarm libp2p publica hoje, mantidas por
    /// `Discovery` (ver `set_libp2p_addrs`). Vivem aqui, e não no `Discovery`,
    /// porque quem monta o `Announce` é o `Network`.
    libp2p_addrs: std::sync::RwLock<Vec<String>>,
    /// Diagnóstico da travessia, mantido por `Discovery`. Ver `TraversalStatus`.
    traversal: std::sync::RwLock<TraversalStatus>,
    /// Peers cujo endereço este nó está a reencontrar. Ver
    /// `request_peer_addresses`.
    rediscovery: std::sync::RwLock<HashMap<String, Rediscovery>>,
}

/// Uma busca em curso pelo endereço atual de um peer.
struct Rediscovery {
    /// Quando a busca começou. É o que decide até quando a tela pode dizer
    /// "reencontrando" (`REDISCOVERY_GIVE_UP`).
    desde: Instant,
    /// Quando a última pergunta saiu, para não repetir antes de
    /// `REDISCOVERY_INTERVAL`.
    ultima_pergunta: Instant,
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
pub(crate) fn short(id: &str) -> &str {
    let mut fim = id.len().min(12);
    while fim > 0 && !id.is_char_boundary(fim) {
        fim -= 1;
    }
    &id[..fim]
}

const PANIC_LOG_INTERVAL: StdDuration = StdDuration::from_secs(10);

/// Limita a frequência dos avisos quando uma entrada defeituosa faz o handler
/// entrar em pânico em todos os datagramas. O primeiro pânico é sempre visível;
/// os seguintes são contados e aparecem no próximo aviso permitido.
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
                    info!("UDP socket bound to [::]:{} (dual-stack)", bind_addr.port());
                    (socket, bind_addr)
                }
                Err(e) => {
                    info!("{}", e);
                    let v4: SocketAddr = format!("0.0.0.0:{}", port).parse()?;
                    let s = UdpSocket::bind(v4).await?;
                    s.set_broadcast(true)?;
                    let bind_addr = s.local_addr()?;
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
            return Err(anyhow::anyhow!("o testkit UDP só aceita destinos loopback"));
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
            authority_pubkey: Arc::new(std::sync::RwLock::new(String::new())),
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
            hostname: String::new(),
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
            libp2p_peer_id: None,
            libp2p_tunnel_ready: AtomicBool::new(false),
            mtp_ready: Arc::new(AtomicBool::new(false)),
            batching_ready: Arc::new(AtomicBool::new(false)),
            admission: None,
            admission_cache: std::sync::RwLock::new(HashMap::new()),
            pending_admissions: std::sync::Mutex::new(std::collections::HashSet::new()),
            libp2p_links: std::sync::RwLock::new(HashMap::new()),
            hole_punched_links: std::sync::Mutex::new(std::collections::HashSet::new()),
            slot_persist_ready: Arc::new(AtomicBool::new(false)),
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
            broadcast_warning_logged: AtomicBool::new(false),
            observed_addr: std::sync::RwLock::new(None),
            libp2p_addrs: std::sync::RwLock::new(Vec::new()),
            traversal: std::sync::RwLock::new(TraversalStatus::new()),
            rediscovery: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Label this node carries in its announces.
    pub fn with_identity(mut self, profile: crate::profile::NodeProfile, hostname: String) -> Self {
        *self.profile.write().expect("profile lock") = profile;
        self.hostname = hostname;
        self
    }

    /// Configura a prova de pertencimento antes de iniciar as tasks de
    /// descoberta. Um processo em produção nunca usa o modo aberto do
    /// `testkit`: sem chave privada válida ou cadeia pública ancorada, a
    /// admissão recusa o peer.
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
            anyhow::bail!("a descoberta local exige a admissão do nó");
        };
        if self.profile() != crate::profile::NodeProfile::Admin {
            anyhow::bail!("somente o administrador pode habilitar a descoberta local");
        }
        let cfg = admission.auth.read().await;
        let trust = admission.trust.document();
        if trust.setup.is_none() {
            anyhow::bail!(
                "a descoberta local exige os dados iniciais da rede no documento de confiança"
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
            .begin_search(Some(crate::local_discovery::DiscoveredMachine::from_system(
                &crate::sysmon::sample().await,
                Some(self.hostname.clone()),
            )))
            .await
    }

    pub(crate) fn local_discovery(&self) -> Arc<crate::local_discovery::LocalDiscoveryRuntime> {
        self.local_discovery.clone()
    }

    pub fn with_authority_pubkey(self, pubkey: String) -> Self {
        *self.authority_pubkey.write().expect("authority key lock") = pubkey;
        self
    }

    pub fn authority_pubkey(&self) -> String {
        self.authority_pubkey
            .read()
            .expect("authority key lock")
            .clone()
    }

    /// Change the profile announced by this process after a local mutation.
    pub fn set_profile(&self, profile: crate::profile::NodeProfile) {
        *self.profile.write().expect("profile lock") = profile;
    }

    pub fn profile(&self) -> crate::profile::NodeProfile {
        *self.profile.read().expect("profile lock")
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

    /// Configura as portas UDP dedicadas do HTTP/3 antes de o nó entrar no
    /// cluster. Elas são anunciadas junto do endereço público, nunca
    /// inferidas a partir da URL TCP de inferência.
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
            info!("Anunciando o endereço fixo {a} (em vez do endereço de origem)");
        }
        *self.advertise_addr.write().expect("advertise_addr lock") = normalized;
    }

    /// Liga (ou desliga, com `None`) a identidade de tensor parallelism deste
    /// nó. O `apply` chama isto quando implanta o nó como primary/worker de
    /// um grupo TP, sem reiniciar o processo.
    pub fn set_tp(&self, tp: Option<TpIdentity>) {
        *self.tp.write().expect("tp lock") = tp;
    }

    /// Identidade de tensor parallelism anunciada por este nó, se houver.
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

    /// Record a peer's libp2p `identify` response as evidence of this node's
    /// own public address (`info.observed_addr` — see `discovery.rs`'s
    /// `identify::Event::Received` handler, the only caller). A no-op when
    /// `host_port`'s host is still private/loopback/link-local
    /// (`addrs::is_private_or_local`) — that's not new information, just a
    /// peer on the same LAN or a NAT rewriting within another private range.
    /// Publica no `Announce` os multiaddresses que o swarm escuta hoje.
    ///
    /// Chamado por `Discovery` sempre que um listener, um endereço externo
    /// confirmado ou uma reserva de circuito entra ou sai. Devolve `true`
    /// quando a lista mudou, para o chamador logar a mudança; o anúncio novo
    /// sai no gossip seguinte, em no máximo 5 s.
    pub fn set_libp2p_addrs(&self, addrs: Vec<String>) -> bool {
        let mut guard = self.libp2p_addrs.write().expect("libp2p_addrs lock");
        if *guard == addrs {
            return false;
        }
        *guard = addrs.clone();
        drop(guard);
        self.traversal.write().expect("traversal lock").addrs = addrs;
        true
    }

    /// Um multiaddress público e direto deste nó, quando existe.
    ///
    /// Circuito fica de fora de propósito: o registro pkarr é republicado a
    /// cada 30 min e uma reserva não dura previsivelmente tanto, então
    /// publicar um circuito ali deixaria o registro apontando a maior parte do
    /// tempo para um relé que não vale mais. O que vale no DHT é o endereço
    /// que o AutoNAT confirmou, para um nó público ser encontrável a frio.
    /// Retrato da travessia para a UI e para o operador.
    pub fn traversal_status(&self) -> TraversalStatus {
        self.traversal.read().expect("traversal lock").clone()
    }

    pub fn set_nat_status(&self, nat: &str) {
        self.traversal.write().expect("traversal lock").nat = nat.to_string();
    }

    pub fn remember_reservation(&self, relay_peer_id: &str) {
        let mut guard = self.traversal.write().expect("traversal lock");
        if !guard.reservations.iter().any(|id| id == relay_peer_id) {
            guard.reservations.push(relay_peer_id.to_string());
        }
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

    pub fn public_libp2p_addr(&self) -> Option<String> {
        self.libp2p_addrs
            .read()
            .expect("libp2p_addrs lock")
            .iter()
            .find(|addr| !addr.contains("/p2p-circuit"))
            .cloned()
    }

    pub fn libp2p_addrs(&self) -> Vec<String> {
        self.libp2p_addrs
            .read()
            .expect("libp2p_addrs lock")
            .clone()
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
            info!("📡 Endereço público observado por um peer: {host_port}");
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

    /// Enderecos que este no anuncia de si.
    ///
    /// O endereço fixado pelo operador vem primeiro. Sem fixação, os
    /// endereços públicos atuais das interfaces vêm antes do observado por
    /// identify, e os endereços locais restantes fecham a lista. Assim, uma
    /// rotação de IPv6 no macOS não deixa um endereço observado antigo na
    /// frente, mas um nó atrás de NAT ainda conserva seu fallback público.
    pub fn self_addr_candidates(&self) -> Vec<String> {
        let porta = self.bind_addr.port();
        let pinned = self
            .advertise_addr
            .read()
            .expect("advertise_addr lock")
            .clone();
        if let Some(pinned) = pinned {
            return vec![pinned];
        }
        let observed = self.observed_addr();
        local_candidates_by_preference(None, observed.as_deref(), crate::addrs::local_addrs())
            .into_iter()
            .map(|ip| host_port(&ip, porta))
            .collect()
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

    /// Envia para os endereços conhecidos até um envio ser aceito pelo socket.
    /// O endereço anunciado é cache, então uma rotação de IPv6 não pode
    /// impedir o anúncio aos demais caminhos vivos.
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
    /// Este é o **único** construtor de `Announce` do nó: o UDP (bootstrap,
    /// peers, broadcast e a resposta a `PeerRequest`) e o publish periódico no
    /// gossipsub passam todos por aqui. Um segundo construtor em `main.rs`
    /// anunciava menos capabilities, com `role`/`hostname` vazios e só quando
    /// havia assignment, então um peer que chegasse pela internet via um nó
    /// diferente do que a LAN via.
    fn self_announce(&self, assignment: Option<&LayerAssignment>) -> P2PMessage {
        // A enumeração é deliberadamente feita aqui, em cada anúncio. O
        // gossip anuncia a cada 5 s; a operação é apenas leitura local de
        // interfaces, não uma sonda de rede. O mesmo snapshot alimenta `addr`
        // e `addr_candidates`, evitando publicar duas fotografias diferentes
        // se o IPv6 temporário rodar durante a montagem.
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
            authority_pubkey: self.authority_pubkey(),
            cluster_id: self.cluster_id(),
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
            // `CAP_LIBP2P_TUNNEL`). The last one only goes out once the swarm
            // has actually come up (`libp2p_tunnel_ready`) — claiming it any
            // earlier would send peers to dial a PeerId that can't answer.
            inference_frame_capabilities: {
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
                    // Todo binário novo fala o protocolo do MTP fatiado
                    // (rascunho pré-fabricado + tokens + mtp_k na wire);
                    // `supports_mtp` exige a cap de todos os passos para
                    // excluir peer velho do anel.
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
                if self.libp2p_tunnel_ready() {
                    caps.push("transport/libp2p-tunnel-v1".into());
                }
                caps
            },
            gateway_port: self.gateway_port,
            gateway_quic_port: self.gateway_quic_port,
            profile: self.profile(),
            tls: self.tls_enabled(),
            quic: crate::peer_client::quic_ativo(),
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
            libp2p_addrs: self.libp2p_addrs(),
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

    pub fn is_idle(&self) -> bool {
        self.idle.load(Ordering::Relaxed)
    }

    pub fn set_idle(&self, idle: bool) {
        self.idle.store(idle, Ordering::Relaxed);
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

    pub(crate) fn cluster_id(&self) -> Option<String> {
        self.admission
            .as_ref()
            .and_then(|admission| admission.trust.cluster_id())
    }

    /// A alça que o servidor de inferência escreve quando (não) conseguiu
    /// anexar a cabeça `nextn`. Ver o campo `mtp_ready`.
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

    /// Muda a geração dona da capacidade e a retira enquanto o novo runtime
    /// ainda não respondeu ao `/health`. O lock cobre geração e capacidade
    /// juntos, para um watcher antigo não ressuscitar um anúncio depois de um
    /// `apply`.
    pub fn set_runtime_generation(&self, generation: u64) {
        if let Ok(mut state) = self.runtime_capacity.write() {
            state.generation = generation;
            state.capacity = None;
        }
    }

    /// Atualiza a capacidade que será enviada no próximo anúncio. Reservado a
    /// testes e a mudanças que já controlam a geração; o watcher de saúde usa
    /// `set_runtime_capacity_for_generation` para não publicar valor antigo.
    pub fn set_runtime_capacity(&self, capacity: Option<RuntimeCapacity>) {
        if let Ok(mut state) = self.runtime_capacity.write() {
            state.capacity = capacity;
        }
    }

    /// Publica somente se este health watcher ainda pertence ao runtime atual.
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

    /// Retorna a capacidade confirmada pelo `/health` local.
    pub fn runtime_capacity(&self) -> Option<RuntimeCapacity> {
        self.runtime_capacity
            .read()
            .ok()
            .and_then(|state| state.capacity)
    }

    pub fn tls_enabled(&self) -> bool {
        self.tls.load(Ordering::Relaxed)
    }

    /// Ligado ou desligado pela rota do principal, ou pela catraca depois de
    /// confirmar com ele. Nunca a partir de um Announce.
    pub fn set_tls(&self, on: bool) {
        self.tls.store(on, Ordering::Relaxed);
    }

    /// Liga o canal da catraca. Sem isto, discordância de anúncio é ignorada -
    /// que é o comportamento certo para um nó de teste, sem config para gravar.
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

    /// A política só muda pela interface do principal ou pela resposta
    /// autenticada da catraca. Nunca pelo booleano recebido no gossip.
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

    /// A política só muda pela interface do principal ou pela resposta
    /// autenticada da catraca. Nunca pelo booleano recebido no gossip.
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

    /// Identifica o principal real para as catracas de política. O valor vem
    /// da cadeia administrativa local, nunca de um campo não autenticado do
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
                crate::peer_client::quic_ativo(),
                announced,
                from_principal,
                node_id,
            )
        {
            if let Ok(slot) = self.quic_probe.read() {
                if let Some(tx) = slot.as_ref() {
                    // O valor anunciado continua sendo só o gatilho. A task
                    // da política busca o valor no endpoint autenticado do
                    // principal antes de o adotar.
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
    /// announce arrived over UDP or gossipsub.
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

    /// Retorna todos os peers admin vivos e seus endereços libp2p. A lista é
    /// usada para replicar dados locais em cada autoridade, não só no
    /// principal.
    pub async fn admin_peers(&self) -> Vec<(libp2p::PeerId, Vec<String>)> {
        let peers = self.peers.read().await;
        peers
            .values()
            .filter(|peer| peer.is_alive() && peer.profile == crate::profile::NodeProfile::Admin)
            .filter_map(|peer| {
                let peer_id = peer.libp2p_peer_id.as_deref()?.parse().ok()?;
                Some((peer_id, peer.libp2p_dialable_addrs()))
            })
            .collect()
    }

    /// Retorna o principal conhecido e seu endereço libp2p para os fluxos que
    /// ainda precisam de um único próximo salto, como o Chat da rede.
    pub async fn principal_peer(&self) -> Option<(libp2p::PeerId, Vec<String>)> {
        let peers = self.peers.read().await;
        peers
            .values()
            .filter(|peer| peer.is_alive() && peer.profile == crate::profile::NodeProfile::Admin)
            .find_map(|peer| {
                let peer_id = peer.libp2p_peer_id.as_deref()?.parse().ok()?;
                Some((peer_id, peer.libp2p_dialable_addrs()))
            })
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
        self.network_settings_sync_needed
            .store(true, Ordering::Release);
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

    /// Repergunta o endereço atual de um peer que ficou ilegível.
    ///
    /// Chamado quando **todo** endereço conhecido de um peer está
    /// desacreditado. Insistir num endereço morto não o ressuscita; o que
    /// ressuscita o peer é a rede voltar a dizer onde ele está. Duas frentes,
    /// porque nenhuma sozinha basta:
    ///
    /// 1. `PeerRequest` para os bootstrap nodes e para os outros peers vivos -
    ///    quem responde manda o próprio `Announce` **e** o `PeerList`, e é dali
    ///    que sai o endereço que o peer perdido anuncia hoje;
    /// 2. os candidatos desacreditados saem da entrada local, para que o
    ///    próximo `Announce` do peer a repovoe do zero em vez de somar-se ao
    ///    cadáver - `merge_observed` só acrescenta.
    ///
    /// Não há broadcast na LAN aqui, de propósito: o peer continua vivo (é ele
    /// que está em `alive_peers`) e o `Announce` dele chega a cada
    /// `GOSSIP_INTERVAL_SECS` de qualquer modo. O broadcast não traria nada que
    /// esse `Announce` não traga, e transmitir na LAN a partir de um handler de
    /// leitura quebraria a regra de que a suíte de rotas nunca fala UDP de
    /// verdade.
    ///
    /// Limitado a uma vez por `REDISCOVERY_INTERVAL` por peer: o Painel relê o
    /// cluster em ciclo curto, e sem freio isto viraria uma tempestade de UDP
    /// por atualização de tela. Devolve `true` quando a busca saiu de fato.
    pub async fn request_peer_addresses(&self, node_id: &str) -> bool {
        if self.is_detached() {
            return false;
        }
        {
            let Ok(mut pedidos) = self.rediscovery.write() else {
                return false;
            };
            let agora = Instant::now();
            match pedidos.get_mut(node_id) {
                Some(busca) => {
                    if busca.ultima_pergunta.elapsed() < REDISCOVERY_INTERVAL {
                        return false;
                    }
                    busca.ultima_pergunta = agora;
                }
                None => {
                    pedidos.insert(
                        node_id.to_string(),
                        Rediscovery {
                            desde: agora,
                            ultima_pergunta: agora,
                        },
                    );
                }
            }
        }

        // O cadáver sai da entrada local. Sem isto o `Announce` seguinte cai no
        // `merge_observed`, que só acrescenta, e o endereço morto seguiria na
        // frente da fila para sempre.
        {
            let mut peers = self.peers.write().await;
            if let Some(peer) = peers.get_mut(node_id) {
                peer.addr_candidates
                    .retain(|addr| !self.routes.endpoint_is_discredited(node_id, addr));
            }
        }

        info!(
            "🔎 Reencontrando {}: nenhum endereço conhecido responde",
            short(node_id)
        );

        let pergunta = P2PMessage::PeerRequest {
            from: self.node_id.clone(),
        };
        for bootstrap in &self.bootstrap_nodes {
            let _ = self.send_to(&pergunta, bootstrap).await;
        }
        let vizinhos: Vec<Vec<String>> = {
            let peers = self.peers.read().await;
            peers
                .values()
                .filter(|p| p.node_id != node_id && p.is_alive())
                .map(PeerInfo::dialable_addrs)
                .filter(|addresses| !addresses.is_empty())
                .collect()
        };
        for addresses in &vizinhos {
            let _ = self.send_to_candidates(&pergunta, addresses).await;
        }
        true
    }

    /// Se este nó está agora reencontrando o peer: já pediu o endereço atual e
    /// ainda está dentro da janela em que faz sentido esperar a resposta.
    ///
    /// É o que separa "não dá para ler" de "estou reencontrando". A tela
    /// precisa da diferença: a primeira é um beco sem saída, a segunda é
    /// trabalho em curso - e depois de `REDISCOVERY_GIVE_UP` volta a ser a
    /// primeira, porque chamar de trabalho em curso o que já não anda seria o
    /// mesmo aviso desonesto com outra palavra.
    pub fn is_rediscovering(&self, node_id: &str) -> bool {
        self.rediscovery
            .read()
            .ok()
            .and_then(|pedidos| pedidos.get(node_id).map(|busca| busca.desde))
            .is_some_and(|desde| desde.elapsed() < REDISCOVERY_GIVE_UP)
    }

    /// Envelhece a busca em curso, para um teste alcançar o prazo sem esperar.
    #[cfg(test)]
    pub fn age_rediscovery_for_test(&self, node_id: &str, quanto: Duration) {
        if let Ok(mut pedidos) = self.rediscovery.write() {
            if let Some(busca) = pedidos.get_mut(node_id) {
                busca.desde -= quanto;
                busca.ultima_pergunta -= quanto;
            }
        }
    }

    /// O peer voltou a ser lido: a busca acabou.
    ///
    /// Sem isto, uma máquina que se recuperou depois de uma rotação continuaria
    /// a contar o tempo da busca antiga, e a busca seguinte já nasceria velha.
    pub fn note_peer_read(&self, node_id: &str) {
        if let Ok(mut pedidos) = self.rediscovery.write() {
            pedidos.remove(node_id);
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
            // Um cache próprio, e não o do gateway: esta task nasce dentro do
            // `Network`, que não conhece o `GatewayState`. O timeout curto é o
            // da sonda, não o de inferência.
            let clientes = crate::peer_client::PeerClients::new(Duration::from_secs(10));
            let mut ticker = interval(Duration::from_secs(30));
            loop {
                ticker.tick().await;
                if net_routes.is_detached() {
                    continue;
                }
                // A sonda nova começa sem política própria. Sem sincronizar o
                // cliente com o estado atual do cluster, os candidatos HTTPS
                // falham silenciosamente no TLS e o painel fica sem RTT dos
                // peers remotos. Atualizar só o cliente mantém o esquema das
                // URLs sob responsabilidade da política global já confirmada.
                clientes.set_tls(net_routes.tls_enabled());
                for peer in net_routes.alive_peers().await {
                    let precisa_rota = net_routes.routes.precisa_medir(&peer.node_id);
                    let precisa_rtt = net_routes.rtts.precisa_medir(&peer.node_id);
                    if !precisa_rota && !precisa_rtt {
                        continue;
                    }

                    if precisa_rota {
                        let candidatos = peer.inference_candidates();
                        if !candidatos.is_empty() {
                            net_routes
                                .routes
                                .measure_peer(&clientes, &peer.node_id, &candidatos)
                                .await;
                        }
                    }

                    if precisa_rtt {
                        let peer_id = peer
                            .libp2p_peer_id
                            .as_deref()
                            .and_then(|id| id.parse::<libp2p::PeerId>().ok());
                        let has_tunnel = peer_id.is_some()
                            && peer
                                .inference_frame_capabilities
                                .iter()
                                .any(|cap| cap == crate::pipeline::CAP_LIBP2P_TUNNEL);
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

                        // A capability anunciada não garante que o swarm local
                        // ainda esteja pronto. Se a medição pelo túnel falhar,
                        // mede a própria rota HTTP de inferência como fallback.
                        // Isso também renova o direct-health a cada intervalo
                        // de RTT, sem esperar o intervalo mais lento de rota.
                        let direct_health_rtt = if tunnel_rtt.is_none() {
                            net_routes
                                .routes
                                .measure_current_peer(&clientes, &peer.node_id)
                                .await
                        } else {
                            None
                        };

                        if let Some((rtt_ms, transport)) =
                            crate::route::escolher_rtt(tunnel_rtt, direct_health_rtt)
                        {
                            net_routes.rtts.remember(&peer.node_id, rtt_ms, transport);
                        } else {
                            net_routes.rtts.mark_failure(&peer.node_id);
                        }
                    }
                }
            }
        });

        // Plano de rotas por aresta: mede em segundo plano o transporte até
        // cada peer vivo, para o frame usar uma rota fixada em vez de
        // redescobrir a escada a cada chamada. Ver `route_plan`.
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
                authority_pubkey,
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
                libp2p_peer_id,
                libp2p_addrs,
                tp,
                runtime_capacity,
                cluster_logs_enabled,
                idle,
                network_chat_enabled,
                cluster_id,
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
                    authority_pubkey,
                    cluster_id,
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
                    libp2p_peer_id,
                    libp2p_addrs,
                    tp,
                    runtime_capacity,
                    idle,
                };

                self.remember_public_discovery_peer(&peer).await;
                if !self.admit_peer(&peer).await {
                    debug!(
                        "Announce de {} descartado: peer sem prova de pertencimento",
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

                // A catraca do TLS. O anúncio é **gatilho, nunca valor**: nada
                // aqui grava a política. Discordar do principal só pede uma
                // busca autenticada nele, e é a resposta dela que vale. Ver
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
                            // O `tls` anunciado vai junto, e serve para **uma
                            // coisa só**: saber por qual esquema discar a
                            // sonda. Sem isso um nó que ainda acha o cluster em
                            // claro discaria `http://` num principal estrito,
                            // seria recusado, e a catraca nunca conseguiria
                            // ligar. Forjar a dica não muda o que é gravado -
                            // só faz a sonda bater na porta errada e falhar.
                            let _ = tx.send((principal, tls));
                        }
                    }
                }

                self.trigger_quic_probe(&peer.node_id, &peer.authority_pubkey, quic, tls);

                self.trigger_cluster_logs_probe(
                    &peer.node_id,
                    &peer.authority_pubkey,
                    cluster_logs_enabled,
                    tls,
                );
                self.trigger_network_chat_probe(
                    &peer.node_id,
                    &peer.authority_pubkey,
                    network_chat_enabled,
                    tls,
                );
                self.trigger_network_settings_probe(&peer.node_id, &peer.authority_pubkey, tls);

                // A origem UDP é um fallback útil somente para anúncios sem
                // candidatos, como os de binários antigos. Quando há uma
                // lista explícita, ela é a autoridade: acrescentar a origem
                // aqui reintroduziria exatamente o endereço LAN que um
                // `advertise_addr` pinado deve conter.
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
                            "PeerList descartou {}: peer sem prova de pertencimento",
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
                    // O snapshot de outro nó é a lista que o peer anuncia
                    // **hoje**; o que se herda daqui é só o que este nó sabe e
                    // o outro não - o endereço observado, que enxerga NAT.
                    //
                    // Herdar a lista inteira tornava candidato morto imortal:
                    // um endereço que o peer deixou de anunciar voltava em todo
                    // `PeerList`, e o conjunto só crescia. Com IPv6 temporário,
                    // que roda sozinho, isso é um cadáver novo por rotação.
                    // Quem já provou que o endereço não atende não o traz de
                    // volta.
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
        let mortos: Vec<String> = {
            let mut peers = self.peers.write().await;
            let mortos: Vec<String> = peers
                .values()
                .filter(|p| !p.is_alive())
                .map(|p| p.node_id.clone())
                .collect();
            peers.retain(|_, p| p.is_alive());
            mortos
        };
        self.public_discovery
            .write()
            .await
            .retain(|_, peer| peer.is_alive());
        if mortos.is_empty() {
            return;
        }
        // O peer saiu: a memória de endereço dele vai junto. Guardá-la faria um
        // nó que volta com o mesmo endereço nascer desacreditado.
        for node_id in &mortos {
            self.routes.forget(node_id);
            if let Ok(mut pedidos) = self.rediscovery.write() {
                pedidos.remove(node_id);
            }
        }
        info!("🧹 Removed {} timed-out peers", mortos.len());
    }

    fn admission_cache_key(&self, peer: &PeerInfo, connection_key: &str) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}",
            peer.node_id,
            peer.pubkey,
            peer.authority_pubkey,
            peer.addr,
            peer.addr_candidates.join(","),
            peer.gateway_port.unwrap_or_default(),
            hex::encode(Sha256::digest(connection_key.as_bytes()))
        )
    }

    fn legacy_identity_matches(node_id: &str, pubkey: &str) -> bool {
        let Ok(bytes) = hex::decode(pubkey) else {
            return false;
        };
        hex::encode(Sha256::digest(bytes)).eq_ignore_ascii_case(node_id)
    }

    fn authority_peer_id_matches(peer: &PeerInfo) -> bool {
        let Some(peer_id) = peer.libp2p_peer_id.as_deref() else {
            return false;
        };
        let Ok(bytes) = hex::decode(&peer.authority_pubkey) else {
            return false;
        };
        let Ok(public) = libp2p::identity::ed25519::PublicKey::try_from_bytes(&bytes) else {
            return false;
        };
        let public = libp2p::identity::PublicKey::from(public);
        libp2p::PeerId::from_public_key(&public).to_string() == peer_id
    }

    fn public_peer_is_admitted(
        peer: &PeerInfo,
        admission: &PeerAdmission,
        access_policy: crate::config::NetworkAccessPolicy,
    ) -> bool {
        if !Self::legacy_identity_matches(&peer.node_id, &peer.pubkey)
            || !Self::authority_peer_id_matches(peer)
        {
            return false;
        }
        let Some(cluster_id) = peer.cluster_id.as_deref() else {
            return false;
        };
        let Some(peer_id) = peer.libp2p_peer_id.as_deref() else {
            return false;
        };
        // `anyone_with_anchor`: o operador declarou que qualquer nó que segura a
        // âncora correspondente participa. Um worker comum nunca recebe
        // concessão de autoridade, então `admits_participant` o recusaria. Aqui a
        // barra é uma âncora local válida para o `cluster_id` anunciado, somada
        // ao vínculo de identidade já conferido acima (node_id ↔ pubkey ↔ chave
        // de autoridade ↔ PeerId, e o Announce do gossip vem assinado por esse
        // PeerId). As políticas mais estritas continuam exigindo concessão.
        if matches!(
            access_policy,
            crate::config::NetworkAccessPolicy::AnyoneWithAnchor
        ) {
            return admission.trust.anchors_cluster(cluster_id);
        }
        let principal = crate::admin_trust::AuthorityPrincipal {
            node_id: peer.node_id.clone(),
            peer_id: peer_id.to_string(),
            pubkey: peer.authority_pubkey.clone(),
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
        if !Self::legacy_identity_matches(&peer.node_id, &peer.pubkey) {
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
        // Último recurso, e para um par atrás de NAT/CGNAT o **único** que
        // chega: a sonda pelo túnel libp2p. Sem ela a admissão nunca conclui
        // para esse par, ele não entra na tabela `peers`, e roteamento, UI e
        // pipeline continuam cegos para ele mesmo com o hole punch pronto.
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
                    "admissão de {} pelo túnel não concluiu: {error}",
                    short(&peer.node_id)
                );
                AdmissionProbe::Unreachable
            }
        }
    }

    /// Confere pertencimento antes de qualquer efeito derivado do peer.
    ///
    /// A resposta `challenge_required` do handshake já prova a chave
    /// compartilhada; concluir a assinatura do nonce seria uma prova de
    /// identidade adicional, não necessária para a admissão privada atual.
    pub(crate) async fn admit_peer(&self, peer: &PeerInfo) -> bool {
        if let Some(allowed) = self.admission_without_probe(peer).await {
            return allowed;
        }
        let Some(admission) = self.admission.as_ref() else {
            return true;
        };
        let connection_key = admission.auth.read().await.connection_key.clone();
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
                "admissão de {}: sem resposta, mantido pela prova anterior",
                short(&peer.node_id)
            );
        }
        let allowed = entry.allowed;
        cache.insert(cache_key, entry);
        allowed
    }

    /// Uma conexão libp2p com `peer` abriu (`ConnectionEstablished`).
    pub(crate) fn note_libp2p_link(
        &self,
        peer: &libp2p::PeerId,
        id: libp2p::swarm::ConnectionId,
        address: &libp2p::Multiaddr,
    ) {
        let (text, relayed, relay_peer_id) = describe_multiaddr(address);
        let hole_punched = self
            .hole_punched_links
            .lock()
            .is_ok_and(|punched| punched.contains(&id));
        let link = Libp2pLink {
            address: text,
            relayed,
            relay_peer_id,
            hole_punched,
        };
        if let Ok(mut links) = self.libp2p_links.write() {
            let entry = links.entry(peer.to_string()).or_default();
            entry.retain(|(existing, _)| *existing != id);
            entry.push((id, link));
        }
    }

    /// A conexão `id` com `peer` fechou (`ConnectionClosed`).
    pub(crate) fn drop_libp2p_link(&self, peer: &libp2p::PeerId, id: libp2p::swarm::ConnectionId) {
        if let Ok(mut punched) = self.hole_punched_links.lock() {
            punched.remove(&id);
        }
        if let Ok(mut links) = self.libp2p_links.write() {
            let key = peer.to_string();
            if let Some(entry) = links.get_mut(&key) {
                entry.retain(|(existing, _)| *existing != id);
                if entry.is_empty() {
                    links.remove(&key);
                }
            }
        }
    }

    /// O DCuTR fechou o hole punch com `peer` e a conexão direta é `id`.
    pub(crate) fn mark_hole_punched(&self, peer: &libp2p::PeerId, id: libp2p::swarm::ConnectionId) {
        if let Ok(mut punched) = self.hole_punched_links.lock() {
            punched.insert(id);
        }
        if let Ok(mut links) = self.libp2p_links.write() {
            if let Some(entry) = links.get_mut(&peer.to_string()) {
                for (existing, link) in entry.iter_mut() {
                    if *existing == id {
                        link.hole_punched = true;
                    }
                }
            }
        }
    }

    /// Conexões libp2p vivas com o par de PeerId `peer_id`. Um circuito que
    /// entrou sem o endereço do relé ganha o endereço da conexão deste nó com
    /// o próprio relé, quando existe.
    pub fn libp2p_links(&self, peer_id: &str) -> Vec<Libp2pLink> {
        let Ok(links) = self.libp2p_links.read() else {
            return Vec::new();
        };
        let mut out: Vec<Libp2pLink> = links
            .get(peer_id)
            .map(|entry| entry.iter().map(|(_, link)| link.clone()).collect())
            .unwrap_or_default();
        for link in out
            .iter_mut()
            .filter(|link| link.relayed && link.address.is_empty())
        {
            if let Some(relay) = link.relay_peer_id.as_deref() {
                if let Some(direct) = links
                    .get(relay)
                    .and_then(|entry| entry.iter().find(|(_, l)| !l.relayed))
                {
                    link.address = direct.1.address.clone();
                }
            }
        }
        out
    }

    /// Endereços do gateway de `peer`, na ordem em que vale tentar: primeiro o
    /// do host que respondeu por último a este nó (`RouteTable::last_success`),
    /// depois os que não falharam há pouco, na ordem anunciada, e por último
    /// os desacreditados.
    ///
    /// O primeiro endereço anunciado pode ser um IPv6 que ninguém alcança; um
    /// endereço que já respondeu é fato, não palpite. `last_success` pode ter
    /// vindo da porta de inferência (o caminho de inferência também o
    /// alimenta), por isso a comparação é pelo host.
    pub fn gateway_bases(&self, peer: &PeerInfo) -> Vec<String> {
        self.order_by_proven_host(&peer.node_id, peer.gateway_candidates())
    }

    /// `gateway_bases` para o servidor de inferência do peer.
    pub fn inference_bases(&self, peer: &PeerInfo) -> Vec<String> {
        self.order_by_proven_host(&peer.node_id, peer.inference_candidates())
    }

    /// A mesma ordem para uma lista de endereços do gateway montada por quem
    /// chama - com o esquema anunciado pelo peer, por exemplo, como fazem as
    /// catracas de política.
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

    /// A rota em uso para `peer`, na ordem em que o plano de controle a tenta:
    /// endereço HTTP direto que respondeu, conexão libp2p direta, circuito
    /// pelo relé.
    pub fn peer_route(&self, peer: &PeerInfo) -> PeerRoute {
        let links = peer
            .libp2p_peer_id
            .as_deref()
            .map(|id| self.libp2p_links(id))
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
                    link.relay_peer_id.clone().unwrap_or_default()
                } else {
                    link.address.clone()
                }),
            )
        } else {
            ("none", None)
        };
        PeerRoute {
            kind,
            address,
            hole_punched,
            links,
        }
    }

    /// O que `admit_peer` responde sem abrir conexão: a rede pública e a chave
    /// vazia se decidem localmente, e a rede privada pelo cache. `None` quando
    /// a resposta depende da sonda `/handshake`.
    pub(crate) async fn admission_without_probe(&self, peer: &PeerInfo) -> Option<bool> {
        let Some(admission) = self.admission.as_ref() else {
            return Some(true);
        };
        let (connection_key, is_public_network, access_policy) = {
            let config = admission.auth.read().await;
            (
                config.connection_key.clone(),
                config.network.visibility == crate::config::NetworkVisibility::Public,
                config.network.access_policy,
            )
        };
        if is_public_network {
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

    /// Roda a sonda de admissão numa tarefa própria e só aquece o cache: o
    /// próximo anúncio do peer (no máximo 5 s depois) encontra o veredito e
    /// segue o caminho normal de `discovery.rs`.
    ///
    /// Existe por causa do laço do swarm. A sonda cai, por último, no túnel
    /// libp2p, e o pedido do túnel é atendido por esse mesmo laço. Aguardada
    /// dentro dele, a sonda esperava por si mesma até o `ADMISSION_TIMEOUT` e
    /// falhava sempre: um peer que só o relé alcança era admitido uma vez
    /// enquanto alguma rota direta funcionava, e saía da tabela (`Removed
    /// timed-out peers`) quando o cache vencia. As sondas diretas, 2 s por
    /// endereço, também paravam o swarm inteiro enquanto esperavam.
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
                "admissão de {} concluída fora do swarm: {}",
                short(&peer.node_id),
                if allowed { "admitido" } else { "recusado" }
            );
        });
    }

    /// A rede pública tem uma porta de descoberta aberta, mas isso não é uma
    /// concessão para participar do enxame. Guardamos somente candidatos que
    /// conseguem provar a ligação entre identidade, chave de autoridade e
    /// PeerId. A cadeia de confiança continua sendo exigida por
    /// `admit_peer` antes de qualquer efeito de membro.
    fn is_public_discovery_candidate(peer: &PeerInfo) -> bool {
        peer.gateway_port.is_some()
            && peer
                .cluster_id
                .as_deref()
                .is_some_and(|cluster| !cluster.trim().is_empty())
            && Self::legacy_identity_matches(&peer.node_id, &peer.pubkey)
            && Self::authority_peer_id_matches(peer)
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
                "ignorando peer {} sem endereço UDP discável",
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
                "🆕 Peer via libp2p: {} model={} layers={}-{}",
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

    /// Insert or update a peer discovered via libp2p (gossipsub / mDNS).
    pub async fn insert_peer(&self, peer: PeerInfo) {
        self.remember_public_discovery_peer(&peer).await;
        if !self.admit_peer(&peer).await {
            debug!(
                "peer {} recusado pela catraca de admissão",
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

    /// O gossipsub e o UDP têm que publicar o mesmo anúncio, inclusive quando o
    /// nó ainda não tem modelo. O publish do libp2p já teve construtor próprio,
    /// que omitia `relay/v1-chain` e `speculate/v1-lookup`, mandava `role` e
    /// `hostname` vazios e **só** publicava com assignment: um nó sem modelo
    /// ficava invisível pela internet enquanto a LAN o via normalmente.
    #[tokio::test]
    async fn gossipsub_and_udp_publish_the_same_announce() {
        let net = Network::testkit("n".into(), "p".into(), vec![], None, Some(8181), Some(8080))
            .await
            .unwrap()
            .with_identity(crate::profile::NodeProfile::Admin, "macbook".into())
            .with_quic_ports(Some(8182), Some(8081));
        net.set_advertise_addr(Some("198.51.100.9:8080"));
        assert_eq!(
            net.inference_quic_addr_candidates(),
            vec!["198.51.100.9:8182"]
        );
        assert_eq!(
            net.gateway_quic_addr_candidates(),
            vec!["198.51.100.9:8081"]
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
                crate::pipeline::CAP_FRAME_BATCH,
                crate::pipeline::CAP_REMOTE_TOKEN_STREAM,
                "relay/v1-chain",
                "relay/v2-direct",
                "speculate/v1-lookup",
            ] {
                assert!(caps.iter().any(|c| c == cap), "faltou {cap} em {caps:?}");
            }
            assert_eq!(gossip["profile"], "admin");
            // Os dois anúncios (UDP e gossipsub) têm que carregar a flag: se só
            // um carregasse, a virada chegaria por um caminho e não pelo outro,
            // e o cluster ficaria meio cifrado sem ninguém notar.
            assert_eq!(gossip["tls"], false);
            assert_eq!(gossip["inference_quic_port"], 8182);
            assert_eq!(gossip["gateway_quic_port"], 8081);
            assert_eq!(gossip["hostname"], "macbook");
        }

        // E a flag ligada aparece nos dois caminhos, não só num deles.
        net.set_tls(true);
        let udp: serde_json::Value = serde_json::to_value(net.self_announce(None)).unwrap();
        let gossip: serde_json::Value =
            serde_json::from_slice(&net.gossip_announce_bytes(None).unwrap()).unwrap();
        assert_eq!(udp["tls"], true);
        assert_eq!(gossip["tls"], true);

        // O estado de ociosidade também precisa ser idêntico nos dois
        // transportes: um peer não pode parecer `serving` pela LAN e `idle`
        // pelo gossipsub.
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
            .expect("socket UDP em porta efêmera");
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
            .expect("socket UDP em porta efêmera");
        let mut first = dummy_peer("admin-a", "127.0.0.1:8081");
        first.authority_pubkey = "authority-a".into();
        first.profile = crate::profile::NodeProfile::Admin;
        first.gateway_port = Some(8081);
        first.libp2p_peer_id = Some(libp2p::PeerId::random().to_string());
        let mut second = dummy_peer("admin-b", "127.0.0.1:8082");
        second.authority_pubkey = "authority-b".into();
        second.profile = crate::profile::NodeProfile::Admin;
        second.gateway_port = Some(8082);
        second.libp2p_peer_id = Some(libp2p::PeerId::random().to_string());
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
        let error = result.expect_err("campo removido não pode ser ignorado no wire");
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

    /// Quando o IPv6 da interface muda, o último endereço observado por
    /// identify não pode continuar vencendo o endereço público atual.
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
            local_candidates_by_preference(None, observed.as_deref(), local),
            vec!["2001:db8::1", "2001:db8::2", "203.0.113.5"]
        );
    }

    /// O host efetivo sai sempre com o IPv6 entre colchetes, venha de onde
    /// vier. O da interface local vinha cru, e a entrada do próprio nó virava
    /// `2804:...:5eb8:42777` na tela de participantes e `2804` para quem
    /// passa o host por `ip_host`.
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

    /// A troca de endereço local entre dois anúncios deve aparecer no anúncio
    /// seguinte sem reiniciar o processo. O teste usa duas fotografias de
    /// interface para não depender do estado IPv6 desta máquina.
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
    /// LAN interfaces to offer — useless to a remote peer over gossipsub,
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
            local_candidates_by_preference(None, Some("203.0.113.5:9999"), vec![])
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
            local_candidates_by_preference(None, observed.as_deref(), local),
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

    #[test]
    fn a_pinned_address_replaces_local_announce_candidates() {
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
        assert_eq!(candidates, vec!["100.122.132.97:42777"]);
    }

    /// O que o peer publicou vence a derivação por `host:port`, e vem
    /// inteiro: o endereço de circuito costuma ser o segundo da lista, e
    /// devolver só o primeiro deixava um par atrás de NAT sem destino.
    #[test]
    fn os_multiaddresses_anunciados_vencem_e_vem_todos() {
        let mut peer = peer_com_candidatos(vec!["192.168.100.210:42777"]);
        peer.libp2p_addrs = vec![
            "/ip4/203.0.113.7/udp/52777/quic-v1".to_string(),
            "/ip4/198.51.100.4/tcp/4001/p2p/12D3KooWRBy97UB99e3J6hiPesre1MZeuNQvfan4gBziswrRJsNK/p2p-circuit".to_string(),
        ];

        assert_eq!(peer.libp2p_dialable_addrs(), peer.libp2p_addrs);
    }

    #[test]
    fn libp2p_dial_uses_addr_canonico_e_ignora_candidatos_legados() {
        let mut peer = peer_com_candidatos(vec!["192.168.100.210:42777", "172.17.0.1:42777"]);
        peer.addr = "100.122.132.97:42777".into();

        assert_eq!(
            peer.libp2p_dialable_addrs(),
            vec!["100.122.132.97:42777".to_string()]
        );
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
            authority_pubkey: String::new(),
            cluster_id: None,
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
            libp2p_peer_id: None,
            libp2p_addrs: Vec::new(),
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
            authority_pubkey: String::new(),
            cluster_id: None,
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
            libp2p_peer_id: None,
            libp2p_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
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

    /// Uma lista transitiva também é uma atualização, não só descoberta. Se o
    /// registro antigo ficar preso, dois gateways que conheceram o mesmo peer
    /// em momentos diferentes calculam buracos diferentes para a mesma fatia.
    #[tokio::test]
    async fn a_peer_list_refreshes_existing_peer_coverage() {
        let net = Network::new("local".into(), "pubkey".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let assignment = Arc::new(RwLock::new(None));

        let mut old = peer_com_candidatos(vec![]);
        old.node_id = "peer".into();
        old.end_layer = 15;
        old.total_layers = 44;
        let old_seen = old.last_seen;
        net.insert_peer(old).await;

        let mut refreshed = peer_com_candidatos(vec![]);
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
        let peer = peers.get("peer").expect("o peer antigo continua conhecido");
        assert_eq!(peer.end_layer, 28);
        assert_eq!(peer.total_layers, 44);
    }

    fn peer_com_candidatos(candidatos: Vec<&str>) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: "n1".into(),
            pubkey: "p".into(),
            authority_pubkey: String::new(),
            cluster_id: None,
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
            libp2p_peer_id: None,
            libp2p_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
        }
    }

    /// O `PeerList` de outro no traz a lista que o peer anuncia hoje. Herdar a
    /// lista local inteira por cima dela tornava candidato morto **imortal**: o
    /// endereco que o peer deixou de anunciar voltava a cada volta de gossip, e
    /// o conjunto so crescia. Com IPv6 temporario, que roda sozinho, isso e um
    /// cadaver novo por rotacao - e foi assim que o cluster do captain ficou a
    /// discar um endereco que nao existia em maquina nenhuma.
    #[tokio::test]
    async fn gossip_does_not_resurrect_an_address_already_proved_dead() {
        let net = Network::new("local".into(), "pubkey".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let assignment = Arc::new(RwLock::new(None));
        let morto = "[2804:d55:5579:1000:1c20:e551:6a95:9a0d]:42777";
        let vivo = "[2804:d55:5579:1000:18cc:e839:7c2:dd6a]:42777";

        let mut antigo = peer_com_candidatos(vec![morto]);
        antigo.node_id = "peer".into();
        antigo.addr = morto.into();
        let visto_em = antigo.last_seen;
        net.insert_peer(antigo).await;
        // Uma requisicao real ja provou que esse endereco nao atende.
        net.routes.mark_failure(
            "peer",
            "http://[2804:d55:5579:1000:1c20:e551:6a95:9a0d]:8080",
        );

        let mut fresco = peer_com_candidatos(vec![vivo]);
        fresco.node_id = "peer".into();
        fresco.addr = vivo.into();
        fresco.last_seen = visto_em + chrono::Duration::seconds(1);
        net.handle_message(
            P2PMessage::PeerList {
                peers: vec![fresco],
            },
            "127.0.0.1:42777".parse().unwrap(),
            &assignment,
        )
        .await;

        let guardado = net.peers.read().await["peer"].clone();
        assert!(
            !guardado.addr_candidates.iter().any(|c| c == morto),
            "o endereco morto voltou pelo gossip: {:?}",
            guardado.addr_candidates
        );
        assert!(
            guardado.addr_candidates.iter().any(|c| c == vivo),
            "o endereco atual tem de ficar: {:?}",
            guardado.addr_candidates
        );
    }

    /// Uma lista explicita anunciada pelo peer substitui a fotografia anterior;
    /// não se herda endereço observado, porque isso reintroduziria LAN, bridge
    /// ou um endereço antigo que o operador já deixou de anunciar.
    #[tokio::test]
    async fn gossip_drops_an_observed_candidate_when_the_peer_announces_a_set() {
        let net = Network::new("local".into(), "pubkey".into(), 0, vec![], None, None, None)
            .await
            .unwrap();
        let assignment = Arc::new(RwLock::new(None));
        let observado = "203.0.113.9:42777";
        let anunciado = "198.51.100.7:42777";

        let mut antigo = peer_com_candidatos(vec![observado]);
        antigo.node_id = "peer".into();
        antigo.addr = observado.into();
        let visto_em = antigo.last_seen;
        net.insert_peer(antigo).await;

        let mut fresco = peer_com_candidatos(vec![anunciado]);
        fresco.node_id = "peer".into();
        fresco.addr = anunciado.into();
        fresco.last_seen = visto_em + chrono::Duration::seconds(1);
        net.handle_message(
            P2PMessage::PeerList {
                peers: vec![fresco],
            },
            "127.0.0.1:42777".parse().unwrap(),
            &assignment,
        )
        .await;

        let guardado = net.peers.read().await["peer"].clone();
        assert_eq!(guardado.addr_candidates, vec![anunciado]);
    }

    /// Quando o peer envia candidatos explícitos, a origem efetiva do pacote
    /// não entra na lista: ela pode ser LAN, bridge ou outro endereço que não
    /// seja o `advertise_addr` que o peer quer publicar.
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
                authority_pubkey: String::new(),
                cluster_id: None,
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
                libp2p_peer_id: None,
                libp2p_addrs: Vec::new(),
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
        let peer = peers.get("peer").expect("o anúncio deve criar o peer");
        assert_eq!(peer.addr_candidates, vec![stale]);
    }

    /// `gateway_url()` era so `peer.addr`, entao o plano de controle tinha um
    /// endereco e nenhum plano B. `inference_url()` ja preferia candidatos ha
    /// tempo; o gateway nunca aprendeu.
    #[test]
    fn the_gateway_has_one_url_per_known_address_with_the_proven_one_first() {
        let mut peer = peer_com_candidatos(vec![
            "10.0.0.5:42777",
            "[2804:f30::1]:42777",
            "192.168.1.9:42777",
        ]);
        peer.addr = "192.168.1.9:42777".into();
        assert_eq!(
            peer.gateway_candidates(),
            vec![
                // O `addr` primeiro: e de onde os pacotes do peer chegaram.
                "http://192.168.1.9:8080".to_string(),
                "http://10.0.0.5:8080".to_string(),
                "http://[2804:f30::1]:8080".to_string(),
            ],
            "o primeiro e fato conhecido, e nenhuma URL sai repetida"
        );
        assert_eq!(
            peer.gateway_url(),
            Some("http://192.168.1.9:8080".to_string())
        );
    }

    /// Uma rotacao de IPv6 muda `addr_candidates` e chama `invalidate`. Apagar
    /// ali a memoria de falha era apagar a prova exatamente no evento que mata
    /// o endereco: o cluster voltava a discar o cadaver na volta seguinte.
    #[test]
    fn an_address_rotation_does_not_erase_the_proof_that_the_old_one_died() {
        let tabela = crate::route::RouteTable::new();
        tabela.mark_failure("peer", "http://[2804:d55::9]:8080");
        tabela.invalidate("peer");
        assert!(
            tabela.is_failed("peer", "http://[2804:d55::9]:8080"),
            "a rotacao nao pode apagar a prova de que o endereco antigo morreu"
        );
        tabela.forget("peer");
        assert!(
            !tabela.is_failed("peer", "http://[2804:d55::9]:8080"),
            "quando o peer some de vez, a memoria dele vai junto"
        );
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
            vec![
                "192.168.1.9:42777".to_string(),
                "[2804:f30::1]:42777".to_string()
            ],
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
            let mut peer = peer_com_candidatos(vec![]);
            peer.addr = addr.into();
            assert_eq!(peer.dialable_addr(), None, "{addr} must not be dialed");
            assert!(
                peer.inference_candidates().is_empty(),
                "{addr} must not become a URL"
            );
        }

        let peer = peer_com_candidatos(vec!["0.0.0.0:42778", "[2804:d55::1]:42778"]);
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
        let mut peer = peer_com_candidatos(vec![]);
        peer.addr = "[2804:d55::1]:0".into();
        assert_eq!(peer.dialable_addr(), None);

        peer.addr_candidates.push("[2804:d55::2]:42777".into());
        assert_eq!(peer.dialable_addr(), Some("[2804:d55::2]:42777"));
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
        assert_eq!(
            peer.addr, "192.168.1.9:42777",
            "addr fica desatualizado de propósito neste teste"
        );
        assert_eq!(
            peer.inference_url().as_deref(),
            Some("http://203.0.113.5:8181")
        );
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
        let msg: P2PMessage =
            serde_json::from_str(antigo).expect("Announce antigo tem que parsear");
        match msg {
            P2PMessage::Announce {
                addr_candidates, ..
            } => assert!(addr_candidates.is_empty()),
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
            deploy_id: String::new(),
            tls: false,
            node_id: node_id.into(),
            pubkey: "p".into(),
            authority_pubkey: String::new(),
            cluster_id: None,
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
            libp2p_peer_id: None,
            libp2p_addrs: Vec::new(),
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
        }))
    }

    fn admission_peer(gateway_port: u16) -> PeerInfo {
        let identity = crate::identity::Identity::generate();
        let node_id = identity.node_id.0.clone();
        let pubkey = identity.public_key_hex();
        PeerInfo {
            node_id,
            pubkey,
            authority_pubkey: String::new(),
            cluster_id: None,
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
            libp2p_peer_id: None,
            libp2p_addrs: Vec::new(),
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

    /// O endereço que já respondeu vem antes do primeiro anunciado, mesmo que
    /// tenha respondido na porta de inferência; os que falharam vão para o fim.
    #[tokio::test]
    async fn gateway_bases_put_the_proven_host_first() {
        let _politica = crate::peer_client::POLITICA_GLOBAL
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
    fn describe_multiaddr_names_the_far_end_and_the_relay() {
        let relay = libp2p::PeerId::random();
        let peer = libp2p::PeerId::random();
        let quic: libp2p::Multiaddr = format!("/ip4/189.75.167.63/udp/28515/quic-v1/p2p/{peer}")
            .parse()
            .unwrap();
        assert_eq!(
            describe_multiaddr(&quic),
            ("189.75.167.63:28515 (QUIC)".to_string(), false, None)
        );
        let tcp6: libp2p::Multiaddr = "/ip6/2001:db8::1/tcp/52777".parse().unwrap();
        assert_eq!(describe_multiaddr(&tcp6).0, "[2001:db8::1]:52777 (TCP)");

        let dialed: libp2p::Multiaddr =
            format!("/ip4/82.25.69.187/tcp/52777/p2p/{relay}/p2p-circuit/p2p/{peer}")
                .parse()
                .unwrap();
        assert_eq!(
            describe_multiaddr(&dialed),
            (
                "82.25.69.187:52777 (TCP)".to_string(),
                true,
                Some(relay.to_string())
            )
        );
        // Circuito que chegou: sem host, só o relé.
        let inbound: libp2p::Multiaddr = format!("/p2p/{relay}/p2p-circuit/p2p/{peer}")
            .parse()
            .unwrap();
        assert_eq!(
            describe_multiaddr(&inbound),
            (String::new(), true, Some(relay.to_string()))
        );
    }

    /// A rota mostrada é a observada: HTTP que respondeu, depois conexão
    /// libp2p direta, depois o relé - nunca o endereço anunciado.
    #[tokio::test]
    async fn peer_route_prefers_http_then_direct_then_relay() {
        use libp2p::swarm::ConnectionId;
        let network = Network::testkit("eu".into(), String::new(), vec![], None, None, None)
            .await
            .unwrap();
        let relay = libp2p::PeerId::random();
        let remote = libp2p::PeerId::random();
        let mut peer = admission_peer(8080);
        peer.addr = "[2804:d55::1]:42777".into();
        peer.libp2p_peer_id = Some(remote.to_string());
        assert_eq!(network.peer_route(&peer).kind, "none");

        // Este nó falando com o relé direto; o par chega por um circuito que
        // não traz o endereço do relé.
        network.note_libp2p_link(
            &relay,
            ConnectionId::new_unchecked(1),
            &"/ip4/82.25.69.187/tcp/52777".parse().unwrap(),
        );
        network.note_libp2p_link(
            &remote,
            ConnectionId::new_unchecked(2),
            &format!("/p2p/{relay}/p2p-circuit/p2p/{remote}")
                .parse()
                .unwrap(),
        );
        let route = network.peer_route(&peer);
        assert_eq!(route.kind, "relayed");
        assert_eq!(route.address.as_deref(), Some("82.25.69.187:52777 (TCP)"));
        assert!(!route.hole_punched);

        // O DCuTR avisa antes de a conexão direta aparecer: a marca vale igual.
        network.mark_hole_punched(&remote, ConnectionId::new_unchecked(3));
        network.note_libp2p_link(
            &remote,
            ConnectionId::new_unchecked(3),
            &"/ip4/189.75.167.63/udp/28515/quic-v1".parse().unwrap(),
        );
        let route = network.peer_route(&peer);
        assert_eq!(route.kind, "p2p_direct");
        assert_eq!(route.address.as_deref(), Some("189.75.167.63:28515 (QUIC)"));
        assert!(route.hole_punched);
        assert_eq!(route.links.len(), 2);

        network
            .routes
            .mark_success(&peer.node_id, "http://192.168.0.37:8080");
        let route = network.peer_route(&peer);
        assert_eq!(route.kind, "http");
        assert_eq!(route.address.as_deref(), Some("192.168.0.37:8080"));

        // A rota HTTP que falha sai; a direta fecha; sobra o relé.
        network
            .routes
            .mark_failure(&peer.node_id, "http://192.168.0.37:8080");
        network.drop_libp2p_link(&remote, ConnectionId::new_unchecked(3));
        assert_eq!(network.peer_route(&peer).kind, "relayed");
    }

    /// Sem veredito no cache a resposta é `None`, sem sonda: é o que o laço do
    /// swarm consulta, e ali uma sonda esperaria pelo próprio laço (ver
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
        // Porta 1: nada escuta ali, a sonda direta falha.
        let peer = admission_peer(1);

        assert_eq!(network.admission_without_probe(&peer).await, None);
        assert!(!network.admit_peer(&peer).await);
        assert_eq!(network.admission_without_probe(&peer).await, Some(false));
    }

    /// Sem resposta, só a prova recente segura o peer; recusa nunca segura.
    #[test]
    fn an_unreachable_recheck_keeps_only_a_recently_proven_peer() {
        let now = Instant::now();
        let recent = now - StdDuration::from_secs(60);
        let stale = now - ADMISSION_UNREACHABLE_GRACE - StdDuration::from_secs(1);

        let kept = admission_verdict(AdmissionProbe::Unreachable, Some(recent), now);
        assert!(kept.allowed);
        assert_eq!(kept.proven_at, Some(recent), "a folga não renova a prova");
        assert_eq!(kept.expires_at, now + REJECTED_ADMISSION_CACHE_TTL);

        assert!(!admission_verdict(AdmissionProbe::Unreachable, Some(stale), now).allowed);
        assert!(!admission_verdict(AdmissionProbe::Unreachable, None, now).allowed);
        assert!(!admission_verdict(AdmissionProbe::Refused, Some(recent), now).allowed);

        let approved = admission_verdict(AdmissionProbe::Approved, None, now);
        assert!(approved.allowed);
        assert_eq!(approved.proven_at, Some(now));
    }

    /// Ponta a ponta: aprovado pelo `/handshake`, o gateway some e o cache
    /// vence; a nova sonda não chega a ninguém e o peer continua admitido.
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
            "sem resposta, a prova recente mantém o peer"
        );
    }

    /// A sonda em segundo plano só aquece o cache, uma vez por peer, e não
    /// mexe na tabela: quem insere é o próximo anúncio.
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
        .expect("a sonda em segundo plano deve concluir");
        assert!(verdict);
        assert_eq!(network.peer_count().await, 0);
        tokio::time::timeout(StdDuration::from_secs(1), async {
            while !network.pending_admissions.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a marca de sonda pendente deve sair");

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
        let mut seed = signing_key.to_bytes();
        let libp2p_keypair = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed[..]).unwrap();
        let peer_id = libp2p_keypair.public().to_peer_id().to_string();
        let mut peer = admission_peer(8080);
        let principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            peer.node_id.clone(),
            peer_id.clone(),
            &signing_key,
        );
        let path =
            std::env::temp_dir().join(format!("pom-admission-{}.json", uuid::Uuid::new_v4()));
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
                admission_config(
                    "local-admin-credential",
                    crate::config::NetworkVisibility::Public,
                ),
                Arc::new(trust),
            );
        peer.authority_pubkey = principal.pubkey;
        peer.libp2p_peer_id = Some(peer_id);
        peer.cluster_id = Some("cluster".into());
        trusted.insert_peer(peer).await;
        assert_eq!(trusted.peer_count().await, 1);

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

    /// Um worker co-locado só tem a âncora e a `connection_key`, nunca uma
    /// concessão de autoridade. Sob `anyone_with_anchor` o Announce dele — com a
    /// identidade amarrada (node_id ↔ pubkey ↔ chave de autoridade ↔ PeerId) e o
    /// `cluster_id` da âncora local — basta para entrar em `peers`. Foi isto que
    /// `6c3b19db` quebrou: só o primário/secundário passava.
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

        // Peer worker: chave de autoridade própria, distinta do primário.
        let worker_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let mut seed = worker_key.to_bytes();
        let worker_peer_id = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed[..])
            .unwrap()
            .public()
            .to_peer_id()
            .to_string();
        let mut worker = admission_peer(8081);
        worker.authority_pubkey = hex::encode(worker_key.verifying_key().as_bytes());
        worker.libp2p_peer_id = Some(worker_peer_id);
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

    /// A relaxação vale só para `anyone_with_anchor`. Com `authorized` (padrão) o
    /// mesmo worker sem concessão continua fora, e um `cluster_id` que não é o da
    /// âncora nunca entra sob nenhuma política.
    #[tokio::test]
    async fn public_authorized_still_requires_a_grant_and_anchor_rejects_wrong_cluster() {
        let local = crate::identity::Identity::generate();
        let primary_key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);

        let worker_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let mut seed = worker_key.to_bytes();
        let worker_peer_id = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed[..])
            .unwrap()
            .public()
            .to_peer_id()
            .to_string();
        let mut worker = admission_peer(8081);
        worker.authority_pubkey = hex::encode(worker_key.verifying_key().as_bytes());
        worker.libp2p_peer_id = Some(worker_peer_id);
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

        // authorized + worker sem concessão => recusado.
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

        // anyone_with_anchor + cluster_id que não é o da âncora => recusado.
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

        let mut peer = admission_peer(8080);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let mut seed = signing_key.to_bytes();
        let peer_id = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed[..])
            .unwrap()
            .public()
            .to_peer_id()
            .to_string();
        let principal = crate::admin_trust::AuthorityPrincipal::from_signing_key(
            peer.node_id.clone(),
            peer_id.clone(),
            &signing_key,
        );
        peer.authority_pubkey = principal.pubkey;
        peer.libp2p_peer_id = Some(peer_id);
        peer.cluster_id = Some("public-network".into());

        assert!(!network.admit_peer(&peer).await);
        network.remember_public_discovery_peer(&peer).await;

        assert_eq!(network.peer_count().await, 0);
        assert_eq!(network.public_discovered_peers().await.len(), 1);
    }

    /// A descoberta transitiva era de tiro único: `PeerRequest` só saía no
    /// arranque e só para os bootstrap nodes. Fora da LAN (sem o broadcast IPv4
    /// para mascarar) um peer conhecido nunca era perguntado de novo, então os
    /// peers *dele* nunca eram aprendidos. O laço de gossip tem que voltar a
    /// perguntar, periodicamente, também aos peers vivos.
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
        let net = Network::testkit("n".into(), "p".into(), vec![], None, None, None)
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

    /// Um pânico no tratamento de um datagrama não pode matar a task que drena
    /// o socket: o datagrama seguinte ainda precisa atualizar o peer conhecido.
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
        .expect("o receive_loop não processou a mensagem depois do pânico");

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
        let recebido =
            tokio::time::timeout(std::time::Duration::from_secs(2), sock.recv_from(&mut buf))
                .await
                .expect("nada chegou de um remetente IPv4 no socket [::]")
                .unwrap();

        assert_eq!(&buf[..recebido.0], b"oi");
    }
}
