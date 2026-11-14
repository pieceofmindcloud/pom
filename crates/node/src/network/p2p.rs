//! Peer-to-peer transport over iroh: QUIC endpoints dialed by Ed25519 key.
//!
//! A node's endpoint runs under the node key itself (`.pom-node.key`), so its
//! `EndpointId` **is** the node's public key: the hex the `Announce` carries in
//! `pubkey` and in `endpoint_id` are the same string. iroh dials a key, not an
//! address; the addresses (`EndpointAddr`) are hints the dialer feeds it - the
//! announced ones, the pkarr record, mDNS. Each connection starts wherever it
//! can (a relay, a direct address) and iroh upgrades it to a direct path by
//! hole punching on its own, coordinated over the relay.
//!
//! Each protocol has its own ALPN, and a connection speaks exactly one: a
//! request/response protocol here is one bidirectional QUIC stream per call,
//! with the framing of the protocol's [`Codec`]. The connection is reused
//! across calls and dropped when it closes.
//!
//! The textual form of an endpoint address, used in the `Announce`
//! (`endpoint_addrs`), the pkarr record and the screens, is one string per
//! transport address: `ip:port` (`[v6]:port`) for a direct UDP address, the
//! relay URL (`https://...`, `http://...`) for a relay.

use async_trait::async_trait;
use futures::{AsyncRead, AsyncWrite};
use iroh::endpoint::Connection;
use iroh::TransportAddr;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::debug;

pub use iroh::{EndpointAddr, EndpointId, RelayUrl, SecretKey};

/// The endpoint's UDP port, and the TCP port of the embedded relay server,
/// are the node's `p2p_port` plus this offset (42777 -> 52777).
pub const P2P_PORT_OFFSET: u16 = 10_000;

/// Capability announced by a node whose endpoint is up and serves the tunnel.
pub const TUNNEL_CAPABILITY: &str = "transport/iroh-tunnel-v1";

/// How long establishing a connection may take before the call gives up.
/// A relay path is usually up in well under a second; the rest is slack for
/// a cold relay connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The iroh secret key of the node key. Both are Ed25519 over the same
/// 32-byte seed.
pub fn secret_key(signing_key: &ed25519_dalek::SigningKey) -> SecretKey {
    SecretKey::from_bytes(&signing_key.to_bytes())
}

/// Parses an endpoint id in any of the forms in use: the hex of the public
/// key (what `pubkey` and `endpoint_id` carry), the z-base-32 of a pkarr key,
/// or the `pk:` URI.
pub fn parse_endpoint_id(text: &str) -> Option<EndpointId> {
    let text = text.trim();
    let text = text.strip_prefix("pk:").unwrap_or(text);
    if text.len() == 64 {
        let bytes: [u8; 32] = hex::decode(text).ok()?.try_into().ok()?;
        return EndpointId::from_bytes(&bytes).ok();
    }
    EndpointId::from_z32(text).ok()
}

/// A fresh endpoint id with no node behind it, for tests and fixtures.
#[cfg(test)]
pub fn random_endpoint_id() -> EndpointId {
    SecretKey::generate().public()
}

/// Parses one textual transport address: `ip:port` or a relay URL.
pub fn parse_transport_addr(text: &str) -> Option<TransportAddr> {
    let text = text.trim();
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return usable_ip(&addr).then_some(TransportAddr::Ip(addr));
    }
    relay_url(text).map(TransportAddr::Relay)
}

/// A relay URL the operator typed: `https://relay.example.org`,
/// `http://203.0.113.7:52777`. Anything else is not a relay.
pub fn relay_url(text: &str) -> Option<RelayUrl> {
    let text = text.trim();
    if !(text.starts_with("https://") || text.starts_with("http://")) {
        return None;
    }
    let url: url::Url = text.parse().ok()?;
    url.host_str()?;
    Some(url.into())
}

/// A relay URL fit for a signed document (a trust document's or a public
/// record's `relay_hints`): one whose host is a DNS name. A document never
/// carries an IP, which goes stale and names no one.
pub fn relay_hint(text: &str) -> Option<String> {
    let url: url::Url = text.trim().parse().ok()?;
    if !matches!(url.scheme(), "https" | "http") {
        return None;
    }
    match url.host()? {
        url::Host::Domain(_) => Some(RelayUrl::from(url).to_string()),
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => None,
    }
}

/// Whether `url` is one of n0's default public relays.
pub fn is_default_relay(url: &str) -> bool {
    relay_url(url).is_some_and(|url| iroh::RelayMode::Default.relay_map().contains(&url))
}

fn usable_ip(addr: &SocketAddr) -> bool {
    addr.port() != 0 && !addr.ip().is_unspecified()
}

/// The address book entry of `id` from its textual addresses. Unparseable
/// entries are skipped, not fatal: one stale form must not hide the others.
pub fn endpoint_addr(id: EndpointId, addrs: &[String]) -> EndpointAddr {
    EndpointAddr::from_parts(
        id,
        addrs.iter().filter_map(|text| parse_transport_addr(text)),
    )
}

/// The textual addresses of an endpoint address, direct ones first.
pub fn addr_texts(addr: &EndpointAddr) -> Vec<String> {
    let mut direct: Vec<String> = addr.ip_addrs().map(|a| a.to_string()).collect();
    direct.extend(addr.relay_urls().map(|url| url.to_string()));
    direct
}

/// One live path of a connection, as the screens and the transfer rule see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkPath {
    /// A direct UDP path.
    Direct(SocketAddr),
    /// Through a relay server, by URL.
    Relay(String),
}

/// The selected path of `connection` right now: where its data goes.
pub fn selected_path(connection: &Connection) -> Option<LinkPath> {
    let paths = connection.paths();
    let selected = paths.iter().find(|path| path.is_selected())?;
    link_path(selected.remote_addr())
}

pub fn link_path(addr: &TransportAddr) -> Option<LinkPath> {
    match addr {
        TransportAddr::Ip(addr) => Some(LinkPath::Direct(*addr)),
        TransportAddr::Relay(url) => Some(LinkPath::Relay(url.to_string())),
        _ => None,
    }
}

// ── Request/response protocols ───────────────────────────────────────────────

/// Framing of one request/response protocol. The shape is the one the
/// protocols already had: each side reads and writes over a byte stream.
#[async_trait]
pub trait Codec: Clone + Send + Sync + 'static {
    type Request: Send + 'static;
    type Response: Send + 'static;

    /// The ALPN that selects this protocol on a connection.
    const PROTOCOL: &'static str;

    async fn read_request<T>(&mut self, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send;

    async fn read_response<T>(&mut self, io: &mut T) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send;

    async fn write_request<T>(&mut self, io: &mut T, req: Self::Request) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send;

    async fn write_response<T>(&mut self, io: &mut T, res: Self::Response) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send;
}

/// Why a call failed, and whether the request could have run on the other side.
#[derive(Debug, Clone)]
pub enum CallError {
    /// Nothing reached the peer: no connection, or the stream did not open.
    BeforeRequest(String),
    /// The request left; the answer did not come back.
    AfterDispatch(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::BeforeRequest(error) | CallError::AfterDispatch(error) => f.write_str(error),
        }
    }
}

/// Runs one call over a fresh stream of `connection`.
pub async fn call_on<C: Codec>(
    connection: &Connection,
    mut codec: C,
    request: C::Request,
) -> Result<C::Response, CallError> {
    let (send, recv) = connection
        .open_bi()
        .await
        .map_err(|error| CallError::BeforeRequest(error.to_string()))?;
    let mut send = send.compat_write();
    codec
        .write_request(&mut send, request)
        .await
        .map_err(|error| CallError::AfterDispatch(error.to_string()))?;
    let mut send = send.into_inner();
    send.finish()
        .map_err(|error| CallError::AfterDispatch(error.to_string()))?;
    let mut recv = recv.compat();
    codec
        .read_response(&mut recv)
        .await
        .map_err(|error| CallError::AfterDispatch(error.to_string()))
}

/// Serves every request stream `connection` opens, each in its own task, until
/// the connection closes. `handler` answers one request.
pub async fn serve_connection<C, H, Fut>(connection: Connection, codec: C, handler: H)
where
    C: Codec,
    H: Fn(Connection, C::Request) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = C::Response> + Send + 'static,
{
    loop {
        let (send, recv) = match connection.accept_bi().await {
            Ok(streams) => streams,
            Err(error) => {
                debug!(
                    "{}: connection from {} ended: {error}",
                    C::PROTOCOL,
                    connection.remote_id().fmt_short()
                );
                return;
            }
        };
        let mut codec = codec.clone();
        let handler = handler.clone();
        let connection = connection.clone();
        tokio::spawn(async move {
            let mut recv = recv.compat();
            let request = match codec.read_request(&mut recv).await {
                Ok(request) => request,
                Err(error) => {
                    debug!(
                        "{}: unreadable request from {}: {error}",
                        C::PROTOCOL,
                        connection.remote_id().fmt_short()
                    );
                    return;
                }
            };
            let response = handler(connection.clone(), request).await;
            let mut send = send.compat_write();
            if let Err(error) = codec.write_response(&mut send, response).await {
                debug!("{}: answer not delivered: {error}", C::PROTOCOL);
                return;
            }
            let _ = send.into_inner().finish();
        });
    }
}

/// An inbound request/response protocol as an iroh protocol handler.
pub struct Serve<C, H> {
    codec: C,
    handler: H,
    on_connection: Option<Arc<dyn Fn(&Connection) + Send + Sync>>,
}

impl<C, H> Serve<C, H> {
    pub fn new(codec: C, handler: H) -> Self {
        Self {
            codec,
            handler,
            on_connection: None,
        }
    }

    /// Called once per accepted connection, before its first request: the
    /// transport tracks the link there.
    pub fn on_connection(mut self, hook: Arc<dyn Fn(&Connection) + Send + Sync>) -> Self {
        self.on_connection = Some(hook);
        self
    }
}

impl<C, H> std::fmt::Debug for Serve<C, H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Serve").finish_non_exhaustive()
    }
}

impl<C, H, Fut> iroh::protocol::ProtocolHandler for Serve<C, H>
where
    C: Codec,
    H: Fn(Connection, C::Request) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = C::Response> + Send + 'static,
{
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        if let Some(hook) = &self.on_connection {
            hook(&connection);
        }
        serve_connection(connection, self.codec.clone(), self.handler.clone()).await;
        Ok(())
    }
}

// ── Outbound connections ─────────────────────────────────────────────────────

/// Wait between redials per peer, after a dial that failed.
///
/// Without it, with the peer unreachable, every batch of cluster logs (every
/// 100 ms) and every call through the tunnel became a new dial, and the record
/// of each failure entered the next batch of logs - a self-sustaining loop.
/// The wait doubles on every failure, from `DIAL_BACKOFF_FLOOR` up to
/// `DIAL_BACKOFF_CEILING`, and an established connection resets it.
#[derive(Default)]
struct DialBackoff {
    peers: HashMap<EndpointId, (u32, Instant)>,
}

const DIAL_BACKOFF_FLOOR: Duration = Duration::from_millis(250);
const DIAL_BACKOFF_CEILING: Duration = Duration::from_secs(10);

impl DialBackoff {
    fn blocked(&self, peer: &EndpointId) -> bool {
        self.peers
            .get(peer)
            .is_some_and(|(_, until)| *until > Instant::now())
    }

    /// Records a failure and returns how many in a row there are by now.
    fn failed(&mut self, peer: EndpointId) -> u32 {
        let entry = self.peers.entry(peer).or_insert((0, Instant::now()));
        entry.0 = entry.0.saturating_add(1);
        let wait = DIAL_BACKOFF_FLOOR
            .saturating_mul(1u32 << entry.0.saturating_sub(1).min(8))
            .min(DIAL_BACKOFF_CEILING);
        entry.1 = Instant::now() + wait;
        entry.0
    }

    fn connected(&mut self, peer: &EndpointId) {
        self.peers.remove(peer);
    }
}

/// The node's endpoint and the connections it keeps open, per peer and ALPN.
pub struct P2p {
    endpoint: iroh::Endpoint,
    /// Addresses learned outside iroh (announces, pkarr, the operator), which
    /// the endpoint consults when it dials a key.
    book: iroh::address_lookup::MemoryLookup,
    connections: Mutex<HashMap<(EndpointId, &'static str), Connection>>,
    /// One dial at a time per peer and ALPN. Every announce of a peer not yet
    /// connected warms the tunnel, and every call dials too: without this,
    /// the dials in flight at once each became a connection, and only the
    /// last one stayed in `connections`.
    dialing: Mutex<HashMap<(EndpointId, &'static str), Arc<tokio::sync::Mutex<()>>>>,
    backoff: Mutex<DialBackoff>,
    /// Called once for each outbound connection established.
    on_connection: Mutex<Option<Arc<dyn Fn(&Connection) + Send + Sync>>>,
}

impl P2p {
    pub fn new(endpoint: iroh::Endpoint, book: iroh::address_lookup::MemoryLookup) -> Arc<Self> {
        Arc::new(Self {
            endpoint,
            book,
            connections: Mutex::new(HashMap::new()),
            dialing: Mutex::new(HashMap::new()),
            backoff: Mutex::new(DialBackoff::default()),
            on_connection: Mutex::new(None),
        })
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub fn set_on_connection(&self, hook: Arc<dyn Fn(&Connection) + Send + Sync>) {
        *self.on_connection.lock().expect("on_connection lock") = Some(hook);
    }

    /// Adds dialable addresses of `id` to the address book. Nothing is dialed.
    pub fn learn(&self, id: EndpointId, addrs: &[String]) {
        let addr = endpoint_addr(id, addrs);
        if !addr.is_empty() {
            self.book.add_endpoint_info(addr);
        }
    }

    /// The peer is backing off after failed dials: it is not worth dialing now.
    pub fn backing_off(&self, id: &EndpointId) -> bool {
        self.backoff.lock().expect("backoff lock").blocked(id)
    }

    /// A live connection with `id` for protocol `alpn`, if one is open.
    pub fn live(&self, id: EndpointId, alpn: &'static str) -> Option<Connection> {
        let mut connections = self.connections.lock().expect("connections lock");
        match connections.get(&(id, alpn)) {
            Some(connection) if connection.close_reason().is_none() => Some(connection.clone()),
            Some(_) => {
                connections.remove(&(id, alpn));
                None
            }
            None => None,
        }
    }

    /// Whether any connection with `id` is open, whatever the protocol.
    pub fn is_connected(&self, id: &EndpointId) -> bool {
        self.connections
            .lock()
            .expect("connections lock")
            .iter()
            .any(|((peer, _), connection)| peer == id && connection.close_reason().is_none())
    }

    /// The connection with `id` for `alpn`: the open one, or a new dial with
    /// `addrs` as extra hints. A peer backing off fails at once.
    pub async fn connect(
        &self,
        id: EndpointId,
        addrs: &[String],
        alpn: &'static str,
    ) -> Result<Connection, String> {
        if id == self.endpoint.id() {
            return Err("the destination is this node".to_string());
        }
        if let Some(connection) = self.live(id, alpn) {
            return Ok(connection);
        }
        if self.backing_off(&id) {
            return Err("peer on hold after dials that failed".to_string());
        }
        // Wait for a dial already in flight, then take what it left.
        let slot = self
            .dialing
            .lock()
            .expect("dialing lock")
            .entry((id, alpn))
            .or_default()
            .clone();
        let _dialing = slot.lock().await;
        if let Some(connection) = self.live(id, alpn) {
            return Ok(connection);
        }
        if self.backing_off(&id) {
            return Err("peer on hold after dials that failed".to_string());
        }
        self.learn(id, addrs);
        let dial = self
            .endpoint
            .connect(endpoint_addr(id, addrs), alpn.as_bytes());
        let result = match tokio::time::timeout(CONNECT_TIMEOUT, dial).await {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => Err(format!("no connection in {CONNECT_TIMEOUT:?}")),
        };
        match result {
            Ok(connection) => {
                self.backoff.lock().expect("backoff lock").connected(&id);
                if let Some(hook) = self
                    .on_connection
                    .lock()
                    .expect("on_connection lock")
                    .clone()
                {
                    hook(&connection);
                }
                let replaced = self
                    .connections
                    .lock()
                    .expect("connections lock")
                    .insert((id, alpn), connection.clone());
                // Only a closed connection is ever replaced, but if one is
                // still open it must not linger beside the new one.
                if let Some(old) = replaced {
                    old.close(0u32.into(), b"replaced");
                }
                Ok(connection)
            }
            Err(error) => {
                let failures = self.backoff.lock().expect("backoff lock").failed(id);
                // Only the 1st, 2nd, 4th, 8th... consecutive failure becomes a
                // log: the record also goes into the cluster's log batches, and
                // one line per dial would feed the very loop the backoff cuts.
                if failures.is_power_of_two() {
                    tracing::info!(
                        "p2p: dial to {} failed ({failures} in a row): {error}",
                        id.fmt_short()
                    );
                }
                Err(error)
            }
        }
    }

    /// A fresh bidirectional stream to `id` over `alpn`, for a protocol whose
    /// answer the caller reads as it arrives instead of as one framed value.
    pub async fn open_bi(
        &self,
        id: EndpointId,
        addrs: &[String],
        alpn: &'static str,
    ) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream), CallError> {
        let connection = self
            .connect(id, addrs, alpn)
            .await
            .map_err(CallError::BeforeRequest)?;
        match connection.open_bi().await {
            Ok(streams) => Ok(streams),
            Err(error) => {
                if connection.close_reason().is_some() {
                    self.connections
                        .lock()
                        .expect("connections lock")
                        .remove(&(id, alpn));
                }
                Err(CallError::BeforeRequest(error.to_string()))
            }
        }
    }

    /// One request/response call to `id`.
    pub async fn call<C: Codec>(
        &self,
        codec: C,
        id: EndpointId,
        addrs: &[String],
        request: C::Request,
    ) -> Result<C::Response, CallError> {
        let connection = self
            .connect(id, addrs, C::PROTOCOL)
            .await
            .map_err(CallError::BeforeRequest)?;
        let result = call_on(&connection, codec, request).await;
        if result.is_err() && connection.close_reason().is_some() {
            self.connections
                .lock()
                .expect("connections lock")
                .remove(&(id, C::PROTOCOL));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_id_is_the_node_public_key() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let id = secret_key(&signing).public();
        let pubkey_hex = hex::encode(signing.verifying_key().to_bytes());
        assert_eq!(id.to_string(), pubkey_hex);
        assert_eq!(parse_endpoint_id(&pubkey_hex), Some(id));
        assert_eq!(parse_endpoint_id(&format!("pk:{}", id.to_z32())), Some(id));
        assert_eq!(parse_endpoint_id("not a key"), None);
    }

    #[test]
    fn transport_addresses_round_trip_through_text() {
        let id = SecretKey::from_bytes(&[3u8; 32]).public();
        let texts = vec![
            "203.0.113.7:52777".to_string(),
            "[2001:db8::1]:52777".to_string(),
            "https://relay.example.org/".to_string(),
            "garbage".to_string(),
            "0.0.0.0:52777".to_string(),
        ];
        let addr = endpoint_addr(id, &texts);
        assert_eq!(addr.ip_addrs().count(), 2);
        assert_eq!(addr.relay_urls().count(), 1);
        assert_eq!(addr_texts(&addr).len(), 3);
        assert!(addr_texts(&addr)[0].parse::<SocketAddr>().is_ok());
    }

    #[test]
    fn a_relay_hint_names_a_host_never_an_ip() {
        assert!(relay_hint("https://relay.example.org").is_some());
        assert!(relay_hint("http://203.0.113.7:52777").is_none());
        assert!(relay_hint("https://[2001:db8::1]/").is_none());
        assert!(relay_hint("pk:abc").is_none());
    }

    #[test]
    fn only_http_urls_are_relays() {
        assert!(relay_url("https://relay.example.org").is_some());
        assert!(relay_url("http://203.0.113.7:52777").is_some());
        assert!(relay_url("pk:abc").is_none());
        assert!(relay_url("203.0.113.7:52777").is_none());
    }
}
