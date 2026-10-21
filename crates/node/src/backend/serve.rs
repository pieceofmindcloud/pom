//! One listener, two protocols.
//!
//! The server peeks at the connection's first byte without consuming it
//! (`peek`) and decides: `0x16` is a TLS handshake, anything else is plain
//! HTTP. Both land on the same router.
//!
//! Why a single port, instead of a TLS port beside it: no announcement changes,
//! no new field enters `PeerInfo` (which is already at the MTU limit of
//! `PeerList`), no new firewall rule - and, what matters most, **the
//! switchover does not have to be simultaneous**. While some peers already
//! speak TLS and others do not yet, this server serves both.

use anyhow::Result;
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tower::Service;
use tracing::{debug, warn};

/// Real address of the socket that brought the request. Administrative
/// authentication uses this to keep localhost mode off the network, without
/// trusting headers the client can forge.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerAddr(pub SocketAddr);

/// HTTP/2 and HTTP/3 carry the host in the `:authority` pseudo-header, never
/// in `Host`. Every check that reads `Host` - the plugin proxy's same-origin
/// fence, the session cookie's name - would see no host over https and refuse
/// the browser, so the authority becomes the `Host` a request without one has.
pub(crate) fn host_from_authority<B>(req: &mut hyper::Request<B>) {
    if req.headers().contains_key(hyper::header::HOST) {
        return;
    }
    let Some(host) = req
        .uri()
        .authority()
        .and_then(|authority| hyper::header::HeaderValue::from_str(authority.as_str()).ok())
    else {
        return;
    };
    req.headers_mut().insert(hyper::header::HOST, host);
}

/// The origin that counts for authentication: a request the p2p tunnel
/// delivers over loopback, with this process's mark, counts as remote.
///
/// Without this, any peer of the endpoint that reached a route through the tunnel
/// would be "localhost" to `AdminAuth` in localhost mode - administration
/// without a password. The header always leaves the request, marked or not, so
/// that no route sees it.
pub(crate) fn request_origin<B>(peer: SocketAddr, req: &mut hyper::Request<B>) -> SocketAddr {
    let tunnel_peer = req
        .headers_mut()
        .remove(crate::infer_tunnel::TUNNEL_PEER_HEADER);
    let Some(mark) = req
        .headers_mut()
        .remove(crate::infer_tunnel::TUNNEL_ORIGIN_HEADER)
    else {
        return peer;
    };
    if crate::auth::is_loopback_addr(peer) && crate::infer_tunnel::is_tunnel_mark(mark.as_bytes()) {
        // The forwarder names the PeerId the swarm authenticated (Noise);
        // only a marked request is believed, since the mark is this
        // process's secret.
        if let Some(caller) = tunnel_peer
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .and_then(TransportPeer::from_endpoint_id)
        {
            req.extensions_mut().insert(caller);
        }
        TUNNELED_ORIGIN
    } else {
        peer
    }
}

/// The node at the other end of the transport, proven by it: the client
/// certificate of a TLS or QUIC connection, or the endpoint id of a p2p
/// tunnel. It is what a node-to-node call is authorized by
/// (`auth::validate_internal_node_headers`); the connection key alone is
/// held by every member, and a removed one keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransportPeer {
    pub node_id: String,
    pub pubkey: String,
}

impl TransportPeer {
    /// From a peer's certificate: the Ed25519 key it carries.
    pub(crate) fn from_cert(der: &[u8]) -> Option<Self> {
        let (_, parsed) = x509_parser::parse_x509_certificate(der).ok()?;
        let bits = parsed.public_key().subject_public_key.data.as_ref();
        let key: [u8; 32] = bits.try_into().ok()?;
        Self::from_key(key)
    }

    /// From an endpoint id, which is the node's Ed25519 key.
    pub(crate) fn from_endpoint_id(endpoint_id: &str) -> Option<Self> {
        Self::from_key(*crate::p2p::parse_endpoint_id(endpoint_id)?.as_bytes())
    }

    fn from_key(key: [u8; 32]) -> Option<Self> {
        let key = ed25519_dalek::VerifyingKey::from_bytes(&key).ok()?;
        Some(Self {
            node_id: crate::identity::NodeId::from_pubkey(&key).0,
            pubkey: hex::encode(key.as_bytes()),
        })
    }
}

/// Address under which a request coming from the tunnel shows up in
/// `PeerAddr`: it is not loopback, and that is all authentication looks at.
pub(crate) const TUNNELED_ORIGIN: SocketAddr = SocketAddr::V4(std::net::SocketAddrV4::new(
    std::net::Ipv4Addr::UNSPECIFIED,
    0,
));

/// Whether the current request came through the TLS branch of the listener.
/// The admin session middleware uses the real transport state to decide if a
/// Secure cookie is safe, never a client-controlled header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TransportSecurity(pub bool);

/// What to do with a plain text connection coming from outside, with the
/// policy on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlainPolicy {
    /// A port that also serves people: answers 307 to `https://` on the same
    /// port, so the operator is not locked out of the dashboard.
    Redirect,
    /// A port exclusively for peers (the inference server): the connection is
    /// closed. There is no browser to accommodate here, and redirecting peer
    /// traffic would only trade a clear failure for a confusing one.
    Refuse,
}

/// What to answer to a request that arrived in plain text when the cluster
/// requires TLS.
#[derive(Debug, PartialEq)]
pub enum PlainAction {
    /// Tells the client to retry over `https://`.
    Redirect,
    /// A route only peers use: nothing to redirect, and `reqwest` follows
    /// redirects by default - returning 307 here would make an old peer try
    /// TLS without knowing how to verify anything and fail with an error that
    /// does not say why. The 403 does.
    Refuse,
}

/// Routes that only exist between peers, and that are therefore never redirected.
/// The transfer routes the screen calls (`jobs`, `offers`, `pull`, `send`) are
/// not among them: `POM_INSECURE_ADMIN_UI` must serve them to the browser.
pub fn plain_action(path: &str) -> PlainAction {
    const PEER_PREFIXES: [&str; 6] = [
        "/api/transfer/manifest",
        "/api/transfer/pom",
        "/api/transfer/download",
        "/v1/relay-result",
        "/v1/prefill",
        "/v1/step",
    ];
    const PEER_EXACT: [&str; 2] = ["/api/transfer/offer", "/api/transfer/offers/answer"];
    if PEER_PREFIXES.iter().any(|p| path.starts_with(p)) || PEER_EXACT.contains(&path) {
        PlainAction::Refuse
    } else {
        PlainAction::Redirect
    }
}

/// Configuration of the HTTP/3 listener, independent of the TLS-over-TCP policy.
pub type QuicServerConfig = Arc<h3_quinn::quinn::ServerConfig>;

/// Derives the self-signed configuration of the QUIC endpoint from the node's
/// identity. QUIC always carries its own TLS 1.3, even when TCP speaks plain HTTP.
pub fn quic_server_config(id: &crate::identity::Identity) -> Result<QuicServerConfig> {
    crate::quic::server_config(id)
}

/// What the server needs to speak TLS, and the state it publishes back.
#[derive(Clone)]
pub struct TlsServer {
    pub configs: crate::tls::ServerConfigs,
    /// What to do with remote plain text when the policy is on.
    pub plain: PlainPolicy,
    /// With `true`, plain text from outside loopback is refused.
    pub strict: Arc<AtomicBool>,
}

/// The TLS handshake of the connection a request arrived on, in the request's
/// extensions. This is where the `tls_handshake_ms` of the inference telemetry
/// comes from.
///
/// It belongs to the connection, not to the server: the first request on the
/// connection takes it and every later one reads zero, because only the first
/// paid for it. A server-wide "last handshake" was read by every hop on every
/// token - reused connections, the persistent channel's frames, even a
/// browser's handshake - and summed by the gateway into seconds that never
/// happened.
#[derive(Clone, Debug)]
pub struct TlsHandshake(Arc<AtomicU64>);

impl TlsHandshake {
    fn new(elapsed: Duration) -> Self {
        // Rounded up: a loopback handshake can take less than a millisecond,
        // and zero must keep meaning "this request opened no connection".
        let ms = elapsed.as_micros().div_ceil(1_000).max(1) as u64;
        Self(Arc::new(AtomicU64::new(ms)))
    }

    /// The handshake's duration in ms, for the first caller only; zero after.
    pub fn take_ms(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

impl TlsServer {
    /// Builds the server state from the node's identity.
    ///
    /// `sans` are the addresses through which this node is reached, and they
    /// go into the certificate the **browser** receives - without them the
    /// browser warning would be "wrong host", which makes the operator
    /// distrust the machine instead of the certificate. See
    /// `tls::browser_certificate`.
    pub fn from_identity(
        id: &crate::identity::Identity,
        strict: bool,
        sans: &[String],
    ) -> Result<Self> {
        Ok(Self {
            configs: crate::tls::server_configs(id, sans)?,
            plain: PlainPolicy::Redirect,
            strict: Arc::new(AtomicBool::new(strict)),
        })
    }

    /// The same configuration, for a port only peers use.
    pub fn peers_only(&self) -> Self {
        Self {
            plain: PlainPolicy::Refuse,
            ..self.clone()
        }
    }

    /// Turns the refusal of remote plain text on or off.
    pub fn set_strict(&self, on: bool) {
        self.strict.store(on, Ordering::Relaxed);
    }
}

/// Addresses that go into the browser certificate.
///
/// The machine's local ones, the address pinned by the operator (if any) and
/// the hostname. Loopback is included because opening the dashboard on the
/// machine itself is the most common case of all.
pub fn browser_sans(pinned: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = vec!["127.0.0.1".into(), "::1".into()];
    if let Some(p) = pinned {
        names.push(
            crate::network::ip_host(p)
                .trim_matches(['[', ']'])
                .to_string(),
        );
    }
    for addr in crate::addrs::filter_candidates(crate::addrs::local_addrs(), None) {
        names.push(
            crate::network::ip_host(&addr)
                .trim_matches(['[', ']'])
                .to_string(),
        );
    }
    if let Ok(h) = hostname_local() {
        names.push(h);
    }
    names.retain(|n| !n.is_empty());
    names.sort();
    names.dedup();
    names
}

fn hostname_local() -> Result<String> {
    let output = crate::console::hidden(&mut std::process::Command::new("hostname")).output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `0x16` is TLS's `ContentType::Handshake`; no HTTP method starts like that.
pub fn wants_tls(first_byte: u8) -> bool {
    first_byte == 0x16
}

/// Plain text is always accepted from loopback, and from outside it only when
/// the flag is off.
pub fn plain_allowed(peer: SocketAddr, strict: bool) -> bool {
    if !strict {
        return true;
    }
    match peer.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6
                    .to_ipv4_mapped()
                    .map(|v4| v4.is_loopback())
                    .unwrap_or(false)
        }
    }
}

/// `POM_INSECURE_ADMIN_UI=1` serves the admin UI in plain HTTP to remote
/// browsers too, for development machines reached by IP with no trusted
/// certificate. Peer-only routes (`plain_action`) are still refused, and a
/// node-to-node call in plain text still carries no `TransportPeer`, so it
/// proves no membership.
pub fn insecure_admin_ui_env() -> String {
    crate::brand::env_var("INSECURE_ADMIN_UI")
}

fn insecure_admin_ui() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        let on = std::env::var(insecure_admin_ui_env()).is_ok_and(|v| v == "1");
        if on {
            warn!(
                "{}=1: the admin UI is served in plain HTTP to remote browsers",
                insecure_admin_ui_env()
            );
        }
        on
    })
}

/// Serves `app` on `listener`, accepting TLS and plain text per the policy.
///
/// `tls: None` is the behavior from before this feature, and it is what the
/// `testkit` and the test servers on `127.0.0.1:0` use.
#[cfg(test)]
pub(crate) async fn serve(
    listener: TcpListener,
    app: Router,
    tls: Option<TlsServer>,
) -> Result<()> {
    serve_with_shutdown(listener, app, tls, std::future::pending::<()>()).await
}

/// How long a live connection has to finish on its own after the shutdown
/// signal, before it is aborted.
const DRAIN_DEADLINE: Duration = Duration::from_secs(3);

/// The QUIC endpoint may be waiting on a connection that never ends. The
/// supervisor cannot turn that case into an endless wait during the reload;
/// after the deadline, aborting the task brings the endpoint down and lets the
/// new generation be created. The value is slightly larger than the QUIC idle
/// timeout of 1,2 s, with margin for the endpoint to close.
const QUIC_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

/// Like `serve`, but it stops accepting when `shutdown` resolves and **closes
/// the connections that are left** instead of leaving them loose.
///
/// The difference matters for memory, not for hygiene: each connection lives
/// in a task that carries a copy of the `Router`, and the inference server's
/// `Router` carries the model's weights. Before this the tasks were detached
/// (`tokio::spawn` with no owner), so a stalled keep-alive connection - the
/// gateway pool leaves several - kept the engine alive after the `undeploy`
/// and the VRAM never came back. See `LlmEngine::unload`.
#[cfg(test)]
pub(crate) async fn serve_with_shutdown(
    listener: TcpListener,
    app: Router,
    tls: Option<TlsServer>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_with_quic(listener, app, tls, None, None, shutdown).await
}

/// Like `serve_with_shutdown`, but with an HTTP/3 endpoint independent of the
/// TLS-over-TCP configuration. The inference port uses this variant so that
/// `tls_required = false` does not prevent QUIC.
pub async fn serve_with_quic(
    listener: TcpListener,
    app: Router,
    tls: Option<TlsServer>,
    quic: Option<QuicServerConfig>,
    quic_port: Option<u16>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    // This is a one-shot lifecycle signal, not a broadcast event. Unlike
    // `Notify::notify_waiters`, a oneshot keeps the value when the QUIC task
    // has not reached its first poll yet.
    let (quic_shutdown_tx, quic_shutdown_rx) = tokio::sync::oneshot::channel();
    let quic_task = quic.and_then(|config| {
        let port = quic_port?;
        let router = app.clone();
        Some(tokio::spawn(async move {
            let result = crate::quic::serve(port, router, config, async move {
                let _ = quic_shutdown_rx.await;
            })
            .await;
            if let Err(error) = &result {
                warn!(
                    port,
                    "HTTP/3 endpoint unavailable, keeping the TCP fallback: {error}"
                );
            }
            result
        }))
    });
    let acceptor = tls.as_ref().map(|t| t.configs.clone());

    let mut conns = tokio::task::JoinSet::new();
    let mut shutdown = std::pin::pin!(shutdown);

    let reason = loop {
        let (stream, peer) = tokio::select! {
            _ = &mut shutdown => break "shutdown requested",
            // Connections that already ended leave the set here; without this
            // the `JoinSet` would grow for the server's whole life.
            Some(_) = conns.join_next(), if !conns.is_empty() => continue,
            accepted = listener.accept() => match accepted {
                Ok(v) => v,
                Err(e) => {
                    warn!("accept failed: {e}");
                    continue;
                }
            },
        };
        // Nagle holds a small write until the previous ACK; in SSE each token is
        // a small write. Same reason as the `tcp_nodelay(true)` that was on
        // `axum::serve`.
        let _ = stream.set_nodelay(true);

        let app = app.clone();
        let acceptor = acceptor.clone();
        let tls = tls.clone();

        conns.spawn(async move {
            if let Err(e) = handle_conn(stream, peer, app, acceptor, tls).await {
                debug!("connection from {peer} closed: {e}");
            }
        });
    };

    // A deadline for whoever is in the middle of a response; after it, abort.
    // One stalled in keep-alive never finishes on its own, and it is exactly
    // the one that was holding the memory.
    debug!("{reason}: draining {} connections", conns.len());
    let drain = async { while conns.join_next().await.is_some() {} };
    if tokio::time::timeout(DRAIN_DEADLINE, drain).await.is_err() {
        warn!("connections still alive after {DRAIN_DEADLINE:?}; closing");
        conns.shutdown().await;
    }
    let _ = quic_shutdown_tx.send(());
    if let Some(mut task) = quic_task {
        match tokio::time::timeout(QUIC_SHUTDOWN_DEADLINE, &mut task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => {
                warn!("HTTP/3 endpoint unavailable, keeping the TCP fallback: {error}")
            }
            Ok(Err(error)) => warn!("HTTP/3 task ended unexpectedly: {error}"),
            Err(_) => {
                warn!("HTTP/3 endpoint did not close in {QUIC_SHUTDOWN_DEADLINE:?}; aborting the task");
                task.abort();
                let _ = task.await;
            }
        }
    }
    Ok(())
}

async fn handle_conn(
    stream: TcpStream,
    peer: SocketAddr,
    app: Router,
    acceptor: Option<crate::tls::ServerConfigs>,
    tls: Option<TlsServer>,
) -> Result<()> {
    let mut first = [0u8; 1];
    // `peek` does not consume: whoever serves the connection still sees the byte.
    let n = stream.peek(&mut first).await?;
    if n == 0 {
        return Ok(());
    }
    let is_tls = wants_tls(first[0]);
    let strict = tls
        .as_ref()
        .map(|t| t.strict.load(Ordering::Relaxed))
        .unwrap_or(false);

    let plain_remote = !is_tls && !plain_allowed(peer, strict);
    let policy = tls.as_ref().map(|t| t.plain).unwrap_or(PlainPolicy::Refuse);
    let insecure_ui = plain_remote && policy == PlainPolicy::Redirect && insecure_admin_ui();
    if plain_remote && !insecure_ui {
        if policy == PlainPolicy::Refuse {
            warn!("refused plain text connection from {peer}: the cluster is on TLS");
            return Ok(());
        }
        // A port that also serves people: instead of slamming the door on the
        // operator, tell the browser to retry over https. See `plain_action`
        // for what is still refused.
        debug!("redirecting the plain connection from {peer} to https");
        let svc = hyper::service::service_fn(redirect_to_https);
        return Builder::new(TokioExecutor::new())
            .serve_connection(TokioIo::new(stream), svc)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"));
    }

    let service = move |caller: Option<TransportPeer>, handshake: Option<TlsHandshake>| {
        hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let mut app = app.clone();
            let caller = caller.clone();
            let handshake = handshake.clone();
            async move {
                if insecure_ui && plain_action(req.uri().path()) == PlainAction::Refuse {
                    return Ok(refuse_plain().map(axum::body::Body::from));
                }
                let mut req = req;
                host_from_authority(&mut req);
                let origin = request_origin(peer, &mut req);
                req.extensions_mut().insert(PeerAddr(origin));
                req.extensions_mut().insert(TransportSecurity(is_tls));
                if let Some(caller) = caller {
                    req.extensions_mut().insert(caller);
                }
                if let Some(handshake) = handshake {
                    req.extensions_mut().insert(handshake);
                }
                Service::call(&mut app, req).await
            }
        })
    };

    match (is_tls, acceptor) {
        (true, Some(configs)) => {
            let started = Instant::now();
            // The configuration is chosen from the ClientHello: a peer is
            // asked for its node certificate, a browser for nothing - asking
            // a browser would make it offer the operator's certificates.
            let start =
                tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream)
                    .await?;
            let config = if crate::tls::wants_peer(start.client_hello().alpn()) {
                configs.peer
            } else {
                configs.browser
            };
            let tls_stream = start.into_stream(config).await?;
            let handshake = TlsHandshake::new(started.elapsed());
            let caller = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|chain| chain.first())
                .and_then(|cert| TransportPeer::from_cert(cert.as_ref()));
            let svc = service(caller, Some(handshake));
            Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(tls_stream), svc)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        (true, None) => {
            // The client spoke TLS to a node that has no certificate. There is
            // nothing to serve, and leaving the connection hanging would hide why.
            warn!("{peer} tried TLS but this node has no TLS configured");
        }
        (false, _) => {
            Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(stream), service(None, None))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    Ok(())
}

/// The answer for whoever spoke plain text on a port that requires TLS,
/// decided without touching any socket: `(status, Location)`.
///
/// The destination is the **same port** - the server serves both protocols on
/// it. The host comes from the header because it is the name the operator
/// typed; rebuilding it from the socket would give the IP, and would swap that
/// name for an address that may not even be in the certificate.
pub fn plain_reply(host: &str, path_and_query: &str) -> (u16, Option<String>) {
    let path = path_and_query.split('?').next().unwrap_or("/");
    if plain_action(path) == PlainAction::Refuse {
        return (403, None);
    }
    if host.is_empty() {
        // Without `Host` there is nowhere to send it, and a broken `Location`
        // is worse than a refusal: the client follows it and fails far from here.
        return (403, None);
    }
    // 307, not 301: a browser caches a 301 for good, and would keep sending
    // the operator to https after `POM_INSECURE_ADMIN_UI` is turned on.
    (307, Some(format!("https://{host}{path_and_query}")))
}

/// The 403 a peer-only route gets in plain text, even with the admin UI open.
fn refuse_plain() -> hyper::Response<String> {
    hyper::Response::builder()
        .status(403)
        .body("this cluster requires TLS between nodes; repeat the call over https\n".to_string())
        .expect("response is constructible")
}

/// Answer for whoever spoke plain text on a port that requires TLS.
async fn redirect_to_https(
    req: hyper::Request<hyper::body::Incoming>,
) -> std::result::Result<hyper::Response<String>, std::convert::Infallible> {
    let host = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let target = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let (status, location) = plain_reply(&host, &target);
    let mut response = hyper::Response::builder().status(status);
    if let Some(l) = &location {
        response = response.header(hyper::header::LOCATION, l);
    }
    let body = if location.is_some() {
        String::new()
    } else {
        "this cluster requires TLS between nodes; repeat the call over https\n".to_string()
    };
    Ok(response.body(body).expect("response is constructible"))
}

#[cfg(test)]
mod integration {
    use super::*;
    use crate::identity::Identity;
    use axum::{
        body::Bytes,
        extract::State,
        http::StatusCode,
        routing::{get, post},
        Router,
    };
    use std::time::Duration;

    async fn start_server(id: &Identity, strict: bool) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/health", get(|| async { "ok" }));
        let tls = TlsServer::from_identity(id, strict, &["127.0.0.1".to_string()]).unwrap();
        tokio::spawn(async move {
            let _ = serve(listener, app, Some(tls)).await;
        });
        addr
    }

    fn clients() -> crate::peer_client::PeerClients {
        let c = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        c.set_tls(true);
        c
    }

    /// A server whose `/whoami` answers the node the transport proved, or
    /// `none`.
    async fn start_whoami_server(id: &Identity) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/whoami",
            get(|caller: Option<axum::Extension<TransportPeer>>| async move {
                caller.map_or_else(|| "none".to_string(), |caller| caller.0.node_id)
            }),
        );
        let tls = TlsServer::from_identity(id, true, &["127.0.0.1".to_string()]).unwrap();
        tokio::spawn(async move {
            let _ = serve(listener, app, Some(tls)).await;
        });
        addr
    }

    async fn whoami(server: &Identity, addr: std::net::SocketAddr, own: Option<&Identity>) -> String {
        let own = own.map(|id| crate::tls::node_certificate(id).unwrap());
        let cfg = crate::tls::client_config_as(&server.node_id.0, own.as_ref()).unwrap();
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(cfg)
            .build()
            .unwrap();
        http.get(format!("https://127.0.0.1:{}/whoami", addr.port()))
            .send()
            .await
            .expect("the handshake had to complete")
            .text()
            .await
            .unwrap()
    }

    /// A server whose `/handshake` answers the handshake its request reports,
    /// or `none` when the request carries none.
    async fn start_handshake_server(id: &Identity) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/handshake",
            get(|handshake: Option<axum::Extension<TlsHandshake>>| async move {
                handshake.map_or_else(|| "none".to_string(), |h| h.take_ms().to_string())
            }),
        );
        let tls = TlsServer::from_identity(id, true, &["127.0.0.1".to_string()]).unwrap();
        tokio::spawn(async move {
            let _ = serve(listener, app, Some(tls)).await;
        });
        addr
    }

    fn peer_http(server: &Identity, own: &Identity) -> reqwest::Client {
        let own = crate::tls::node_certificate(own).unwrap();
        let cfg = crate::tls::client_config_as(&server.node_id.0, Some(&own)).unwrap();
        reqwest::Client::builder()
            .use_preconfigured_tls(cfg)
            .build()
            .unwrap()
    }

    async fn reported_handshake(http: &reqwest::Client, addr: std::net::SocketAddr) -> u64 {
        http.get(format!("https://127.0.0.1:{}/handshake", addr.port()))
            .send()
            .await
            .expect("the handshake had to complete")
            .text()
            .await
            .unwrap()
            .parse()
            .expect("a TLS request carries its connection's handshake")
    }

    /// The handshake is a connection's cost: the request that opened the
    /// connection reports it, the ones reusing it from the pool report zero,
    /// and another connection's handshake never leaks into them. Reading a
    /// server-wide "last handshake" made every token of a generation repeat
    /// it, and the gateway summed 2823 copies into 33 s that never happened.
    #[tokio::test]
    async fn the_handshake_is_reported_once_by_the_connection_that_paid_it() {
        let server = Identity::generate();
        let caller = Identity::generate();
        let addr = start_handshake_server(&server).await;
        let pooled = peer_http(&server, &caller);

        assert!(reported_handshake(&pooled, addr).await >= 1);
        assert_eq!(reported_handshake(&pooled, addr).await, 0);

        let other = peer_http(&server, &caller);
        assert!(reported_handshake(&other, addr).await >= 1);
        assert_eq!(reported_handshake(&pooled, addr).await, 0);
        assert_eq!(reported_handshake(&other, addr).await, 0);
    }

    /// Plain HTTP opened no TLS connection, so there is no handshake to report.
    #[tokio::test]
    async fn a_plain_request_carries_no_handshake() {
        let server = Identity::generate();
        let addr = start_handshake_server(&server).await;
        let body = reqwest::get(format!("http://127.0.0.1:{}/handshake", addr.port()))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "none");
    }

    /// Mutual TLS: the server learns which node is calling from the client
    /// certificate, and that is what node-to-node calls are authorized by.
    #[tokio::test]
    async fn the_server_learns_the_calling_node_from_its_certificate() {
        let server = Identity::generate();
        let caller = Identity::generate();
        let addr = start_whoami_server(&server).await;
        assert_eq!(whoami(&server, addr, Some(&caller)).await, caller.node_id.0);
    }

    /// A peer connection without a client certificate still completes, and
    /// carries no identity: the routes that need one refuse it.
    #[tokio::test]
    async fn a_peer_without_a_certificate_carries_no_identity() {
        let server = Identity::generate();
        let addr = start_whoami_server(&server).await;
        assert_eq!(whoami(&server, addr, None).await, "none");
    }

    /// A browser offers no `pom/1`: it gets the browser certificate and is
    /// not asked for one of its own.
    #[tokio::test]
    async fn a_browser_is_not_asked_for_a_certificate() {
        let server = Identity::generate();
        let addr = start_whoami_server(&server).await;
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let body = http
            .get(format!("https://127.0.0.1:{}/whoami", addr.port()))
            .send()
            .await
            .expect("the browser handshake had to complete")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "none");
    }

    /// Plain HTTP from loopback is still served with TLS on: the operator
    /// opens the interface at `http://localhost` with no certificate warning.
    #[tokio::test]
    async fn loopback_plain_http_is_served_with_tls_on() {
        let server = Identity::generate();
        let addr = start_whoami_server(&server).await;
        let body = reqwest::get(format!("http://127.0.0.1:{}/whoami", addr.port()))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "none");
    }

    /// The tunnel names the caller by the endpoint id its handshake
    /// authenticated, which is the node key: it yields the same node_id.
    #[test]
    fn an_endpoint_id_yields_the_node_of_its_key() {
        let id = Identity::generate();
        let endpoint_id = crate::p2p::secret_key(&id.signing_key).public().to_string();
        let caller = TransportPeer::from_endpoint_id(&endpoint_id).unwrap();
        assert_eq!(caller.node_id, id.node_id.0);
        assert_eq!(caller.pubkey, id.public_key_hex());
    }

    /// The whole happy path: certificate derived from the identity, a client
    /// that pins the node_id, handshake, response.
    #[tokio::test]
    async fn a_pinned_client_talks_to_the_server() {
        let id = Identity::generate();
        let addr = start_server(&id, false).await;
        let http = clients().for_peer(&id.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("the handshake had to complete");
        assert!(r.status().is_success());
    }

    /// HTTP/3 must stay available when the inference TCP is on plain HTTP.
    /// The endpoint gets its own TLS 1.3 configuration and cannot depend on
    /// `TlsServer` or on `tls_required`.
    #[tokio::test]
    async fn quic_starts_without_a_tcp_tls_configuration() {
        let identity = Identity::generate();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route(
            "/test",
            post(|body: Bytes| async move { (StatusCode::OK, body) }),
        );
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let quic = quic_server_config(&identity).unwrap();
        let udp_probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let quic_port = udp_probe.local_addr().unwrap().port();
        drop(udp_probe);
        let server = tokio::spawn(async move {
            serve_with_quic(
                listener,
                app,
                None,
                Some(quic),
                Some(quic_port),
                async move {
                    let _ = stop_rx.await;
                },
            )
            .await
            .unwrap();
        });

        let response = crate::quic::ClientPool::new()
            .request(
                &identity.node_id.0,
                &format!("http://127.0.0.1:{port}"),
                &[format!("127.0.0.1:{quic_port}")],
                "/test",
                "application/octet-stream",
                None,
                None,
                b"http3-without-tcp-tls".to_vec(),
                Duration::from_secs(3),
            )
            .await
            .unwrap();

        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body, b"http3-without-tcp-tls");
        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }

    /// Two peers may be idle and receive the wake-up at the same time. A QUIC
    /// request hanging on each one cannot turn the shutdown of the previous
    /// generation into an endless wait.
    #[tokio::test]
    async fn two_idle_peers_can_close_quic_concurrently() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        #[derive(Clone)]
        struct RequestsInFlight {
            started: Arc<AtomicUsize>,
            all_started: Arc<Notify>,
        }

        async fn hold_request(
            State(state): State<RequestsInFlight>,
            _body: Bytes,
        ) -> std::convert::Infallible {
            if state.started.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                state.all_started.notify_one();
            }
            std::future::pending::<()>().await;
            unreachable!()
        }

        async fn spawn_peer(
            identity: Identity,
            state: RequestsInFlight,
        ) -> (
            tokio::sync::oneshot::Sender<()>,
            tokio::task::JoinHandle<Result<()>>,
            u16,
            u16,
        ) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let tcp_port = listener.local_addr().unwrap().port();
            let udp_probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let quic_port = udp_probe.local_addr().unwrap().port();
            drop(udp_probe);
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            let quic = quic_server_config(&identity).unwrap();
            let app = Router::new()
                .route("/hold", post(hold_request))
                .with_state(state);
            let server = tokio::spawn(async move {
                serve_with_quic(
                    listener,
                    app,
                    None,
                    Some(quic),
                    Some(quic_port),
                    async move {
                        let _ = stop_rx.await;
                    },
                )
                .await
            });
            (stop_tx, server, tcp_port, quic_port)
        }

        let barrier = RequestsInFlight {
            started: Arc::new(AtomicUsize::new(0)),
            all_started: Arc::new(Notify::new()),
        };
        let first_identity = Identity::generate();
        let second_identity = Identity::generate();
        let first_node_id = first_identity.node_id.0.clone();
        let second_node_id = second_identity.node_id.0.clone();
        let (first_stop, first_server, first_tcp, first_quic) =
            spawn_peer(first_identity, barrier.clone()).await;
        let (second_stop, second_server, second_tcp, second_quic) =
            spawn_peer(second_identity, barrier.clone()).await;

        let clients = crate::quic::ClientPool::new();
        let first_request = tokio::spawn({
            let clients = clients.clone();
            let node_id = first_node_id;
            async move {
                clients
                    .request(
                        &node_id,
                        &format!("http://127.0.0.1:{first_tcp}"),
                        &[format!("127.0.0.1:{first_quic}")],
                        "/hold",
                        "application/octet-stream",
                        None,
                        None,
                        Vec::new(),
                        Duration::from_secs(600),
                    )
                    .await
            }
        });
        let second_request = tokio::spawn({
            let clients = clients.clone();
            let node_id = second_node_id;
            async move {
                clients
                    .request(
                        &node_id,
                        &format!("http://127.0.0.1:{second_tcp}"),
                        &[format!("127.0.0.1:{second_quic}")],
                        "/hold",
                        "application/octet-stream",
                        None,
                        None,
                        Vec::new(),
                        Duration::from_secs(600),
                    )
                    .await
            }
        });

        tokio::time::timeout(Duration::from_secs(2), barrier.all_started.notified())
            .await
            .expect("both peers should accept the QUIC request");

        let shutdown = tokio::time::timeout(Duration::from_secs(5), async {
            first_stop.send(()).unwrap();
            second_stop.send(()).unwrap();
            first_server.await.unwrap().unwrap();
            second_server.await.unwrap().unwrap();
        })
        .await;

        first_request.abort();
        second_request.abort();
        assert!(
            shutdown.is_ok(),
            "idle peers cannot wait indefinitely for the QUIC shutdown"
        );
    }

    /// The reload may ask for the shutdown before the QUIC task has been
    /// scheduled for the first time. The signal must survive that race on
    /// each peer, or the supervisor waits forever.
    #[tokio::test(flavor = "current_thread")]
    async fn two_peers_do_not_lose_the_quic_shutdown_signal_before_the_first_poll() {
        async fn peer_parts(
            identity: Identity,
        ) -> (tokio::net::TcpListener, QuicServerConfig, u16) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let udp_probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let quic_port = udp_probe.local_addr().unwrap().port();
            drop(udp_probe);
            let quic = quic_server_config(&identity).unwrap();
            (listener, quic, quic_port)
        }

        // Prepare both sockets before spawning either server. This leaves the
        // already-ready shutdown futures queued together with the two peers,
        // making the lost `notify_waiters` wake-up deterministic.
        let (first_listener, first_quic, first_quic_port) = peer_parts(Identity::generate()).await;
        let (second_listener, second_quic, second_quic_port) =
            peer_parts(Identity::generate()).await;
        let mut first = tokio::spawn(serve_with_quic(
            first_listener,
            Router::new(),
            None,
            Some(first_quic),
            Some(first_quic_port),
            async {},
        ));
        let mut second = tokio::spawn(serve_with_quic(
            second_listener,
            Router::new(),
            None,
            Some(second_quic),
            Some(second_quic_port),
            async {},
        ));
        tokio::task::yield_now().await;
        let finished = tokio::time::timeout(Duration::from_secs(1), async {
            (&mut first).await.unwrap().unwrap();
            (&mut second).await.unwrap().unwrap();
        })
        .await;

        if finished.is_err() {
            first.abort();
            second.abort();
            let _ = first.await;
            let _ = second.await;
        }
        assert!(
            finished.is_ok(),
            "the simultaneous shutdown of both peers cannot lose the QUIC signal"
        );
    }

    /// And the attack pinning exists to prevent: whoever expects another
    /// node_id does not complete the handshake, even though the certificate is
    /// valid as a self-signed one.
    #[tokio::test]
    async fn a_client_expecting_another_node_is_refused() {
        let server = Identity::generate();
        let other = Identity::generate();
        let addr = start_server(&server, false).await;
        let http = clients().for_peer(&other.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await;
        assert!(r.is_err(), "the handshake had to fail, got {r:?}");
    }

    /// The switchover is not simultaneous: the same server serves plain text
    /// while peers have not yet learned of the new policy.
    #[tokio::test]
    async fn the_same_server_serves_plain_text() {
        let id = Identity::generate();
        let addr = start_server(&id, false).await;
        let r = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }

    /// In strict mode loopback still goes through in plain text - it is a
    /// requirement, not an oversight: `infer_tunnel::serve_loopback`, the
    /// `runtime` probe and the `testkit` hit 127.0.0.1 without a key pair.
    #[tokio::test]
    async fn in_strict_mode_loopback_still_speaks_plain() {
        let id = Identity::generate();
        let addr = start_server(&id, true).await;
        let r = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
    }

    /// The browser has to be able to open the dashboard with the cluster on TLS.
    ///
    /// This test exists because of a real failure: the pinning certificate is
    /// Ed25519, which browsers do not verify - measured with macOS's
    /// curl/LibreSSL, `sslv3 alert handshake failure`. Combined with strict
    /// mode, turning TLS on left the interface unreachable from any other
    /// machine. A client that does **not** offer the `pom/1` ALPN receives the
    /// ECDSA certificate, which it knows how to verify.
    #[tokio::test]
    async fn a_browser_can_open_the_interface_over_tls() {
        let id = Identity::generate();
        let addr = start_server(&id, true).await;

        // Like a browser: without `pom/1`, and trusting no CA at all (it is self
        // signed; the real browser shows the warning and the operator proceeds).
        let browser = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let r = browser
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("the browser had to complete the handshake");
        assert!(r.status().is_success());
    }

    /// And the peer still receives the pinning certificate on the same port.
    #[tokio::test]
    async fn on_the_same_port_the_peer_gets_the_pinning_certificate() {
        let id = Identity::generate();
        let addr = start_server(&id, true).await;
        let http = clients().for_peer(&id.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("the peer had to complete the handshake with the pinning cert");
        assert!(r.status().is_success());
    }

    /// And TLS still works in strict mode, which is the point of it.
    #[tokio::test]
    async fn in_strict_mode_tls_still_goes_through() {
        let id = Identity::generate();
        let addr = start_server(&id, true).await;
        let http = clients().for_peer(&id.node_id.0).unwrap();
        let r = http
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .expect("TLS must go through in strict mode");
        assert!(r.status().is_success());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_origin_header(value: &str) -> hyper::Request<()> {
        let mut req = hyper::Request::new(());
        req.headers_mut().insert(
            crate::infer_tunnel::TUNNEL_ORIGIN_HEADER,
            value.parse().unwrap(),
        );
        req
    }

    /// An https browser speaks HTTP/2: its host is only in the URI, and the
    /// checks that read `Host` must still see it.
    #[test]
    fn the_authority_fills_a_missing_host() {
        let mut req = hyper::Request::builder()
            .uri("https://ksia:8080/api/ui/node")
            .body(())
            .unwrap();
        host_from_authority(&mut req);
        assert_eq!(req.headers()[hyper::header::HOST], "ksia:8080");

        let mut req = hyper::Request::builder()
            .uri("https://ksia:8080/")
            .header(hyper::header::HOST, "salem:8080")
            .body(())
            .unwrap();
        host_from_authority(&mut req);
        assert_eq!(req.headers()[hyper::header::HOST], "salem:8080");

        let mut req = hyper::Request::new(());
        host_from_authority(&mut req);
        assert!(req.headers().get(hyper::header::HOST).is_none());
    }

    /// What the tunnel delivers over loopback, with this process's mark, is
    /// not local: that is what prevents a peer of the endpoint from becoming an
    /// administrator in localhost mode.
    #[test]
    fn a_tunneled_request_is_never_loopback() {
        let loopback: SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let mut req = with_origin_header(crate::infer_tunnel::tunnel_mark());
        let origin = request_origin(loopback, &mut req);
        assert!(!crate::auth::is_loopback_addr(origin));
        assert!(req
            .headers()
            .get(crate::infer_tunnel::TUNNEL_ORIGIN_HEADER)
            .is_none());
    }

    /// A wrong mark, or one coming from outside loopback, does not change the
    /// origin - and the header leaves the request either way.
    #[test]
    fn a_forged_tunnel_mark_changes_nothing() {
        let loopback: SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let mut req = with_origin_header("forjada");
        assert_eq!(request_origin(loopback, &mut req), loopback);
        assert!(req
            .headers()
            .get(crate::infer_tunnel::TUNNEL_ORIGIN_HEADER)
            .is_none());

        let remote: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        let mut req = with_origin_header(crate::infer_tunnel::tunnel_mark());
        assert_eq!(request_origin(remote, &mut req), remote);
    }
    use std::net::SocketAddr;

    /// `0x16` is TLS's `ContentType::Handshake`. Every HTTP method starts with
    /// an uppercase ASCII letter (`G`, `P`, `O`...), so a single byte tells the
    /// two protocols apart without ambiguity.
    #[test]
    fn the_first_byte_separates_tls_from_http() {
        assert!(wants_tls(0x16));
        assert!(!wants_tls(b'G')); // GET
        assert!(!wants_tls(b'P')); // POST
        assert!(!wants_tls(b'O')); // OPTIONS
    }

    /// Loopback always speaks plain text: `infer_tunnel::serve_loopback`, the
    /// `runtime.rs` probe and the `gateway::testkit` hit 127.0.0.1 and have no
    /// key pair to present. Traffic that does not leave the machine has
    /// nothing to protect.
    #[test]
    fn loopback_speaks_plain_even_in_strict_mode() {
        let local: SocketAddr = "127.0.0.1:5050".parse().unwrap();
        let local6: SocketAddr = "[::1]:5050".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:5050".parse().unwrap();
        assert!(plain_allowed(local, true));
        assert!(plain_allowed(local6, true));
        // The dual-stack socket delivers IPv4 as `::ffff:a.b.c.d`, so without
        // unmapping, local loopback would be refused by the node itself.
        assert!(plain_allowed(mapped, true));
    }

    /// The UI and the routes for people are redirected; the peer ones are not.
    #[test]
    fn only_a_peer_route_stops_being_redirected() {
        assert_eq!(plain_action("/"), PlainAction::Redirect);
        assert_eq!(plain_action("/assets/app.js"), PlainAction::Redirect);
        assert_eq!(plain_action("/api/ui/node"), PlainAction::Redirect);
        assert_eq!(plain_action("/admin/nodes"), PlainAction::Redirect);

        // A peer's `reqwest` would follow these silently, and the resulting
        // failure would not say why. The 403 does.
        assert_eq!(plain_action("/api/transfer/download"), PlainAction::Refuse);
        assert_eq!(plain_action("/v1/relay-result"), PlainAction::Refuse);
        assert_eq!(plain_action("/v1/prefill"), PlainAction::Refuse);
        assert_eq!(plain_action("/v1/step"), PlainAction::Refuse);
    }

    /// The dashboard is redirected preserving path and query - a redirect that
    /// drops the operator at the root loses the link they opened.
    #[test]
    fn the_redirect_preserves_the_path_and_the_query() {
        assert_eq!(
            plain_reply("ksia:8080", "/config/network?tab=tls"),
            (
                307,
                Some("https://ksia:8080/config/network?tab=tls".to_string())
            )
        );
        // The port goes along because it is the same: the server serves both protocols.
        assert_eq!(
            plain_reply("192.168.100.138:8080", "/"),
            (307, Some("https://192.168.100.138:8080/".to_string()))
        );
    }

    #[test]
    fn a_peer_route_gets_403_instead_of_a_redirect() {
        assert_eq!(
            plain_reply("ksia:8080", "/api/transfer/download?path=x"),
            (403, None)
        );
        assert_eq!(plain_reply("ksia:8080", "/v1/relay-result"), (403, None));
        assert_eq!(plain_reply("ksia:8080", "/api/transfer/offer"), (403, None));
        assert_eq!(plain_reply("ksia:8080", "/api/transfer/offers/answer"), (403, None));
    }

    /// The transfer routes the screen polls are the operator's, not peer-only.
    #[test]
    fn the_screen_transfer_routes_are_not_peer_only() {
        for path in ["/api/transfer/jobs", "/api/transfer/offers", "/api/transfer/pull"] {
            assert_eq!(plain_action(path), PlainAction::Redirect, "{path}");
        }
    }

    /// Without `Host` there is nowhere to send it, and a broken `Location`
    /// would make the client fail far from here, with an error that does not
    /// point back here.
    #[test]
    fn without_a_host_no_destination_is_invented() {
        assert_eq!(plain_reply("", "/"), (403, None));
    }

    /// From outside, strict mode refuses plain text - it is what gives the flag teeth.
    #[test]
    fn a_plain_remote_is_refused_in_strict_mode() {
        let remote: SocketAddr = "192.168.100.138:5050".parse().unwrap();
        assert!(!plain_allowed(remote, true));
        assert!(plain_allowed(remote, false));
    }

    /// The `undeploy` bug: a stalled keep-alive connection held the task that
    /// served it, the task held a copy of the `Router`, and the inference
    /// server's `Router` holds the model's weights - the VRAM never came back
    /// and the next model did not fit. `serve` has to shut down even with one
    /// of those connections open, and that is what this test pins down.
    #[tokio::test]
    async fn a_stalled_connection_does_not_hold_the_shutdown() {
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, shutdown) = tokio::sync::oneshot::channel::<()>();

        let app = Router::new().route("/health", axum::routing::get(|| async { "ok" }));
        let server = tokio::spawn(async move {
            serve_with_shutdown(listener, app, None, async {
                let _ = shutdown.await;
            })
            .await
        });

        // A real request, so the connection exists on the server side, and
        // then nothing: it is the stalled keep-alive of the gateway pool.
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = [0u8; 12];
        tokio::io::AsyncReadExt::read(&mut client, &mut response)
            .await
            .unwrap();

        let _ = stop.send(());
        // With slack over `DRAIN_DEADLINE`, and far below the "forever" that
        // was the old behavior.
        let output = tokio::time::timeout(DRAIN_DEADLINE * 3, server).await;
        assert!(
            output.is_ok(),
            "serve did not stop with a stalled connection"
        );
        assert!(output.unwrap().unwrap().is_ok());
    }
}
