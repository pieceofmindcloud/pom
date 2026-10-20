//! Request/response protocol over the iroh endpoint carrying inference HTTP
//! calls: the preferred inference transport for peers reached through it.
//! When the tunnel is unavailable or fails at the transport level, the caller uses
//! the pinned direct client as a fallback.
//!
//! Mirrors `bitswap.rs`'s shape ([4-byte BE length][JSON payload] framing
//! over an `AsyncRead`/`AsyncWrite` stream), but carries one generic
//! `{path, body}` pair instead of a fixed message enum — exactly mirroring
//! how `relay::send_frame` already takes `path: &str` generically across
//! `/v1/prefill`, `/v1/step`, `/v1/session/release`, `/v1/session/rollback`.
//! `body` is the same wire-encoded byte blob `send_frame` already builds;
//! this is purely an alternate transport for it, not a new format.

use crate::p2p::Codec;
use async_trait::async_trait;
use futures::prelude::*;
use crate::p2p::EndpointId;
use serde::{Deserialize, Serialize};
use std::io;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub const INFER_TUNNEL_PROTOCOL: &str = "/pom/infer-tunnel/1.0.0";

/// Which local server serves a call that arrived through the tunnel.
///
/// The destination is chosen by the sender but **resolved** by the receiver: it
/// maps the variant to one of the two ports it chose itself
/// (`inference_port`, `gateway_port`). A remote peer never names a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TunnelTarget {
    /// Embedded inference server (`/v1/prefill`, `/v1/step`, ...).
    #[default]
    Inference,
    /// Gateway: the control plane. See `gateway_path_is_allowed`.
    Gateway,
}

/// Gateway routes that the tunnel delivers. **Closed list, checked at the
/// receiver.**
///
/// Forwarding a tunnel call to the gateway makes it arrive there through
/// `127.0.0.1`, and the gateway uses the socket address (`serve::PeerAddr`) to
/// decide what is local: `install_trust_anchor` releases the network's administration
/// based **only** on that. A tunnel that accepted any path would give
/// any peer of the endpoint a way to become an administrator. That is why
/// there is no prefix or wildcard here: only the routes that admission and the
/// policy gates need, each one already authenticated on its own.
/// Header with which the tunnel forwarder marks the request it delivers to
/// this node's gateway. The gateway (`serve::handle_conn`) only accepts the mark coming from
/// loopback and carrying this process's secret (`tunnel_mark`), and then treats the
/// request as remote: everything that leaves the tunnel arrives through `127.0.0.1`, and without the
/// mark localhost mode would give the peer on the other side the node's administration.
pub(crate) const TUNNEL_ORIGIN_HEADER: &str = "x-pom-tunnel-origin";
/// The PeerId the swarm authenticated for a tunneled request, set by the
/// forwarder beside the mark and believed only with it
/// (`serve::request_origin`).
pub(crate) const TUNNEL_PEER_HEADER: &str = "x-pom-tunnel-peer";

/// Random secret of this process for `TUNNEL_ORIGIN_HEADER`. It never leaves the
/// process: a local client does not know it and neither does a remote peer, and the
/// forwarder overwrites any value the peer sends under that name.
pub(crate) fn tunnel_mark() -> &'static str {
    static MARK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MARK.get_or_init(|| hex::encode(rand::random::<[u8; 32]>()))
}

pub(crate) fn is_tunnel_mark(value: &[u8]) -> bool {
    value == tunnel_mark().as_bytes()
}

pub fn gateway_path_is_allowed(path: &str) -> bool {
    // The query string does not change the route, but it must not serve to escape the
    // exact comparison.
    let path = path.split(['?', '#']).next().unwrap_or(path);
    plugin_path_is_allowed(path)
        || matches!(
        path,
        "/handshake"
            | "/health"
            | "/api/cluster/quic"
            | "/api/cluster/logs"
            | "/api/cluster/network-chat"
            | "/api/cluster/network-settings"
            // A member telling the principal it is leaving; a member behind
            // NAT reaches the principal only this way.
            | "/api/cluster/member-left"
            // The return of the direct reply. The gateway sends its addresses in
            // `reply_to` and the tail delivers there; behind NAT none of them is
            // reachable from the other end, and without this path the frame
            // dies silently at the frame deadline. Both routes
            // are authenticated by the ticket, which is unique per frame: delivering
            // without a valid ticket returns 410 and reaches no session at all.
            | "/v1/relay-result"
            | "/v1/remote-stream-result"
            // The control plane for deployment and cluster reads, which
            // `GatewayState::control_get`/`control_post_json` send through the
            // tunnel when no address of the peer answers - the case of a
            // peer behind symmetric NAT. Safe only because the request arrives
            // marked (`TUNNEL_ORIGIN_HEADER`) and counts as remote: each route
            // authenticates the calling node by the control signature or by the
            // connection key, as it would in a direct call.
            | "/api/ui/node"
            | "/api/ui/models"
            | "/api/ui/model-metadata"
            | "/api/ui/model-identity"
            | "/api/ui/model-layer-options"
            | "/api/ui/slice-ranges"
            | "/api/ui/plan"
            | "/api/ui/apply"
            | "/api/ui/reconfigure"
            | "/api/ui/stop-inference"
            | "/api/ui/unload"
            | "/api/ui/leave"
            | "/api/ui/cache-types"
            | "/api/ui/deployments"
            | "/api/ui/deploy/capacity-preview"
            | "/api/ui/tp/check"
            | "/api/ui/network/rtt"
            | "/api/ui/network/chat"
            | "/api/internal/reload_model"
            // Administrative sync with the principal (`admin.rs`): trust
            // chain, succession and infer admission. A node that only
            // reaches the principal through the relay had no other path. All
            // of them require `InternalNodeAuth` (control signature or
            // connection key), never the socket's origin.
            | "/api/internal/admin-trust/reconcile"
            | "/api/internal/admin-trust/complete-transfer"
            | "/api/internal/admin-trust/chain"
            // Membership tokens, the admission list a secondary admin
            // reads from the principal and the revocation exchange
            // (`webui/membership.rs`).
            | "/api/internal/membership/token"
            | "/api/internal/membership/roster"
            | "/api/internal/membership/decide"
            | "/api/internal/membership/revocations"
            // A file offer: small and signed like the others.
            | "/api/transfer/offer"
            // A deployment asking the target to fetch its slice, and then
            // following the copy. Without these a target behind NAT could
            // receive a file but never be told to.
            | "/api/transfer/pull"
            | "/api/transfer/jobs"
            // The file itself, when no direct address answers. Carried only
            // over a connection that may carry it - see
            // `is_transfer_data_path`.
            | "/api/transfer/download"
            | "/api/transfer/manifest"
            | "/api/transfer/pom"
    )
}

/// The plugin calls between nodes that `control_get`/`control_post_json`
/// send through the tunnel when no address answers: the services listing,
/// a plugin's peer operation (`plugin_services::plugin_peer_call`) and a
/// generation on the machine serving a plugin engine's model. Each one
/// takes `InternalNodeAuth`. The engine's session files and stage links are
/// not here.
fn plugin_path_is_allowed(path: &str) -> bool {
    if path == "/api/internal/plugins/services" {
        return true;
    }
    let Some(rest) = path.strip_prefix("/api/internal/plugins/") else {
        return false;
    };
    let segment = |s: &str| {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    match rest.split('/').collect::<Vec<_>>().as_slice() {
        [code, "peer", op] => segment(code) && segment(op),
        [code, "engine", "generate"] => segment(code),
        _ => false,
    }
}

/// The route that carries the bytes of a file. The receiving node refuses it
/// on a connection that crosses a public relay
/// (`Network::connection_carries_transfers`), with
/// `TRANSFER_ROUTE_REFUSED_STATUS`.
pub fn is_transfer_data_path(path: &str) -> bool {
    path.split(['?', '#']).next().unwrap_or(path) == "/api/transfer/download"
}

/// How many bytes of a file one tunnel request asks for.
pub const TRANSFER_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
/// The most a tunnel request may ask for. The answer is held in memory on
/// both ends, so a request for a whole file is refused.
pub const MAX_TRANSFER_CHUNK_BYTES: u64 = 2 * TRANSFER_CHUNK_BYTES;

/// The closed byte range (`bytes=<first>-<last>`) a request asks for, as
/// `(first, length)`. `None` for an open range or no range at all.
pub fn closed_range(value: &str) -> Option<(u64, u64)> {
    let (first, last) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    (last >= first).then(|| (first, last - first + 1))
}

/// Whether a file chunk request asks for a bounded piece the tunnel can hold.
pub fn transfer_chunk_is_bounded(headers: &[(String, String)]) -> bool {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("range"))
        .and_then(|(_, value)| closed_range(value))
        .is_some_and(|(_, length)| length <= MAX_TRANSFER_CHUNK_BYTES)
}

/// "Misdirected request": this connection does not carry file transfers. The
/// sender may try again, because p2p spreads requests over every
/// connection it has with the peer.
pub const TRANSFER_ROUTE_REFUSED_STATUS: u16 = 421;

/// A single generic inference call, forwarded to the receiving peer's own
/// loopback server chosen by `target` (`http://127.0.0.1:<porta><path>`)
/// once it arrives — see `discovery.rs`'s handling of
/// `DiscBehaviourEvent::InferTunnel`.
#[derive(Debug, Clone)]
pub struct TunnelRequest {
    /// HTTP method used on the peer's loopback.
    pub method: String,
    /// Local server that answers.
    pub target: TunnelTarget,
    pub path: String,
    pub body: Vec<u8>,
    /// Headers needed by routes that do not carry their metadata in the body
    /// (for example, the TP reduce barrier).
    pub headers: Vec<(String, String)>,
}

impl TunnelRequest {
    pub fn post(path: impl Into<String>, body: Vec<u8>, headers: Vec<(String, String)>) -> Self {
        Self {
            method: "POST".to_string(),
            target: TunnelTarget::Inference,
            path: path.into(),
            body,
            headers,
        }
    }

    pub fn get(path: impl Into<String>) -> Self {
        Self {
            method: "GET".to_string(),
            target: TunnelTarget::Inference,
            path: path.into(),
            body: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Control-plane call, served by the destination's gateway.
    pub fn gateway(
        method: &str,
        path: impl Into<String>,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            method: method.to_string(),
            target: TunnelTarget::Gateway,
            path: path.into(),
            body,
            headers,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Request/response failure with a known delivery boundary. Dial errors
/// occur before the request is sent; a timeout or closed connection after
/// dispatch does not prove that the handler stopped executing the frame.
#[derive(Debug)]
pub struct TunnelDeliveryError {
    pub before_request: bool,
    message: String,
}

impl TunnelDeliveryError {
    pub fn before_request(message: impl Into<String>) -> Self {
        Self {
            before_request: true,
            message: message.into(),
        }
    }

    pub fn after_dispatch(message: impl Into<String>) -> Self {
        Self {
            before_request: false,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TunnelDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TunnelDeliveryError {}

impl From<crate::p2p::CallError> for TunnelDeliveryError {
    fn from(error: crate::p2p::CallError) -> Self {
        match error {
            crate::p2p::CallError::BeforeRequest(message) => Self::before_request(message),
            crate::p2p::CallError::AfterDispatch(message) => Self::after_dispatch(message),
        }
    }
}

/// One outbound call handed to the transport task (`Discovery::run`) from
/// outside it - the gateway in hub mode, or a relaying hop inside
/// `infer_server.rs` in ring mode. `Discovery::run` runs the call on its own
/// task and resolves `reply` with the answer or the failure - see
/// `relay::send_frame_via_tunnel`.
pub struct TunnelOutbound {
    pub peer_id: EndpointId,
    /// Endpoint addresses announced by the destination (`endpoint_addrs`),
    /// used as dial hints when the address book does not know it yet.
    pub addr_candidates: Vec<String>,
    pub request: TunnelRequest,
    pub reply: oneshot::Sender<Result<TunnelResponse, TunnelDeliveryError>>,
}

pub type TunnelOutboundSender = mpsc::Sender<TunnelOutbound>;

#[derive(Debug, Clone, Default)]
pub struct InferTunnelCodec;

/// A tunnel request/response carries exactly one wire-encoded frame
/// (`crate::wire`), so it must not be allowed to grow past what that frame
/// format itself already permits — same bound `send_frame` enforces.
const MAX_TUNNEL_PAYLOAD_BYTES: usize = crate::wire::MAX_FRAME_PAYLOAD_BYTES;
/// Bound of the JSON head of a request (method, target, path, headers).
const MAX_TUNNEL_HEAD_BYTES: usize = 64 * 1024;

/// What a request carries besides its body. The body travels raw after it:
/// as a JSON array of numbers it cost about 3.6 bytes on the wire per byte,
/// and a circuit through a relay paid that on every frame.
#[derive(Serialize, Deserialize)]
struct RequestHead {
    method: String,
    target: TunnelTarget,
    path: String,
    headers: Vec<(String, String)>,
}

fn too_large(what: &str, len: usize, bound: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("tunnel {what} of {len} bytes exceeds the {bound}-byte bound"),
    )
}

/// Reads `[u32 length][bytes]`, refusing a declared length past `bound`
/// before allocating for it.
async fn read_block<R>(io: &mut R, what: &str, bound: usize) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin + Send,
{
    use futures::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > bound {
        return Err(too_large(what, len, bound));
    }
    let mut data = vec![0u8; len];
    io.read_exact(&mut data).await?;
    Ok(data)
}

async fn write_block<W>(io: &mut W, what: &str, bound: usize, data: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    use futures::AsyncWriteExt;
    if data.len() > bound {
        return Err(too_large(what, data.len(), bound));
    }
    io.write_all(&(data.len() as u32).to_be_bytes()).await?;
    io.write_all(data).await
}

/// Binary framing. A request is `[u32][JSON head][u32][raw body]`; a response
/// is `[u16 status][u32][raw body]`. Lengths are big-endian.
#[async_trait]
impl Codec for InferTunnelCodec {
    const PROTOCOL: &'static str = INFER_TUNNEL_PROTOCOL;
    type Request = TunnelRequest;
    type Response = TunnelResponse;

    async fn read_request<T>(&mut self, io: &mut T) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let head = read_block(io, "head", MAX_TUNNEL_HEAD_BYTES).await?;
        let head: RequestHead = serde_json::from_slice(&head)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let body = read_block(io, "frame", MAX_TUNNEL_PAYLOAD_BYTES).await?;
        Ok(TunnelRequest {
            method: head.method,
            target: head.target,
            path: head.path,
            body,
            headers: head.headers,
        })
    }

    async fn read_response<T>(
        &mut self,
        io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        use futures::AsyncReadExt;
        let mut status = [0u8; 2];
        io.read_exact(&mut status).await?;
        let body = read_block(io, "frame", MAX_TUNNEL_PAYLOAD_BYTES).await?;
        Ok(TunnelResponse {
            status: u16::from_be_bytes(status),
            body,
        })
    }

    async fn write_request<T>(
        &mut self,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let head = serde_json::to_vec(&RequestHead {
            method: req.method,
            target: req.target,
            path: req.path,
            headers: req.headers,
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        write_block(io, "head", MAX_TUNNEL_HEAD_BYTES, &head).await?;
        write_block(io, "frame", MAX_TUNNEL_PAYLOAD_BYTES, &req.body).await
    }

    async fn write_response<T>(
        &mut self,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        use futures::AsyncWriteExt;
        io.write_all(&res.status.to_be_bytes()).await?;
        write_block(io, "frame", MAX_TUNNEL_PAYLOAD_BYTES, &res.body).await
    }
}


static TUNNEL_SENDER: std::sync::OnceLock<TunnelOutboundSender> = std::sync::OnceLock::new();
static LOCAL_NODE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Registers this process's discovery identity so that relay callers
/// do not try to open a tunnel to the node itself.
pub fn set_local_node_id(node_id: String) {
    let _ = LOCAL_NODE_ID.set(node_id);
}

/// This process's `node_id`, once already recorded.
pub fn local_node_id() -> Option<&'static str> {
    LOCAL_NODE_ID.get().map(String::as_str)
}

/// True when the destination is this process, not a remote peer.
pub fn is_local_node(node_id: &str) -> bool {
    LOCAL_NODE_ID.get().is_some_and(|local| local == node_id)
}

/// Set once, from `main.rs`, right after the channel is created (before
/// `Discovery::new` is even called — see that call site's comment). The only
/// way `infer_server.rs`'s ring-relay code, which holds no `GatewayState`
/// handle at all, can reach the tunnel — mirrors that module's existing
/// `http_client()` lazy-static for the exact same reason. A second call is a
/// bug (only one such channel is ever created per process) and is
/// intentionally dropped rather than panicking, matching this crate's
/// existing "config wins once, don't crash on a redundant set" style.
pub fn set_tunnel_sender(tx: TunnelOutboundSender) {
    let _ = TUNNEL_SENDER.set(tx);
}

/// `None` only if `set_tunnel_sender` was never called at all — in practice
/// that never happens on the path that constructs `infer_server.rs`'s state,
/// since `main.rs` sets it unconditionally before p2p discovery is even
/// attempted. A tunnel call made through this sender still degrades
/// correctly (fast, documented "channel closed" error) when p2p itself
/// never came up — see the doc comment on the channel's construction site.
pub fn tunnel_sender() -> Option<&'static TunnelOutboundSender> {
    TUNNEL_SENDER.get()
}

/// Measures the round-trip latency between this node and a peer over the same
/// endpoint path used to carry the inference frames.
///
/// The remote endpoint is `/health` on the peer's loopback. The measurement includes the endpoint
/// queue, any connection establishment, the transport, the local
/// forwarding and the processing of the `/health` handler, without carrying the
/// cost of prefill, decode or hidden-state transfer. The measurement is
/// directional: each process records only the time from its own origin.
pub async fn measure_rtt(
    sender: &TunnelOutboundSender,
    peer_id: EndpointId,
    timeout: Duration,
) -> Option<f64> {
    let (reply, response) = oneshot::channel();
    let started = std::time::Instant::now();
    sender
        .send(TunnelOutbound {
            peer_id,
            addr_candidates: Vec::new(),
            request: TunnelRequest::get("/health"),
            reply,
        })
        .await
        .ok()?;

    let Ok(Ok(Ok(response))) = tokio::time::timeout(timeout, response).await else {
        return None;
    };
    if !(200..300).contains(&response.status) {
        return None;
    }
    Some(started.elapsed().as_secs_f64() * 1000.0)
}

/// `GET /health` of a peer's inference server through the tunnel, and the body
/// when it answers 2xx. It is the plan-admission probe
/// (`gateway::ready_for_pipeline`) for a peer that no address reaches.
pub(crate) async fn health_via_tunnel(
    peer: &crate::peer_client::TunnelPeer,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let sender = tunnel_sender()?;
    let (reply, response) = oneshot::channel();
    sender
        .send(TunnelOutbound {
            peer_id: peer.peer_id,
            addr_candidates: peer.addrs.clone(),
            request: TunnelRequest::get("/health"),
            reply,
        })
        .await
        .ok()?;
    let Ok(Ok(Ok(response))) = tokio::time::timeout(timeout, response).await else {
        return None;
    };
    if !(200..300).contains(&response.status) {
        return None;
    }
    serde_json::from_slice(&response.body).ok()
}

/// A real endpoint speaking this protocol, wired to a real `TunnelOutboundSender`:
/// the same pairing `Discovery::run` provides in production, reduced to the one
/// protocol under test.
///
/// Exists so the live probes (`tunnel_reaches_a_live_peer` here, and
/// `relay::tests::the_tunnel_carries_a_real_frame_to_a_live_peer`) drive
/// production code against a machine that is actually on the network, instead
/// of a fake channel. Test-only: production always goes through `Discovery`,
/// which owns the full protocol set. It dials through n0's relays as well as
/// the given addresses, like a node does.
#[cfg(test)]
pub async fn live_probe_sender(peer: EndpointId, addrs: Vec<String>) -> TunnelOutboundSender {
    let book = iroh::address_lookup::MemoryLookup::new();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .address_lookup(book.clone())
        .bind()
        .await
        .expect("bind a probe endpoint");
    let p2p = crate::p2p::P2p::new(endpoint, book);
    p2p.learn(peer, &addrs);

    let (tx, mut rx) = mpsc::channel::<TunnelOutbound>(8);
    tokio::spawn(async move {
        while let Some(TunnelOutbound {
            peer_id,
            addr_candidates,
            request,
            reply,
        }) = rx.recv().await
        {
            eprintln!("[probe-endpoint] call -> {}", peer_id.fmt_short());
            let p2p = p2p.clone();
            tokio::spawn(async move {
                let result = p2p
                    .call(InferTunnelCodec, peer_id, &addr_candidates, request)
                    .await
                    .map_err(TunnelDeliveryError::from);
                let _ = reply.send(result);
            });
        }
    });
    tx
}

/// The entire inbound side of the NAT-tunnel: forward a request that arrived
/// over the endpoint to this node's own inference server on loopback, and carry
/// back whatever it answers.
///
/// Deliberately not a live `SharedState` handle — `InferState` has no stable
/// external handle to hold (it's destroyed and recreated on every model
/// reconfiguration by `NodeRuntime`'s supervisor), so keeping this task in
/// sync with that would be its own ongoing-maintenance burden. The always-
/// current, always-correct entry point stays the real Axum server; this is
/// just a same-machine hop in front of it, at a sub-millisecond cost.
///
/// Inference frames remain POSTs. The only exception is the short GET
/// of `/health` used by the directional RTT measurement.
/// The status `serve_loopback` answers when the peer's own server is not
/// listening: the tunnel reached the peer, the destination did not exist.
pub(crate) const NOTHING_LISTENING_STATUS: u16 = 502;

pub async fn serve_loopback(
    http: &reqwest::Client,
    port: u16,
    request: TunnelRequest,
) -> TunnelResponse {
    // Always cleartext on loopback: `serve::plain_allowed` allows loopback
    // even with the cluster on strict TLS, and the node's certificate does not cover
    // `127.0.0.1`.
    let url = format!("http://127.0.0.1:{port}{}", request.path);
    let has_content_type = request
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(reqwest::header::CONTENT_TYPE.as_str()));
    // An unknown method falls back to POST, the old behavior for anything
    // that was not GET.
    let method = reqwest::Method::from_bytes(request.method.to_ascii_uppercase().as_bytes())
        .unwrap_or(reqwest::Method::POST);
    let mut request_builder = if method == reqwest::Method::GET {
        http.get(&url)
    } else {
        let builder = http.request(method, &url).body(request.body);
        if has_content_type {
            builder
        } else {
            builder.header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        }
    };
    for (name, value) in request.headers {
        if name.eq_ignore_ascii_case(TUNNEL_ORIGIN_HEADER) {
            continue;
        }
        request_builder = request_builder.header(name, value);
    }
    let sent = request_builder
        .header(TUNNEL_ORIGIN_HEADER, tunnel_mark())
        .send()
        .await;

    match sent {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
            TunnelResponse { status, body }
        }
        // The peer's own inference server isn't reachable even on its own
        // loopback (not yet started, mid-reconfiguration, crashed) — a
        // synthetic 502 lets the caller's fallback logic tell this apart
        // from a successful-but-erroring response, without inventing a new
        // status code space just for the tunnel.
        Err(e) => TunnelResponse {
            status: NOTHING_LISTENING_STATUS,
            body: e.to_string().into_bytes(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The escalation the closed list exists to prevent: `install_trust_anchor`
    /// releases the network's administration looking only at whether the socket is loopback, and
    /// everything that leaves the tunnel reaches the gateway through `127.0.0.1`.
    #[test]
    fn the_tunnel_does_not_carry_an_administrative_route() {
        assert!(!gateway_path_is_allowed("/api/ui/trust/anchor"));
        assert!(!gateway_path_is_allowed("/api/ui/network/trust-anchor"));
        assert!(!gateway_path_is_allowed("/admin/config"));
        assert!(!gateway_path_is_allowed("/admin/nodes"));
        assert!(!gateway_path_is_allowed("/api/generate"));
        assert!(!gateway_path_is_allowed("/"));
    }

    #[test]
    fn a_file_chunk_over_the_tunnel_is_a_bounded_range() {
        let range = |value: &str| vec![("Range".to_string(), value.to_string())];
        assert_eq!(closed_range("bytes=10-19"), Some((10, 10)));
        assert_eq!(closed_range("bytes=10-"), None);
        assert!(transfer_chunk_is_bounded(&range("bytes=0-4194303")));
        assert!(!transfer_chunk_is_bounded(&range("bytes=0-")));
        assert!(!transfer_chunk_is_bounded(&range("bytes=0-99999999999")));
        assert!(!transfer_chunk_is_bounded(&[]));
    }

    #[test]
    fn only_the_download_counts_as_file_bytes() {
        assert!(is_transfer_data_path("/api/transfer/download?grant=abc"));
        assert!(gateway_path_is_allowed("/api/transfer/download?grant=abc"));
        assert!(!is_transfer_data_path("/api/transfer/offer"));
        assert!(!is_transfer_data_path("/api/transfer/downloadx"));
    }

    /// A deployment tells the target to fetch its slice and then follows the
    /// copy. Behind NAT the target has no address that answers, so both calls
    /// must be able to cross the tunnel - and neither is file bytes.
    #[test]
    fn a_deployment_can_ask_for_a_pull_and_follow_it_through_the_tunnel() {
        for path in ["/api/transfer/pull", "/api/transfer/jobs"] {
            assert!(gateway_path_is_allowed(path), "{path}");
            assert!(!is_transfer_data_path(path), "{path}");
        }
        // Cancelling and the offers inbox stay off the tunnel.
        assert!(!gateway_path_is_allowed("/api/transfer/jobs/abc/cancel"));
        assert!(!gateway_path_is_allowed("/api/transfer/offers"));
    }

    /// The deployment control plane passes, and only because the request arrives
    /// marked as remote (see `serve::request_origin`).
    #[test]
    fn the_tunnel_carries_the_deployment_control_plane() {
        assert!(gateway_path_is_allowed("/api/ui/node"));
        assert!(gateway_path_is_allowed("/api/ui/apply"));
        assert!(gateway_path_is_allowed("/api/ui/deployments?local=true"));
    }

    /// Administrative sync with the principal passes: a node that only reaches
    /// the principal through the relay must follow the trust chain. The other
    /// internal routes stay out, API keys among them: they never leave a node.
    #[test]
    fn the_tunnel_carries_the_sync_with_the_principal() {
        for path in [
            "/api/internal/admin-trust/reconcile",
            "/api/internal/admin-trust/complete-transfer",
            "/api/internal/admin-trust/chain",
        ] {
            assert!(gateway_path_is_allowed(path), "{path}");
        }
        assert!(!gateway_path_is_allowed(
            "/api/internal/enterprise/fingerprint"
        ));
        assert!(!gateway_path_is_allowed("/api/internal/api-keys/upsert"));
        assert!(!gateway_path_is_allowed("/api/internal/api-keys/validate"));
    }

    /// A plugin's calls between nodes pass, and only in their exact shape.
    #[test]
    fn the_tunnel_carries_the_plugin_calls() {
        assert!(gateway_path_is_allowed("/api/internal/plugins/services"));
        assert!(gateway_path_is_allowed("/api/internal/plugins/rag/peer/search"));
        assert!(gateway_path_is_allowed("/api/internal/plugins/moe_x/engine/generate"));
        assert!(!gateway_path_is_allowed("/api/internal/plugins/rag/peer/search/x"));
        assert!(!gateway_path_is_allowed("/api/internal/plugins/rag/peer/"));
        assert!(!gateway_path_is_allowed("/api/internal/plugins/../peer/x"));
        assert!(!gateway_path_is_allowed("/api/internal/plugins/rag/engine/session/file"));
        assert!(!gateway_path_is_allowed("/api/internal/plugins/rag/engine/link/d"));
    }

    /// The mark is a process secret: constant while it lives and never
    /// a value that a peer on the other side would guess.
    #[test]
    fn the_tunnel_mark_is_a_stable_secret() {
        assert_eq!(tunnel_mark(), tunnel_mark());
        assert_eq!(tunnel_mark().len(), 64);
        assert!(is_tunnel_mark(tunnel_mark().as_bytes()));
        assert!(!is_tunnel_mark(b""));
        assert!(!is_tunnel_mark(b"1"));
    }

    /// The routes that admission and the policy gates depend on. Without
    /// `/handshake` here, a peer behind NAT never enters the `peers` table.
    #[test]
    fn the_tunnel_carries_the_control_plane() {
        for path in [
            "/handshake",
            "/health",
            "/api/cluster/quic",
            "/api/cluster/logs",
            "/api/cluster/network-chat",
            "/api/cluster/network-settings",
            "/v1/relay-result",
            "/v1/remote-stream-result",
        ] {
            assert!(gateway_path_is_allowed(path), "{path} should pass");
        }
    }

    /// Exact comparison, not prefix: a suffix cannot hang another route
    /// on an allowed one, and a query string cannot serve as an escape.
    #[test]
    fn the_list_comparison_is_exact() {
        assert!(!gateway_path_is_allowed(
            "/handshake/../api/ui/trust/anchor"
        ));
        assert!(!gateway_path_is_allowed("/health/../admin"));
        assert!(!gateway_path_is_allowed("/api/cluster/quicx"));
        assert!(gateway_path_is_allowed("/api/cluster/quic?verbose=1"));
    }

    use futures::io::Cursor;

    /// A request/response has to survive the exact framing a real
    /// `AsyncRead`/`AsyncWrite` stream would carry it over — this drives the
    /// codec through an in-memory buffer instead of a real p2p stream,
    /// the same spirit as `wire.rs`'s own round-trip tests.
    #[tokio::test]
    async fn a_tunnel_request_round_trips_through_the_codec() {
        let mut codec = InferTunnelCodec;
        let req = TunnelRequest::post("/v1/prefill", vec![1, 2, 3, 4, 5], Vec::new());

        let mut buf = Vec::new();
        codec
            .write_request(&mut buf, req.clone())
            .await
            .expect("write_request");

        let mut cursor = Cursor::new(buf);
        let decoded = codec
            .read_request(&mut cursor)
            .await
            .expect("read_request");

        assert_eq!(decoded.path, req.path);
        assert_eq!(decoded.body, req.body);
    }

    /// The body travels raw: the frame is the body plus a small head, not
    /// several bytes of JSON per byte of payload.
    #[tokio::test]
    async fn the_body_is_not_inflated_on_the_wire() {
        let mut codec = InferTunnelCodec;
        let body = vec![0xffu8; 100_000];
        let req = TunnelRequest::gateway(
            "GET",
            "/api/transfer/download",
            body.clone(),
            vec![("range".into(), "bytes=0-99".into())],
        );

        let mut buf = Vec::new();
        codec.write_request(&mut buf, req).await.unwrap();
        assert!(buf.len() < body.len() + 512, "frame of {} bytes", buf.len());

        let decoded = codec
            .read_request(&mut Cursor::new(buf))
            .await
            .unwrap();
        assert_eq!(decoded.target, TunnelTarget::Gateway);
        assert_eq!(
            decoded.headers,
            vec![("range".to_string(), "bytes=0-99".to_string())]
        );
        assert_eq!(decoded.body, body);
    }

    #[tokio::test]
    async fn a_tunnel_response_round_trips_through_the_codec() {
        let mut codec = InferTunnelCodec;
        let res = TunnelResponse {
            status: 200,
            body: b"hello from the other side".to_vec(),
        };

        let mut buf = Vec::new();
        codec
            .write_response(&mut buf, res.clone())
            .await
            .expect("write_response");

        let mut cursor = Cursor::new(buf);
        let decoded = codec
            .read_response(&mut cursor)
            .await
            .expect("read_response");

        assert_eq!(decoded.status, res.status);
        assert_eq!(decoded.body, res.body);
    }

    /// An empty body (a `GET` with nothing to send) is a real case — `/health`
    /// checks look exactly like this — and must not be confused with "no
    /// frame at all".
    #[tokio::test]
    async fn an_empty_body_round_trips_cleanly() {
        let mut codec = InferTunnelCodec;
        let req = TunnelRequest::get("/health");

        let mut buf = Vec::new();
        codec
            .write_request(&mut buf, req.clone())
            .await
            .expect("write_request");

        let mut cursor = Cursor::new(buf);
        let decoded = codec
            .read_request(&mut cursor)
            .await
            .expect("read_request");

        assert_eq!(decoded.path, "/health");
        assert_eq!(decoded.method, "GET");
        assert!(decoded.body.is_empty());
    }

    /// A declared length past the wire's own frame bound must be refused
    /// before the codec allocates a buffer for it — the same defence
    /// `wire.rs::read_frame` has for a hostile or corrupted declared size.
    #[tokio::test]
    async fn a_frame_declaring_more_than_the_bound_is_refused() {
        let mut codec = InferTunnelCodec;

        let head = br#"{"method":"POST","target":"inference","path":"/v1/step","headers":[]}"#;
        let mut buf = (head.len() as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(head);
        buf.extend_from_slice(&((MAX_TUNNEL_PAYLOAD_BYTES as u32) + 1).to_be_bytes());
        buf.extend_from_slice(b"doesn't matter, never read");

        let mut cursor = Cursor::new(buf);
        let result = codec.read_request(&mut cursor).await;

        assert!(result.is_err(), "oversized frame must be refused");
    }

    /// `serve_loopback` against a real (in-process, real TCP on loopback)
    /// fake inference server — a genuine HTTP round trip, not a mock, since
    /// that's exactly the "new glue code" (URL construction, status/body
    /// carry-through) a bug here would hide in.
    async fn spawn_fake_inference_server(
        handler: impl Fn(Vec<u8>) -> (u16, Vec<u8>) + Clone + Send + Sync + 'static,
    ) -> u16 {
        use axum::{body::Bytes, routing::post, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route(
            "/v1/step",
            post(move |body: Bytes| {
                let handler = handler.clone();
                async move {
                    let (status, body) = handler(body.to_vec());
                    (axum::http::StatusCode::from_u16(status).unwrap(), body)
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        port
    }

    async fn spawn_fake_health_server() -> u16 {
        use axum::{routing::get, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route("/health", get(|| async { "ok" }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        port
    }

    #[tokio::test]
    async fn serve_loopback_uses_get_for_the_rtt_probe() {
        let port = spawn_fake_health_server().await;
        let response =
            serve_loopback(&reqwest::Client::new(), port, TunnelRequest::get("/health")).await;

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
    }

    #[tokio::test]
    async fn serve_loopback_forwards_the_body_and_carries_back_the_response() {
        let port = spawn_fake_inference_server(|body| {
            assert_eq!(body, b"the request body");
            (200, b"the response body".to_vec())
        })
        .await;

        let http = reqwest::Client::new();
        let response = serve_loopback(
            &http,
            port,
            TunnelRequest::post("/v1/step", b"the request body".to_vec(), Vec::new()),
        )
        .await;

        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"the response body");
    }

    #[tokio::test]
    async fn serve_loopback_preserves_json_content_type_for_lifecycle_calls() {
        use axum::{
            body::Bytes,
            http::{HeaderMap, StatusCode},
            routing::post,
            Router,
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = Router::new().route(
            "/v1/session/release",
            post(|headers: HeaderMap, body: Bytes| async move {
                let content_type = headers
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok());
                if content_type == Some("application/json")
                    && body.as_ref() == br#"{"session_id":"s"}"#
                {
                    StatusCode::OK
                } else {
                    StatusCode::UNSUPPORTED_MEDIA_TYPE
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let response = serve_loopback(
            &reqwest::Client::new(),
            port,
            TunnelRequest::post(
                "/v1/session/release",
                br#"{"session_id":"s"}"#.to_vec(),
                vec![(
                    reqwest::header::CONTENT_TYPE.as_str().to_string(),
                    "application/json".to_string(),
                )],
            ),
        )
        .await;

        assert_eq!(response.status, 200);
    }

    #[tokio::test]
    async fn serve_loopback_carries_back_a_non_success_status_unchanged() {
        let port = spawn_fake_inference_server(|_body| (503, b"kv cache busy".to_vec())).await;

        let http = reqwest::Client::new();
        let response = serve_loopback(
            &http,
            port,
            TunnelRequest::post("/v1/step", vec![], Vec::new()),
        )
        .await;

        assert_eq!(response.status, 503);
        assert_eq!(response.body, b"kv cache busy");
    }

    /// No inference server listening at all on that port — the loopback hop
    /// itself failed, not the model. The caller needs a way to tell this
    /// apart from a real (successfully reached) error response.
    #[tokio::test]
    async fn serve_loopback_reports_a_synthetic_502_when_nothing_is_listening() {
        let http = reqwest::Client::new();
        // Port 0 alone never accepts a connection; using the OS-assigned
        // port from a listener we immediately drop guarantees nothing is
        // bound there without racing a real port number.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let response = serve_loopback(
            &http,
            port,
            TunnelRequest::post("/v1/step", vec![], Vec::new()),
        )
        .await;

        assert_eq!(response.status, 502);
    }

    /// Speaks the tunnel protocol against a **real** peer, over the internet.
    ///
    /// The commit that created the tunnel recorded as a known gap that it had only
    /// been exercised on loopback and with fake endpoints. This is what
    /// closes that gap, and it cannot become a CI test: it depends on a
    /// remote machine being up. It stays `#[ignore]` and is driven by an environment
    /// variable, as a reproducible procedure.
    /// ```sh
    /// POM_TUNNEL_PROBE=<endpoint-id>@203.0.113.7:52777,https://relay.example.org \
    ///   cargo test -p node --bin pom tunnel_reaches_a_live_peer -- --ignored --nocapture
    /// ```
    ///
    /// The endpoint id is the `endpoint_id` the peer announces (its public key);
    /// after the `@`, optional addresses from its `endpoint_addrs`. Any answer
    /// already proves the path: the synthetic 502 of `serve_loopback` is
    /// generated **on the other machine**, so even it proves the frame got
    /// across.
    #[tokio::test]
    #[ignore = "needs a live peer; see the test doc"]
    async fn tunnel_reaches_a_live_peer() {
        let Ok(target) = std::env::var("POM_TUNNEL_PROBE") else {
            panic!("set POM_TUNNEL_PROBE to <endpoint-id>[@addr,addr]");
        };
        let (id, addrs) = target.split_once('@').unwrap_or((target.as_str(), ""));
        let peer_id = crate::p2p::parse_endpoint_id(id).expect("invalid endpoint id");
        let addrs: Vec<String> = addrs
            .split(',')
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect();
        let path =
            std::env::var("POM_TUNNEL_PROBE_PATH").unwrap_or_else(|_| "/v1/step".to_string());

        let sender = live_probe_sender(peer_id, addrs.clone()).await;
        let (reply, response) = oneshot::channel();
        sender
            .send(TunnelOutbound {
                peer_id,
                addr_candidates: addrs,
                request: TunnelRequest::get(path),
                reply,
            })
            .await
            .expect("probe queue");
        let response = tokio::time::timeout(Duration::from_secs(45), response)
            .await
            .expect("the peer did not answer within the deadline")
            .expect("the probe dropped the call")
            .expect("the tunnel failed");
        println!(
            "tunnel answered: status={} body={:?}",
            response.status,
            String::from_utf8_lossy(&response.body[..response.body.len().min(200)])
        );
    }
}
