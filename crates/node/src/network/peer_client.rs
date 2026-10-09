//! One `reqwest::Client` per peer, because the verifier is per destination.
//!
//! Before this feature there was a single client for everything. With pinning by
//! node_id, the `rustls::ClientConfig` carries **which** node_id to expect, and it is
//! pinned when the client is built - so the client becomes per destination. Without
//! TLS it goes back to being a single one, because then there is nothing to pin.
//!
//! *Rejected alternative:* putting the node_id in the SNI (`<node_id>.pom`) and
//! resolving with `resolve_to_addrs`, which would allow a single client. reqwest's
//! resolutions are static at build time, so the gain evaporates in a cluster where
//! peers come and go.
//!
//! Everything here is synchronous on purpose: the cache is a `HashMap` under a `std`
//! lock, and no critical section crosses an `await`. An async lock would
//! force every peer to peer call site to become `async` because of a
//! hash table lookup.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tracing::debug;

/// Whether this node speaks TLS with the peers, visible from anywhere in the process.
///
/// It is global because the flag is a property **of the node**, not of a connection:
/// the URL builders of `PeerInfo` (`inference_candidates`, `gateway_url`)
/// need the scheme, and passing it as a parameter to each of them would spread an
/// argument across dozens of signatures to carry a single bit that never
/// differs between them. Two nodes in the same process only happen in tests.
static TLS_ENABLED: AtomicBool = AtomicBool::new(false);
static QUIC_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn tls_enabled() -> bool {
    TLS_ENABLED.load(Ordering::Relaxed)
}

pub fn set_tls_enabled(on: bool) {
    TLS_ENABLED.store(on, Ordering::Relaxed);
}

pub fn quic_enabled() -> bool {
    QUIC_ENABLED.load(Ordering::Relaxed)
}

pub fn set_quic_enabled(on: bool) {
    QUIC_ENABLED.store(on, Ordering::Relaxed);
}

/// Scheme in force for talking to a peer.
pub fn peer_scheme() -> &'static str {
    scheme(tls_enabled())
}

/// Lock that serializes the tests that read or write `TLS_ENABLED`.
///
/// The state is global to the process and the tests run in parallel: without this, a
/// test that turns the policy on makes another one, which builds a peer URL, see
/// `https://` where it expected `http://`. It has happened. Every test that touches
/// the global scheme takes this lock first.
#[cfg(test)]
pub static GLOBAL_POLICY: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A peer reachable through the swarm, so the control plane falls back to the tunnel
/// when the direct HTTP dial does not get through.
///
/// Behind NAT/CGNAT the direct dial never gets through, and admission depends on
/// an HTTP probe (`/handshake`): without this path, a peer behind NAT does not
/// enter the `peers` table even with hole punching working, and routing, UI
/// and pipeline stay blind to it.
#[derive(Debug, Clone)]
pub(crate) struct TunnelPeer {
    pub peer_id: libp2p::PeerId,
    pub addrs: Vec<String>,
}

impl TunnelPeer {
    /// From what travels in a `reply_to`: the `PeerId` and the multiaddresses of the
    /// gateway, with no `PeerInfo` around. `None` if the `PeerId` does not parse.
    pub(crate) fn from_parts(peer_id: &str, addrs: Vec<String>) -> Option<Self> {
        Some(Self {
            peer_id: peer_id.parse().ok()?,
            addrs,
        })
    }

    /// `None` when the peer does not announce the tunnel or has no valid `PeerId`.
    pub(crate) fn from_peer(peer: &crate::network::PeerInfo) -> Option<Self> {
        if !peer
            .inference_frame_capabilities
            .iter()
            .any(|capability| capability == "transport/libp2p-tunnel-v1")
        {
            return None;
        }
        let peer_id = peer.libp2p_peer_id.as_deref()?.parse().ok()?;
        Some(Self {
            peer_id,
            addrs: peer.libp2p_dialable_addrs(),
        })
    }
}

/// Minimum deadline of a call through the tunnel. The request may have to build a
/// circuit through a relay on the internet before leaving, and the deadlines of the
/// direct calls (`webui::PEER_TIMEOUT`, 4 s) do not account for that.
const TUNNEL_MIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Header with the `node_id` of whoever answered, stamped by the gateway and by the
/// inference server on every response (`stamp_node_id`).
pub const NODE_ID_HEADER: &str = "x-pom-node-id";

/// Stamps this process's `node_id` on the response.
pub(crate) async fn stamp_node_id(
    mut response: axum::response::Response,
) -> axum::response::Response {
    if let Some(value) = crate::infer_tunnel::local_node_id()
        .and_then(|id| axum::http::HeaderValue::from_str(id).ok())
    {
        response.headers_mut().insert(NODE_ID_HEADER, value);
    }
    response
}

/// `Some(who answered)` when the response carries the stamp of **another** node.
///
/// The IP of an address does not prove who is behind it: two nodes on a Docker
/// bridge with the same internal IP made salem, dialing ksia's internal
/// address, receive its own response. QUIC and HTTPS already pin the `node_id` in
/// the certificate; in cleartext it is this stamp that checks it. A response without
/// a stamp passes, because whoever answers may not be a node (a test, a proxy).
pub(crate) fn answered_by_other_node(
    headers: &reqwest::header::HeaderMap,
    expected: &str,
) -> Option<String> {
    let actual = headers.get(NODE_ID_HEADER)?.to_str().ok()?;
    (actual != expected).then(|| actual.to_string())
}

/// The header of a signed control command (`auth::CONTROL_SIGNATURE_HEADER`).
const CONTROL_SIGNATURE_HEADER: &str = "x-pom-control-signature";

/// Whether sending this same request again could be refused as a replay: a
/// signed command that changes state. The peer consumes the envelope's nonce
/// when it accepts the command, so a second copy of it gets `409 Conflict`.
fn consumes_a_control_nonce(request: &reqwest::Request) -> bool {
    request.headers().contains_key(CONTROL_SIGNATURE_HEADER)
        && !matches!(
            *request.method(),
            reqwest::Method::GET | reqwest::Method::HEAD | reqwest::Method::OPTIONS
        )
}

/// Sends a control call using the address list defined by the
/// caller, which may be empty to try only the tunnel. Useful when the
/// caller has already ordered the endpoints by the `RouteTable`'s failure memory.
///
/// A signed command that may have reached the peer is not sent again by
/// another address. The same envelope goes in every attempt, so after a
/// timeout on a connection that was established - the peer got the command and
/// is still executing it - the next address answered `409 Conflict` (replay),
/// and the caller reported that the machine refused a command it was in fact
/// carrying out. Only an attempt that never connected moves on.
pub(crate) async fn send_to_gateway_over<F>(
    peer: &crate::network::PeerInfo,
    bases: &[String],
    build: F,
) -> Result<reqwest::Response>
where
    F: Fn(&str) -> reqwest::RequestBuilder,
{
    let mut failures = Vec::new();
    for base in bases {
        let (client, request) = build(base).build_split();
        let request = request?;
        let single_use = consumes_a_control_nonce(&request);
        match client.execute(request).await {
            Ok(response) => match answered_by_other_node(response.headers(), &peer.node_id) {
                None => return Ok(response),
                Some(actual) => failures.push(format!(
                    "{base}: answered as node {}, not {}",
                    crate::network::short(&actual),
                    crate::network::short(&peer.node_id)
                )),
            },
            Err(error) if single_use && !error.is_connect() => {
                anyhow::bail!(
                    "the gateway of {} received the command through {base} and did not answer in time ({error}); the command may be running and was not resent",
                    crate::network::short(&peer.node_id)
                );
            }
            Err(error) => failures.push(format!("{base}: {error}")),
        }
    }
    if let Some(tunnel) = TunnelPeer::from_peer(peer) {
        match send_request_via_tunnel(&tunnel, build("http://tunnel.invalid")).await {
            Ok(response) => return Ok(response),
            Err(error) => failures.push(format!("libp2p tunnel: {error:#}")),
        }
    }
    anyhow::bail!(
        "the gateway of {} did not answer: {}",
        crate::network::short(&peer.node_id),
        if failures.is_empty() {
            "no announced address".to_string()
        } else {
            failures.join(", ")
        }
    )
}

/// The same request as `build`, carried through the tunnel. The response comes back
/// as an ordinary `reqwest::Response`, so the caller does not need to know the path.
pub(crate) async fn send_request_via_tunnel(
    tunnel: &TunnelPeer,
    build: reqwest::RequestBuilder,
) -> Result<reqwest::Response> {
    let request = build.build()?;
    let mut path = request.url().path().to_string();
    if let Some(query) = request.url().query() {
        path.push('?');
        path.push_str(query);
    }
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
        .collect();
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .map(<[u8]>::to_vec)
        .unwrap_or_default();
    let timeout = request
        .timeout()
        .copied()
        .unwrap_or(TUNNEL_MIN_TIMEOUT)
        .max(TUNNEL_MIN_TIMEOUT);
    let response = tunnel_gateway_call(
        tunnel,
        request.method().as_str(),
        &path,
        body,
        headers,
        timeout,
    )
    .await?;
    let response = http::Response::builder()
        .status(response.status)
        .body(response.body)?;
    Ok(reqwest::Response::from(response))
}

/// Response already read, because the tunnel does not produce a `reqwest::Response`.
#[derive(Debug, Clone)]
pub(crate) struct InternalResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl InternalResponse {
    pub(crate) fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub(crate) fn json<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

/// Dispatches a control plane call through the libp2p tunnel.
///
/// The destination is `TunnelTarget::Gateway`; the route still goes through the
/// closed list checked on the receiver (`infer_tunnel::gateway_path_is_allowed`).
pub(crate) async fn tunnel_gateway_call(
    tunnel: &TunnelPeer,
    method: &str,
    path: &str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    timeout: std::time::Duration,
) -> Result<InternalResponse> {
    let Some(sender) = crate::infer_tunnel::tunnel_sender() else {
        anyhow::bail!("the libp2p tunnel is not available in this process");
    };
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    sender
        .send(crate::infer_tunnel::TunnelOutbound {
            peer_id: tunnel.peer_id,
            addr_candidates: tunnel.addrs.clone(),
            request: crate::infer_tunnel::TunnelRequest::gateway(method, path, body, headers),
            reply: reply_tx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("the libp2p tunnel queue closed"))?;
    let response = tokio::time::timeout(timeout, reply_rx)
        .await
        .map_err(|_| anyhow::anyhow!("the libp2p tunnel did not answer in {timeout:?}"))?
        .map_err(|_| anyhow::anyhow!("the libp2p tunnel dropped the response"))??;
    Ok(InternalResponse {
        status: response.status,
        body: response.body,
    })
}

/// `https` when the cluster is encrypted, `http` when it is not.
pub fn scheme(tls: bool) -> &'static str {
    if tls {
        "https"
    } else {
        "http"
    }
}

/// Observable state of the peer's HTTP pool.
///
/// reqwest keeps the individual socket under its internal pool, so this
/// distinction identifies construction/reuse of the pinned client. When the
/// path uses the shared cleartext client, the reuse of a socket cannot
/// be proven at this layer and stays unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionState {
    New,
    Reused,
    Unknown,
}

impl ConnectionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reused => "reused",
            Self::Unknown => "unknown",
        }
    }
}

/// URL base for talking to a peer, with the scheme the flag dictates.
///
/// The IPv6 bracket is this function's responsibility: the cluster is IPv6 in
/// practice, and `http://2804:d55::1:8181` does not parse as a URL - the parser reads
/// the last group as the port.
pub fn peer_base_url(host: &str, port: u16, tls: bool) -> String {
    let host = if host.starts_with('[') || !host.contains(':') {
        host.to_string()
    } else {
        format!("[{host}]")
    };
    format!("{}://{host}:{port}", scheme(tls))
}

#[derive(Clone)]
pub struct PeerClients {
    timeout: Duration,
    tls: Arc<AtomicBool>,
    quic: Arc<AtomicBool>,
    quic_pool: crate::quic::ClientPool,
    plain: reqwest::Client,
    internal_plain: reqwest::Client,
    per_peer: Arc<RwLock<HashMap<String, reqwest::Client>>>,
    reply_routes: Arc<RwLock<HashMap<String, ReplyRouteState>>>,
    /// Peers whose direct path has already proven unreachable and that are now
    /// tried **first** through the tunnel. See `prefer_tunnel`.
    tunnel_first: Arc<RwLock<std::collections::HashSet<String>>>,
    /// When the direct path of a `tunnel_first` peer was last
    /// probed. See `claim_direct_reprobe`.
    direct_reprobe_at: Arc<RwLock<HashMap<String, Instant>>>,
    auth: Arc<RwLock<Option<crate::auth::AuthState>>>,
}

#[derive(Default)]
struct ReplyRouteState {
    preferred: Option<String>,
    /// **Consecutive** failures per address, reset by any success.
    streak: HashMap<String, u32>,
    /// When the address accumulated [`ROUTE_FAILURE_STREAK`] consecutive failures.
    failed: HashMap<String, Instant>,
}

/// How long an address stays at the **end** of the order after accumulating
/// consecutive failures. It is not exclusion - see [`PeerClients::route_order`].
///
/// It is the single failure memory window, `FAILURE_MEMORY`.
pub(crate) const ROUTE_FAILURE_COOLDOWN: Duration = FAILURE_MEMORY;

/// Single failure memory window of the inference transport: how long
/// an address, a QUIC endpoint, the opening of the persistent channel or a peer
/// that failed the admission probe goes without being paid for again.
///
/// All of them govern the same decision - whether it is worth paying again for a
/// path that failed -, and different deadlines for it left an already recovered path
/// out of use for inconsistent times: QUIC came back in 5 s, but the route and the
/// persistent channel stayed marked for 300 s. None of them is a wait; they
/// only avoid repeating an attempt that has just failed.
pub(crate) const FAILURE_MEMORY: Duration = Duration::from_secs(5);

/// How many **consecutive** failures an address has to accumulate before going to
/// the end of the order.
///
/// The ceiling of one attempt is not a quarantine criterion. A delivery that
/// exceeded the ceiling may just be slow - busy gateway, cold TLS handshake,
/// large batch, WAN - and a single sample does not tell "slow" from "dead". A
/// truly dead address accumulates all three in a few milliseconds, because
/// `CONNECT_TIMEOUT` cuts in first.
const ROUTE_FAILURE_STREAK: u32 = 3;

/// An announced address that swallows SYN cannot hand control of the
/// process to the operating system's timeout. The total request timeout
/// is still chosen by the caller, but the connection has its own ceiling -
/// sub-second, so a dead direct address becomes a fallback (to the Noise
/// tunnel) almost immediately, in the same spirit as QUIC's RTT budget.
/// Minimum interval between two probes of the direct path of a peer that
/// is being served through the tunnel (`claim_direct_reprobe`).
pub(crate) const DIRECT_REPROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Ceiling of a direct probe: connection plus `/health`. It runs in the
/// background, off the frame's path.
pub(crate) const DIRECT_REPROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// TCP connection liveness detection: a peer that went down with the connection open
/// becomes an error in ~4 s (1 s idle + 3 probes of 1 s) instead of waiting for the
/// frame's deadline. It is the TCP counterpart of `QUIC_MAX_IDLE`, and what replaces
/// the old fixed ceiling of 30 s per attempt - that ceiling also killed legitimate
/// longer prefill/decode. The kernel only accepts these values in whole seconds.
const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(1);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
const TCP_KEEPALIVE_RETRIES: u32 = 3;

fn with_liveness(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .tcp_keepalive(TCP_KEEPALIVE_IDLE)
        .tcp_keepalive_interval(TCP_KEEPALIVE_INTERVAL)
        .tcp_keepalive_retries(TCP_KEEPALIVE_RETRIES)
}

#[cfg(not(test))]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);

#[cfg(test)]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(50);

impl PeerClients {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            tls: Arc::new(AtomicBool::new(false)),
            quic: Arc::new(AtomicBool::new(false)),
            quic_pool: crate::quic::ClientPool::new(),
            plain: with_liveness(reqwest::Client::builder())
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(timeout)
                .tcp_nodelay(true)
                .build()
                .expect("an HTTP client without TLS cannot fail to build"),
            internal_plain: with_liveness(reqwest::Client::builder())
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(timeout)
                .tcp_nodelay(true)
                .default_headers(internal_node_headers())
                .build()
                .expect("an internal HTTP client without TLS cannot fail to build"),
            per_peer: Arc::new(RwLock::new(HashMap::new())),
            reply_routes: Arc::new(RwLock::new(HashMap::new())),
            tunnel_first: Arc::new(RwLock::new(std::collections::HashSet::new())),
            direct_reprobe_at: Arc::new(RwLock::new(HashMap::new())),
            auth: Arc::new(RwLock::new(None)),
        }
    }

    /// Clients of the route plan probes (`route_plan`): QUIC uses the
    /// probe's fixed budget instead of the one derived from the measured RTT.
    pub fn for_route_probes() -> Self {
        Self {
            quic_pool: crate::quic::ClientPool::for_probes(crate::route_plan::PROBE_TIMEOUT),
            ..Self::new(Duration::from_secs(10))
        }
    }

    /// Binds this client cache to the node's live configuration. The read is
    /// done per call, because the key can be regenerated through the API without
    /// restarting the process.
    pub fn set_auth_state(&self, auth: crate::auth::AuthState) {
        if let Ok(mut slot) = self.auth.write() {
            *slot = Some(auth);
        }
    }

    /// Return the shared peer credential for an internal inference request.
    /// Client API keys never cross this boundary.
    pub async fn internal_node_token(&self) -> Option<String> {
        let auth = self.auth.read().ok()?.clone()?;
        let config = auth.read().await;
        crate::auth::internal_node_token(&config)
    }

    pub async fn authorize_inference_request_for_session(
        &self,
        request: reqwest::RequestBuilder,
        session_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let request = match session_id {
            Some(id) if !id.is_empty() => request.header(crate::auth::INFERENCE_SESSION_HEADER, id),
            _ => request,
        };
        self.authorize_inference_request(request).await
    }

    /// Attaches the POM credential of an internal inference hop, **identical** to
    /// the one the libp2p tunnel and the QUIC client already send: the marker
    /// `x-pom-internal-node: 1` always, and `Authorization: Bearer <token>` when
    /// a `connection_key` is configured, including on the HTTP fallback of a
    /// public network. Without the
    /// marker, the peer's `require_inference_auth` refuses the persistent HTTP
    /// channel and the session lifecycle calls with 401/403 even
    /// on a private network; on a public network, the marker enables the bypass by
    /// secure transport and the bearer keeps the cleartext HTTP path authenticated.
    /// The tunnel worked only because it built these two headers by hand
    /// (`send_frame_via_tunnel_with_auth`).
    pub async fn authorize_inference_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        let request = request.header(crate::auth::INTERNAL_NODE_REQUEST_HEADER, "1");
        match self.internal_node_token().await {
            Some(token) => {
                request.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            }
            None => request,
        }
    }

    pub fn tls_enabled(&self) -> bool {
        self.tls.load(Ordering::Relaxed)
    }

    pub fn quic_enabled(&self) -> bool {
        self.quic.load(Ordering::Relaxed)
    }

    /// The node's effective preference for new inference traversals.
    ///
    /// The principal's policy is also kept in the global atomic to
    /// reach the inference server's lazy clients. Consulting both
    /// prevents a `PeerClients` created before the policy was adopted from still
    /// choosing the tunnel while the rest of the process already chooses QUIC.
    pub fn effective_quic_enabled(&self) -> bool {
        self.quic_enabled() || quic_enabled()
    }

    /// Updates the preference without touching TCP clients already open. The policy
    /// only chooses new calls; a traversal in progress keeps the channel
    /// it has already acquired.
    pub fn set_quic(&self, on: bool) {
        self.quic.store(on, Ordering::Relaxed);
    }

    pub async fn quic_post(
        &self,
        peer_id: &str,
        base_url: &str,
        quic_addresses: &[String],
        path: &str,
        body: Vec<u8>,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<crate::quic::QuicResponse> {
        self.quic_post_typed(
            peer_id,
            base_url,
            quic_addresses,
            path,
            "application/octet-stream",
            body,
            session_id,
            timeout,
        )
        .await
    }

    /// Like `quic_post`, but with an explicit `content-type`. The session lifecycle
    /// routes (`/v1/session/{release,rewind,rollback}`) use
    /// `application/json` and a `Json<T>` that refuses `octet-stream`; sending them
    /// over QUIC is what gives them the same secure transport treatment that
    /// decode already has, instead of a cleartext POST with no credential that the peer
    /// rejects with 401 on a public network.
    #[allow(clippy::too_many_arguments)]
    pub async fn quic_post_typed(
        &self,
        peer_id: &str,
        base_url: &str,
        quic_addresses: &[String],
        path: &str,
        content_type: &str,
        body: Vec<u8>,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<crate::quic::QuicResponse> {
        let token = self.internal_node_token().await;
        self.quic_pool
            .request(
                peer_id,
                base_url,
                quic_addresses,
                path,
                content_type,
                token,
                session_id,
                body,
                timeout,
            )
            .await
    }

    /// Flips the flag and throws the clients away: a client built to speak
    /// cleartext will never speak TLS, and its connection pool is of no use either.
    pub fn set_tls(&self, on: bool) {
        let before = self.tls.swap(on, Ordering::Relaxed);
        if before != on {
            if let Ok(mut m) = self.per_peer.write() {
                m.clear();
            }
        }
    }

    /// Applies the cluster's policy: this cache **and** the global scheme of the URLs.
    ///
    /// It is the production path, and the only one that touches `TLS_ENABLED`. `set_tls`
    /// keeps to the cache only, so a unit test does not push the global
    /// scheme into the tests that run in parallel with it.
    pub fn apply_cluster_policy(&self, on: bool) {
        set_tls_enabled(on);
        self.set_tls(on);
    }

    /// Client without pinning, for a destination that is not a peer (loopback, Ollama).
    pub fn plain(&self) -> reqwest::Client {
        self.plain.clone()
    }

    /// Client dedicated to latency probes.
    ///
    /// The probe only needs to know whether the peer answered. It does not authenticate
    /// an inference session and therefore must not depend on the self-signed
    /// certificate or on the name present in the peer's certificate. The pinned
    /// client of `for_peer` remains the only path for inference
    /// data.
    pub fn probe(&self) -> Result<reqwest::Client> {
        if !self.tls_enabled() {
            return Ok(self.internal_plain.clone());
        }
        with_liveness(reqwest::Client::builder())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(self.timeout)
            .tcp_nodelay(true)
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .context("could not build the TLS client of the probe")
    }

    /// Client for talking to `node_id`, built on demand.
    ///
    /// Without TLS it returns the shared internal client, which marks the peer
    /// calls for the administrative boundary without changing their credential.
    pub fn for_peer(&self, node_id: &str) -> Result<reqwest::Client> {
        self.for_peer_with_state(node_id).map(|(client, _)| client)
    }

    /// Fetches an authenticated control route on all the addresses announced
    /// for the same peer.
    ///
    /// An old address may keep accepting TCP and answer 403 because it
    /// landed on another node (or on a ghost process), so an HTTP error
    /// response also has to let the probe try the next candidate. The
    /// TLS identity stays pinned to the peer_id for every attempt.
    /// `tunnel` is the last attempt, after all the direct addresses.
    /// For a peer behind NAT it is usually the **only** one that gets through; the route
    /// is still checked against the closed list on the receiver.
    pub(crate) async fn get_internal_from_candidates(
        &self,
        peer_id: &str,
        base_urls: &[String],
        path: &str,
        connection_key: &str,
        extra_headers: &reqwest::header::HeaderMap,
        tunnel: Option<&TunnelPeer>,
    ) -> Result<InternalResponse> {
        let http = self.for_peer(peer_id)?;
        let mut failures = Vec::with_capacity(base_urls.len() + 1);
        for base in base_urls {
            let url = format!("{base}{path}");
            let mut request = http.get(&url).headers(extra_headers.clone());
            if !connection_key.is_empty() {
                request = request.bearer_auth(connection_key);
            }
            match request.timeout(crate::webui::PEER_TIMEOUT).send().await {
                Ok(response) if response.status().is_success() => {
                    let status = response.status().as_u16();
                    let body = response
                        .bytes()
                        .await
                        .map(|b| b.to_vec())
                        .unwrap_or_default();
                    return Ok(InternalResponse { status, body });
                }
                Ok(response) => failures.push(format!("{url}: {}", response.status())),
                Err(error) => failures.push(format!("{url}: {error}")),
            }
        }
        if let Some(tunnel) = tunnel {
            let mut headers: Vec<(String, String)> = extra_headers
                .iter()
                .filter_map(|(name, value)| {
                    Some((name.to_string(), value.to_str().ok()?.to_string()))
                })
                .collect();
            if !connection_key.is_empty() {
                headers.push((
                    reqwest::header::AUTHORIZATION.to_string(),
                    format!("Bearer {connection_key}"),
                ));
            }
            match tunnel_gateway_call(
                tunnel,
                "GET",
                path,
                Vec::new(),
                headers,
                crate::webui::PEER_TIMEOUT,
            )
            .await
            {
                Ok(response) if response.is_success() => return Ok(response),
                Ok(response) => failures.push(format!("libp2p tunnel: {}", response.status)),
                Err(error) => failures.push(format!("libp2p tunnel: {error}")),
            }
        }
        anyhow::bail!(
            "no candidate answered the internal route {path}: {}",
            if failures.is_empty() {
                "no announced address".to_string()
            } else {
                failures.join(", ")
            }
        )
    }

    /// Client and atomic observation of the cache entry. The observation is made
    /// together with the insertion so that concurrent calls do not label as
    /// reused a client that has just been built.
    pub fn for_peer_with_state(&self, node_id: &str) -> Result<(reqwest::Client, ConnectionState)> {
        if !self.tls_enabled() {
            return Ok((self.internal_plain.clone(), ConnectionState::Unknown));
        }
        if let Ok(m) = self.per_peer.read() {
            if let Some(c) = m.get(node_id) {
                return Ok((c.clone(), ConnectionState::Reused));
            }
        }
        let cfg = crate::tls::client_config(node_id)?;
        let client = with_liveness(reqwest::Client::builder())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(self.timeout)
            .tcp_nodelay(true)
            .default_headers(internal_node_headers())
            .use_preconfigured_tls(cfg)
            .build()
            .context("could not build the TLS client of the peer")?;
        if let Ok(mut m) = self.per_peer.write() {
            m.insert(node_id.to_string(), client.clone());
        }
        Ok((client, ConnectionState::New))
    }

    /// URL base for this peer, already with the scheme in force.
    pub fn base_url(&self, host: &str, port: u16) -> String {
        peer_base_url(host, port, self.tls_enabled())
    }

    /// Peers with a pinned client in the cache.
    ///
    /// It exists for the test: the `Debug` of `reqwest::Client` is the same for two
    /// different clients, so comparing instances proves nothing. What
    /// matters to observe is the cache - who got in, and what flipping the flag
    /// threw away.
    pub fn cached_peers(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .per_peer
            .read()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Orders return addresses: first the route that has already answered, then
    /// the others, and last the ones that have been failing - from the one that failed
    /// longest ago to the most recent.
    ///
    /// The route is used once per decode batch. Without this memory,
    /// an IPv4/IPv6 that accepts the SYN but never answers is tried again on
    /// every batch and the cost of the timeout becomes a cost per token.
    ///
    /// **Never returns an empty list for a non-empty candidate list.**
    /// The failure memory reorders, it does not exclude. Excluding was the defect: with
    /// all the candidates in quarantine the caller tried no address at
    /// all, and without an attempt there is no error to report - only the gateway's
    /// deadline to expire (600 s) and a warning with an empty reason. What decides
    /// whether an address works is the real attempt; this function only picks the order
    /// in which it is paid for.
    /// The direct path to this peer has already failed entirely and the tunnel delivered.
    ///
    /// Behind NAT/CGNAT no announced address answers, and without this mark
    /// **every frame** burns the full direct ladder - QUIC timeout plus
    /// HTTP timeout, on every prefill and every decode - before reaching the
    /// only transport that works. The mark is not permanent: a direct
    /// success clears it, and a tunnel failure brings the direct ladder back on the
    /// next attempt, because the route may come back (new hole punch, a peer
    /// that changed networks).
    pub(crate) fn prefer_tunnel(&self, node_id: &str) -> bool {
        self.tunnel_first
            .read()
            .map(|set| set.contains(node_id))
            .unwrap_or(false)
    }

    pub(crate) fn remember_tunnel_win(&self, node_id: &str) {
        if let Ok(mut set) = self.tunnel_first.write() {
            if set.insert(node_id.to_string()) {
                debug!("{node_id}: the direct path does not answer; the tunnel becomes the first choice");
            }
        }
    }

    /// Reserves a probe of the direct path of a peer that is in
    /// `tunnel_first`, at most one every `DIRECT_REPROBE_INTERVAL`.
    ///
    /// Without this the mark was permanent in practice: only a direct success
    /// cleared it, but with the mark the direct path was never tried while the tunnel
    /// delivered. A single stumble left the peer on the tunnel (higher RTT) until the
    /// process restarted. The probe runs off the frame - see
    /// `relay::spawn_direct_reprobe` -, so it adds no wait to anyone.
    pub(crate) fn claim_direct_reprobe(&self, node_id: &str) -> bool {
        let Ok(mut at) = self.direct_reprobe_at.write() else {
            return false;
        };
        let now = Instant::now();
        match at.get(node_id) {
            Some(last) if now.duration_since(*last) < DIRECT_REPROBE_INTERVAL => false,
            _ => {
                at.insert(node_id.to_string(), now);
                true
            }
        }
    }

    pub(crate) fn remember_direct_win(&self, node_id: &str) {
        if let Ok(mut set) = self.tunnel_first.write() {
            if set.remove(node_id) {
                debug!("{node_id}: the direct path is back; no longer preferring the tunnel");
            }
        }
    }

    pub(crate) fn route_order(&self, key: &str, candidates: &[String]) -> Vec<String> {
        let Ok(mut routes) = self.reply_routes.write() else {
            return candidates.to_vec();
        };
        let state = routes.entry(key.to_string()).or_default();
        let now = Instant::now();
        let known = |url: &String| candidates.iter().any(|candidate| candidate == url);
        let expired: Vec<String> = state
            .failed
            .iter()
            .filter(|(url, failed_at)| {
                known(url) && now.duration_since(**failed_at) >= ROUTE_FAILURE_COOLDOWN
            })
            .map(|(url, _)| url.clone())
            .collect();
        state.failed.retain(|url, failed_at| {
            known(url) && now.duration_since(*failed_at) < ROUTE_FAILURE_COOLDOWN
        });
        for url in expired {
            state.streak.remove(&url);
        }
        state.streak.retain(|url, _| known(url));

        let mut ordered = Vec::with_capacity(candidates.len());
        if let Some(preferred) = state
            .preferred
            .as_ref()
            .filter(|url| known(url) && !state.failed.contains_key(*url))
        {
            ordered.push(preferred.clone());
        }
        for candidate in candidates {
            if !ordered.iter().any(|url| url == candidate) && !state.failed.contains_key(candidate)
            {
                ordered.push(candidate.clone());
            }
        }
        let mut quarantined: Vec<String> = Vec::new();
        for candidate in candidates {
            if !ordered.iter().any(|url| url == candidate)
                && !quarantined.iter().any(|url| url == candidate)
            {
                quarantined.push(candidate.clone());
            }
        }
        // Whoever failed longest ago has had more time to come back, so it is the
        // first in the queue of the degraded ones.
        quarantined.sort_by_key(|url| state.failed.get(url).copied());
        ordered.extend(quarantined);
        ordered
    }

    /// Records a failure on this address.
    ///
    /// The first failure only takes the address out of preference - it stays in the
    /// order, ahead of those that were already failing. Only [`ROUTE_FAILURE_STREAK`]
    /// consecutive failures send it to the end.
    pub(crate) fn remember_route_failure(&self, key: &str, url: &str) {
        if let Ok(mut routes) = self.reply_routes.write() {
            let state = routes.entry(key.to_string()).or_default();
            let streak = {
                let counter = state.streak.entry(url.to_string()).or_insert(0);
                *counter = counter.saturating_add(1);
                *counter
            };
            if streak >= ROUTE_FAILURE_STREAK {
                state.failed.insert(url.to_string(), Instant::now());
            }
            if state.preferred.as_deref() == Some(url) {
                state.preferred = None;
            }
        }
    }

    pub(crate) fn remember_route_success(&self, key: &str, url: &str) {
        if let Ok(mut routes) = self.reply_routes.write() {
            let state = routes.entry(key.to_string()).or_default();
            state.streak.remove(url);
            state.failed.remove(url);
            state.preferred = Some(url.to_string());
        }
    }

    /// How many consecutive failures this address has accumulated. It exists for the
    /// bench to prove the "demote, then degrade" ladder without waiting 300 s.
    #[cfg(test)]
    pub(crate) fn route_failure_streak(&self, key: &str, url: &str) -> u32 {
        self.reply_routes
            .read()
            .ok()
            .and_then(|routes| {
                routes
                    .get(key)
                    .and_then(|state| state.streak.get(url).copied())
            })
            .unwrap_or(0)
    }
}

fn internal_node_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        crate::auth::INTERNAL_NODE_REQUEST_HEADER,
        reqwest::header::HeaderValue::from_static("1"),
    );
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An address that answers as another node is not the peer: the control
    /// plane moves on to the next candidate.
    #[tokio::test]
    async fn a_base_answered_by_another_node_is_skipped() {
        async fn serve_as(node_id: &'static str) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let app = axum::Router::new().route(
                "/api/ui/node",
                axum::routing::get(move || async move { ([(NODE_ID_HEADER, node_id)], node_id) }),
            );
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://127.0.0.1:{port}")
        }
        let impostor = serve_as("eu-mesmo").await;
        let real = serve_as("alvo").await;
        let peer = crate::gateway::testkit::peer("alvo", "alvo", 8081);
        let client = reqwest::Client::new();

        let response = send_to_gateway_over(&peer, &[impostor.clone(), real.clone()], |base| {
            client.get(format!("{base}/api/ui/node"))
        })
        .await
        .expect("the second address is the peer");
        assert_eq!(response.text().await.unwrap(), "alvo");

        let only_impostor = send_to_gateway_over(&peer, &[impostor], |base| {
            client.get(format!("{base}/api/ui/node"))
        })
        .await
        .expect_err("only the impostor answered");
        assert!(
            only_impostor.to_string().contains("answered as node"),
            "{only_impostor}"
        );
    }

    /// A signed command that reached the peer and timed out is not sent again
    /// by the next address: the copy would be refused as a replay (409) while
    /// the peer is still carrying out the first one.
    #[tokio::test]
    async fn a_signed_command_that_timed_out_is_not_replayed_on_another_address() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        async fn serve(delay: Duration, calls: std::sync::Arc<AtomicUsize>) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let handler = move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(delay).await;
                    "ok"
                }
            };
            let app = axum::Router::new().route(
                "/api/ui/unload",
                axum::routing::post(handler.clone()).get(handler),
            );
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://127.0.0.1:{port}")
        }
        let slow_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let fast_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let slow = serve(Duration::from_secs(5), slow_calls.clone()).await;
        let fast = serve(Duration::ZERO, fast_calls.clone()).await;
        let peer = crate::gateway::testkit::peer("alvo", "alvo", 8081);
        let client = reqwest::Client::new();
        let bases = [slow, fast];

        let error = send_to_gateway_over(&peer, &bases, |base| {
            client
                .post(format!("{base}/api/ui/unload"))
                .timeout(Duration::from_millis(150))
                .header(CONTROL_SIGNATURE_HEADER, "{}")
        })
        .await
        .expect_err("the command arrived and did not answer in time");
        assert!(error.to_string().contains("was not resent"), "{error}");
        assert_eq!(slow_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fast_calls.load(Ordering::SeqCst), 0, "the copy would be a replay");

        // A read consumes no nonce and still falls through to the next address.
        let response = send_to_gateway_over(&peer, &bases, |base| {
            client
                .get(format!("{base}/api/ui/unload"))
                .timeout(Duration::from_millis(150))
                .header(CONTROL_SIGNATURE_HEADER, "{}")
        })
        .await
        .expect("the read moves on to the next address");
        assert_eq!(response.text().await.unwrap(), "ok");
        assert_eq!(fast_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn direct_probing_of_the_tunnel_is_rate_limited_by_interval() {
        let clients = PeerClients::new(Duration::from_secs(1));
        assert!(clients.claim_direct_reprobe("peer-a"));
        assert!(!clients.claim_direct_reprobe("peer-a"));
        assert!(clients.claim_direct_reprobe("peer-b"));

        clients.remember_tunnel_win("peer-a");
        assert!(clients.prefer_tunnel("peer-a"));
        clients.remember_direct_win("peer-a");
        assert!(!clients.prefer_tunnel("peer-a"));
    }

    #[test]
    fn the_url_scheme_follows_the_flag() {
        assert_eq!(scheme(true), "https");
        assert_eq!(scheme(false), "http");
    }

    /// The defect that hung the request for 600 s: with all the candidates
    /// in quarantine the order came out empty, the caller's `for` did not run and
    /// nobody tried any address. Without an attempt there is no error to
    /// report - only the gateway's deadline to expire.
    #[test]
    fn candidates_all_in_quarantine_still_produce_attempts() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://a".to_string(), "http://b".to_string()];
        for url in &candidates {
            for _ in 0..ROUTE_FAILURE_STREAK {
                c.remember_route_failure("reply:peer", url);
            }
        }
        let ordered = c.route_order("reply:peer", &candidates);
        assert_eq!(
            ordered.len(),
            candidates.len(),
            "the failure memory reorders, it does not exclude: {ordered:?}"
        );
        for candidate in &candidates {
            assert!(
                ordered.contains(candidate),
                "{candidate} disappeared from {ordered:?}"
            );
        }
    }

    /// Whoever failed longest ago has had more time to come back, and so it is the
    /// first of the degraded ones.
    #[test]
    fn among_degraded_ones_the_oldest_comes_first() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://a".to_string(), "http://b".to_string()];
        for _ in 0..ROUTE_FAILURE_STREAK {
            c.remember_route_failure("reply:peer", "http://a");
        }
        std::thread::sleep(Duration::from_millis(5));
        for _ in 0..ROUTE_FAILURE_STREAK {
            c.remember_route_failure("reply:peer", "http://b");
        }
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://a".to_string(), "http://b".to_string()]
        );
    }

    /// The mark is learning, not configuration: it turns on with a tunnel
    /// delivery, and turns off with a direct success.
    #[test]
    fn the_tunnel_preference_turns_on_and_off() {
        let c = PeerClients::new(Duration::from_secs(1));
        assert!(!c.prefer_tunnel("peer"));

        c.remember_tunnel_win("peer");
        assert!(c.prefer_tunnel("peer"));

        c.remember_direct_win("peer");
        assert!(!c.prefer_tunnel("peer"));
    }

    /// The mark is per peer: a peer behind NAT cannot push the reachable
    /// neighbor onto the tunnel.
    #[test]
    fn the_tunnel_preference_does_not_leak_between_peers() {
        let c = PeerClients::new(Duration::from_secs(1));
        c.remember_tunnel_win("atras-de-nat");

        assert!(c.prefer_tunnel("atras-de-nat"));
        assert!(!c.prefer_tunnel("na-lan"));
    }

    #[test]
    fn an_isolated_failure_does_not_degrade_the_route() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://lento".to_string(), "http://outro".to_string()];
        c.remember_route_success("reply:peer", "http://lento");
        c.remember_route_failure("reply:peer", "http://lento");
        assert_eq!(c.route_failure_streak("reply:peer", "http://lento"), 1);
        // Lost the preference, but still ahead of those that never answered?
        // No: losing the preference means going to the natural order of the candidates.
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://lento".to_string(), "http://outro".to_string()],
            "a single failure does not send the address to the end"
        );

        c.remember_route_failure("reply:peer", "http://lento");
        c.remember_route_failure("reply:peer", "http://lento");
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://outro".to_string(), "http://lento".to_string()],
            "three failures in a row send it to the end"
        );

        // And a success resets the count: the route becomes the preferred one again.
        c.remember_route_success("reply:peer", "http://lento");
        assert_eq!(c.route_failure_streak("reply:peer", "http://lento"), 0);
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://lento".to_string(), "http://outro".to_string()]
        );
    }

    /// The route that answered comes first, and that - not the quarantine - is what
    /// keeps the hot path from paying for a dead address again.
    #[test]
    fn the_route_that_answered_comes_first() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://morto".to_string(), "http://vivo".to_string()];
        c.remember_route_failure("reply:peer", "http://morto");
        c.remember_route_success("reply:peer", "http://vivo");
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://vivo".to_string(), "http://morto".to_string()]
        );
    }

    /// A repeated candidate does not become a repeated attempt.
    #[test]
    fn a_repeated_candidate_appears_once() {
        let c = PeerClients::new(Duration::from_secs(600));
        let candidates = vec!["http://a".to_string(), "http://a".to_string()];
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://a".to_string()]
        );
        for _ in 0..ROUTE_FAILURE_STREAK {
            c.remember_route_failure("reply:peer", "http://a");
        }
        assert_eq!(
            c.route_order("reply:peer", &candidates),
            vec!["http://a".to_string()]
        );
    }

    /// With no candidate at all there is nothing to order - and the caller has to say
    /// so in so many words instead of an empty reason.
    #[test]
    fn without_candidates_the_order_is_empty() {
        let c = PeerClients::new(Duration::from_secs(600));
        assert!(c.route_order("reply:peer", &[]).is_empty());
    }

    #[test]
    fn a_peer_url_follows_the_flag_and_brackets_ipv6() {
        assert_eq!(
            peer_base_url("192.168.1.5", 8181, false),
            "http://192.168.1.5:8181"
        );
        assert_eq!(
            peer_base_url("192.168.1.5", 8181, true),
            "https://192.168.1.5:8181"
        );
        // IPv6 without brackets becomes an invalid URL, and the cluster is IPv6 in practice.
        assert_eq!(
            peer_base_url("2804:d55::1", 8181, true),
            "https://[2804:d55::1]:8181"
        );
        // Already bracketed, it does not get a second pair.
        assert_eq!(
            peer_base_url("[2804:d55::1]", 8181, true),
            "https://[2804:d55::1]:8181"
        );
    }

    /// Without TLS, every peer shares a single client - that is the behavior that
    /// existed before this feature, and it does not cost a new connection per peer.
    #[test]
    fn without_tls_nobody_enters_the_cache() {
        let c = PeerClients::new(Duration::from_secs(600));
        assert!(!c.tls_enabled());
        c.for_peer("aaaa").unwrap();
        c.for_peer("bbbb").unwrap();
        assert!(
            c.cached_peers().is_empty(),
            "without TLS everyone uses the shared client"
        );
    }

    #[tokio::test]
    async fn a_peer_client_marks_control_and_plain_does_not() {
        async fn capture_headers(client: reqwest::Client) -> String {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                String::from_utf8_lossy(&request).into_owned()
            });

            client
                .get(format!("http://{address}/health"))
                .send()
                .await
                .unwrap();
            server.await.unwrap()
        }

        let c = PeerClients::new(Duration::from_secs(600));
        let internal = capture_headers(c.for_peer("peer-a").unwrap()).await;
        assert!(internal.contains("x-pom-internal-node: 1"));

        let external = capture_headers(c.plain()).await;
        assert!(!external.contains("x-pom-internal-node:"));
    }

    #[tokio::test]
    async fn http_fallback_preserves_connection_authorization_on_public_network() {
        let state = crate::gateway::testkit::state_with("cluster-secret", vec![]).await;
        {
            let mut config = state.auth.write().await;
            config.connection_key = "cluster-secret".into();
            config.network.visibility = crate::config::NetworkVisibility::Public;
        }
        let clients = PeerClients::new(Duration::from_secs(5));
        clients.set_auth_state(state.auth.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let read = stream.read(&mut chunk).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = seen_tx.send(String::from_utf8_lossy(&request).into_owned());
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });

        let request = clients
            .for_peer("peer-public")
            .unwrap()
            .post(format!("http://{address}/v1/prefill"))
            .body("prefill-frame");
        let response = clients
            .authorize_inference_request_for_session(request, Some("session-public"))
            .await
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let request = seen_rx.await.unwrap().to_ascii_lowercase();
        server.await.unwrap();

        assert!(request.contains("authorization: bearer cluster-secret"));
        assert!(request.contains("x-pom-internal-node: 1"));
        assert!(request.contains("x-pom-inference-session: session-public"));
    }

    /// With TLS, each peer needs its own, because the verifier is pinned when the
    /// client is built and carries the expected node_id.
    #[test]
    fn with_tls_each_peer_has_its_own_and_it_is_reused() {
        let c = PeerClients::new(Duration::from_secs(600));
        c.set_tls(true);
        let (_, first) = c.for_peer_with_state("aaaa").unwrap();
        let (_, second) = c.for_peer_with_state("aaaa").unwrap();
        let (_, third) = c.for_peer_with_state("bbbb").unwrap();
        assert_eq!(first, ConnectionState::New);
        assert_eq!(second, ConnectionState::Reused);
        assert_eq!(third, ConnectionState::New);
        assert_eq!(
            c.cached_peers(),
            vec!["aaaa".to_string(), "bbbb".to_string()]
        );
    }

    /// Flipping the flag throws the clients away: a client built to speak
    /// cleartext will never speak TLS, and its connection pool is of no use either.
    #[test]
    fn flipping_the_flag_empties_the_cache() {
        let c = PeerClients::new(Duration::from_secs(600));
        c.set_tls(true);
        c.for_peer("aaaa").unwrap();
        assert_eq!(c.cached_peers(), vec!["aaaa".to_string()]);
        c.set_tls(false);
        assert!(c.cached_peers().is_empty());
    }
}
