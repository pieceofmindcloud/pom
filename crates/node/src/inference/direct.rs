//! Direct response: the tail delivers the result to whoever asked.
//!
//! The ring mode (`relay/v1-chain`) chains nested HTTP calls - each hop
//! holds its own response open while it calls the next hop, and the
//! result comes back retracing the path of the outbound trip. That is `2N` network traversals per
//! token for a chain of `N` hops, and the count gets worse precisely when the
//! head is not on the machine that received the request: then the outbound and the return
//! cross the link twice each. Measured on 2026-08-14 between the captain's two
//! sites (RTT 30 ms): 25 tok/s with the head local against 14 tok/s
//! with it remote, exactly one RTT more per token.
//!
//! Here the chain stops coming back. The gateway sends along with the frame a
//! [`DirectReply::reply_to`] - the addresses through which it can itself be
//! reached, plus a random *ticket* - and each hop relays that field
//! onward without touching it. Whoever finishes (the tail, or the hop that failed to
//! relay) makes **one** POST to `/v1/relay-result` straight to the gateway. That is
//! `N + 1` traversals, and the cost stops depending on where the head is.
//!
//! What each hop returns to its caller becomes an immediate *ack*: the hop has already
//! accepted responsibility for the frame, and nothing else travels that way. The ack
//! leaves the critical path - whoever waits is the gateway, on the ticket.
//!
//! ## Failure, cancellation and deadline
//!
//! * **A middle hop dies before accepting** - whoever called it receives a transport
//!   error and delivers a [`crate::relay::RelayFailure`] straight to the gateway,
//!   naming the node that really died.
//! * **The tail dies** - likewise, reported by the previous hop.
//! * **A hop dies after accepting** (process crashed, machine vanished) - there is
//!   nobody to report it; what closes the request is the frame deadline (`relay::frame_deadline`).
//! * **Direct response lost** (gateway unreachable from the tail) -
//!   same deadline. The tail tries each `reply_to` URL before giving up.
//! * **A response that arrives after the giving up** - the [`Ticket`] has already left the
//!   inbox (it is removed on `Drop`, on any exit path), and the late result
//!   gets `410 Gone`. Since the ticket is unique per frame, it can never
//!   be confused with the one of the next token.
//!
//! The capability is negotiated like all the others: it only enters when **all** the
//! steps announce `relay/v2-direct` (see `PipelinePlan::supports_direct_reply`)
//! and the gateway knows at least one address of itself. A cluster in
//! rolling update falls back to the usual ring, with nothing to configure.

use anyhow::{anyhow, Result};
use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// Route on which the gateway receives the result the tail delivers.
pub const RELAY_RESULT_PATH: &str = "/v1/relay-result";
/// Callback used by the remote decode transport for each opportunistic batch.
pub const REMOTE_STREAM_RESULT_PATH: &str = "/v1/remote-stream-result";

// How long the gateway waits for the direct result is the frame deadline
// (`relay::frame_deadline`), proportional to the tokens and hops of the chain; the
// caller passes it to `await_result_with_ack`.

/// Cap of one delivery attempt when **there is still another address** to
/// try. Short on purpose: an address that accepts the SYN and never answers
/// must not cost the whole deadline before the next one is paid.
#[cfg(not(test))]
const DELIVERY_TIMEOUT: Duration = Duration::from_millis(500);

/// On the bench the cap needs to stay above a legitimate traversal of the
/// laboratory (`direct_lab::LINK_MS`, 50 ms), otherwise the test itself declares
/// dead a delivery that is only slow.
#[cfg(test)]
const DELIVERY_TIMEOUT: Duration = Duration::from_millis(150);

/// Cap of the **last** attempt. After it there is no third way: the gateway
/// waits the whole frame deadline (`relay::frame_deadline`) and the session dies silently.
///
/// Giving up at 500 ms on a merely slow delivery - busy portal, cold TLS,
/// large batch, WAN - trades tens of seconds of waiting for ten minutes of
/// silence. The last address gets the cap the per-request path always
/// used; the speed of switching candidates holds while there is a candidate.
#[cfg(not(test))]
const LAST_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
const LAST_DELIVERY_TIMEOUT: Duration = Duration::from_millis(600);

/// The response may already have been released when the transport receipt arrives.
/// We keep only the small snapshot for this period, without holding up generation.
const TELEMETRY_RETENTION: Duration = Duration::from_secs(30);

// ── Inbox ─────────────────────────────────────────────────────────────────────

/// The frames this node dispatched and is still waiting a result for.
///
/// A ticket -> channel map. It lives in the gateway (`GatewayState`) because it is the
/// gateway that opens the request; the `/v1/relay-result` handler only looks up the
/// ticket here.
#[derive(Clone, Default)]
pub struct ReplyInbox(Arc<Mutex<InboxState>>);

impl std::fmt::Debug for ReplyInbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ReplyInbox").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct InboxState {
    replies: HashMap<String, ReplyState>,
    streams: HashMap<String, StreamState>,
}

struct ReplyState {
    sender: Option<oneshot::Sender<Value>>,
    result: Option<Value>,
    delivered: bool,
    expected_transport: usize,
    transport: Vec<Option<Value>>,
    late_transport: u64,
    expires_at: Option<Instant>,
}

/// Bounded on purpose: a slow gateway must apply backpressure to the remote
/// emitter and, through it, to the inference producer.
// A bounded queue decouples the remote producer from callback jitter. 64
// entries cover several transient 67-114 ms network spikes without allowing
// an unbounded request-sized allocation; backpressure still closes the loop
// when the client is genuinely slower than generation.
const REMOTE_STREAM_QUEUE_CAPACITY: usize = 64;
const REMOTE_STREAM_DEDUPE_WINDOW: usize = 1_024;

struct StreamState {
    sender: mpsc::Sender<Value>,
    seen_frames: HashSet<String>,
}

impl ReplyInbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a ticket. The returned guard removes it from the inbox when it drops, on
    /// any exit path - it is what keeps an orphan ticket from holding
    /// memory after an error or an expired deadline.
    pub fn register(&self) -> Ticket {
        self.register_with_expected_transport(0)
    }

    /// Opens a ticket that also waits for evidence per traversal.
    ///
    /// The direct response arrives through a POST separate from the acks. The result is
    /// published as soon as it arrives. Receipts already received are attached; those that
    /// arrive later leave the sample explicitly incomplete, without putting
    /// telemetry on the critical path of generation.
    pub fn register_with_expected_transport(&self, expected_transport: usize) -> Ticket {
        self.cleanup_expired();
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.0.lock().unwrap().replies.insert(
            id.clone(),
            ReplyState {
                sender: Some(tx),
                result: None,
                delivered: false,
                expected_transport,
                transport: vec![None; expected_transport],
                late_transport: 0,
                expires_at: None,
            },
        );
        Ticket {
            id,
            inbox: self.clone(),
            rx,
        }
    }

    /// Opens a ticket that can receive several remote decode batches.
    pub fn register_stream(&self) -> StreamTicket {
        self.cleanup_expired();
        let id = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = mpsc::channel(REMOTE_STREAM_QUEUE_CAPACITY);
        self.0.lock().unwrap().streams.insert(
            id.clone(),
            StreamState {
                sender,
                seen_frames: HashSet::new(),
            },
        );
        StreamTicket {
            id,
            inbox: self.clone(),
            receiver,
        }
    }

    /// Delivers a result. `false` when the ticket no longer exists - the
    /// gateway has already given up, and whoever delivered needs to know it arrived late.
    pub fn deliver(&self, ticket: &str, meta: Value) -> bool {
        self.cleanup_expired();
        let mut state = self.0.lock().unwrap();
        let Some(entry) = state.replies.get_mut(ticket) else {
            return false;
        };
        if entry.delivered {
            return false;
        }
        let meta = complete_transport_trace(&entry, meta);
        entry.result = Some(meta.clone());
        entry.delivered = true;
        entry.expires_at = Some(Instant::now() + TELEMETRY_RETENTION);
        let sender = entry.sender.take();
        if entry.expected_transport == 0 {
            state.replies.remove(ticket);
        }
        sender.is_some_and(|sender| meta_sender_send(meta, sender))
    }

    /// Records the response of a traversal without turning it into a result.
    /// Returns `false` when the ticket has already expired or was removed.
    pub fn record_transport(&self, ticket: &str, hop_index: usize, meta: Value) -> bool {
        self.cleanup_expired();
        let mut state = self.0.lock().unwrap();
        let Some(entry) = state.replies.get_mut(ticket) else {
            return false;
        };
        if hop_index >= entry.expected_transport {
            return false;
        }
        if entry.transport[hop_index].is_some() {
            return false;
        }
        if entry.delivered {
            entry.late_transport = entry.late_transport.saturating_add(1);
        }
        entry.transport[hop_index] = Some(meta);
        if entry.delivered {
            let Some(previous) = entry.result.take() else {
                return true;
            };
            let mut updated = complete_transport_trace(entry, previous);
            updated["telemetry_late_spans"] = json!(entry.late_transport);
            entry.result = Some(updated);
        }
        true
    }

    /// Snapshots the latest version of the telemetry without blocking the response.
    pub fn trace_snapshot(&self, ticket: &str) -> Option<Value> {
        self.cleanup_expired();
        self.0
            .lock()
            .unwrap()
            .replies
            .get(ticket)
            .and_then(|entry| entry.result.clone())
    }

    fn cleanup_expired(&self) {
        let now = Instant::now();
        let mut state = self.0.lock().unwrap();
        state
            .replies
            .retain(|_, entry| entry.expires_at.is_none_or(|expires_at| expires_at > now));
    }

    fn cancel(&self, ticket: &str) {
        let mut state = self.0.lock().unwrap();
        if state
            .replies
            .get(ticket)
            .is_some_and(|entry| !entry.delivered)
        {
            state.replies.remove(ticket);
        }
        state.streams.remove(ticket);
    }

    /// How many frames are in flight. Only for tests and diagnostics.
    pub fn outstanding(&self) -> usize {
        self.cleanup_expired();
        self.0
            .lock()
            .unwrap()
            .replies
            .values()
            .filter(|entry| !entry.delivered)
            .count()
    }

    #[cfg(test)]
    fn stream_outstanding(&self) -> usize {
        self.0.lock().unwrap().streams.len()
    }

    /// Delivers a batch without holding the inbox lock across the await. A
    /// terminal batch removes the stream after its value has entered the
    /// consumer queue.
    pub async fn deliver_stream(&self, ticket: &str, meta: Value) -> bool {
        self.cleanup_expired();
        let terminal = meta.get("done").and_then(Value::as_bool).unwrap_or(false)
            || meta.get("error").and_then(Value::as_str).is_some();
        let sender = {
            let mut state = self.0.lock().unwrap();
            let Some(stream) = state.streams.get_mut(ticket) else {
                return false;
            };
            let frame_id = meta
                .get("stream_id")
                .and_then(Value::as_str)
                .zip(
                    meta.get("remote_stream_frame_index")
                        .and_then(Value::as_u64),
                )
                .map(|(stream_id, index)| format!("{stream_id}:{index}"));
            if let Some(frame_id) = frame_id {
                if stream.seen_frames.contains(&frame_id) {
                    return true;
                }
                if stream.seen_frames.len() >= REMOTE_STREAM_DEDUPE_WINDOW {
                    stream.seen_frames.clear();
                }
                stream.seen_frames.insert(frame_id);
            }
            stream.sender.clone()
        };
        if sender.send(meta).await.is_err() {
            self.0.lock().unwrap().streams.remove(ticket);
            return false;
        }
        if terminal {
            self.0.lock().unwrap().streams.remove(ticket);
        }
        true
    }
}

fn complete_transport_trace(entry: &ReplyState, mut meta: Value) -> Value {
    if entry.expected_transport == 0 {
        return meta;
    }
    let traversals: Vec<Value> = entry.transport.iter().flatten().cloned().collect();
    meta["transport_traversals"] = json!(traversals);
    meta["transport_trace_complete"] = json!(
        entry.expected_transport == meta["transport_traversals"].as_array().map_or(0, Vec::len)
    );
    meta
}

fn meta_sender_send(meta: Value, sender: oneshot::Sender<Value>) -> bool {
    sender.send(meta).is_ok()
}

/// A frame in flight. Removed from the inbox on `Drop`.
pub struct Ticket {
    id: String,
    inbox: ReplyInbox,
    rx: oneshot::Receiver<Value>,
}

impl Ticket {
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.inbox.cancel(&self.id);
    }
}

/// Consumer side of a bounded remote decode stream.
pub struct StreamTicket {
    id: String,
    inbox: ReplyInbox,
    receiver: mpsc::Receiver<Value>,
}

impl StreamTicket {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub async fn recv(&mut self) -> Option<Value> {
        self.receiver.recv().await
    }
}

impl Drop for StreamTicket {
    fn drop(&mut self) {
        self.inbox.cancel(&self.id);
    }
}

// ── What the gateway sends along with the frame ───────────────────────────────

/// Addresses through which the tail reaches this gateway, plus the inbox where the
/// result will be looked up.
#[derive(Clone)]
pub struct DirectReply {
    inbox: ReplyInbox,
    urls: Vec<String>,
    quic_addresses: Vec<String>,
    /// The gateway's `PeerId` and multiaddresses, so the delivery lands on the libp2p
    /// tunnel when no URL in `urls` is reachable from where the tail is.
    /// Behind NAT/CGNAT that is the normal case, not the exception: the URLs come
    /// from the gateway's local interfaces and are worth nothing at the other end.
    libp2p_peer_id: Option<String>,
    libp2p_addrs: Vec<String>,
    /// node_id of whoever will receive the response.
    ///
    /// It travels in the `reply_to` because whoever delivers needs it to pin the
    /// gateway's certificate: with the cluster on TLS, a URL without an identity cannot
    /// be verified. A peer on an old binary does not send the field, and then the
    /// delivery falls to the client without pinning - see `deliver_result`.
    node_id: String,
}

impl DirectReply {
    /// `None` when this node knows no address of itself: without that the
    /// tail would have nowhere to answer, and the right thing is to fall back to the usual ring
    /// instead of dispatching a frame that never comes back.
    pub fn new(inbox: ReplyInbox, urls: Vec<String>, node_id: String) -> Option<Self> {
        if urls.is_empty() {
            return None;
        }
        Some(Self {
            inbox,
            urls,
            quic_addresses: Vec::new(),
            libp2p_peer_id: None,
            libp2p_addrs: Vec::new(),
            node_id,
        })
    }

    /// This gateway's libp2p identity, so the return has a path when
    /// no direct URL is reachable.
    pub fn with_libp2p(mut self, peer_id: Option<String>, addrs: Vec<String>) -> Self {
        self.libp2p_peer_id = peer_id;
        self.libp2p_addrs = addrs;
        self
    }

    /// `host:port` addresses of the gateway's QUIC endpoint. They are kept apart
    /// from the HTTP URLs so the callback never tries to turn `http://...`
    /// into a UDP destination.
    pub fn with_quic_addresses(mut self, addresses: Vec<String>) -> Self {
        self.quic_addresses = addresses;
        self
    }

    pub fn ticket_for_hops(&self, expected_hops: usize) -> Ticket {
        self.inbox.register_with_expected_transport(expected_hops)
    }

    pub fn reply_to(&self, ticket: &str) -> Value {
        json!({
            "urls": self.urls,
            "quic_addresses": self.quic_addresses,
            "libp2p_peer_id": self.libp2p_peer_id,
            "libp2p_addrs": self.libp2p_addrs,
            "ticket": ticket,
            "node_id": self.node_id,
        })
    }

    pub fn stream_ticket(&self) -> StreamTicket {
        self.inbox.register_stream()
    }

    pub fn stream_reply_to(&self, ticket: &str) -> Value {
        self.reply_to(ticket)
    }

    pub(crate) fn trace_inbox(&self) -> ReplyInbox {
        self.inbox.clone()
    }

    pub fn record_transport(
        &self,
        ticket: &str,
        hop_index: usize,
        sent: &crate::relay::SentFrame,
    ) -> bool {
        self.inbox
            .record_transport(ticket, hop_index, transport_receipt(hop_index, sent))
    }
}

fn transport_receipt(hop_index: usize, sent: &crate::relay::SentFrame) -> Value {
    json!({
        "hop_index": hop_index,
        "transport": sent.telemetry_transport().as_str(),
        "transport_fallback": sent.transport_fallback,
        "transport_fallback_reason": sent.transport_fallback_reason,
        "round_trip_ms": sent.round_trip_ms,
        "processing_ms": sent.meta.get("processing_ms").cloned().unwrap_or(json!(0)),
        "transport_rtt_us": sent.transport_rtt_us,
        "bytes_sent": sent.bytes_sent,
        "bytes_received": sent.bytes_received,
        "tls": sent.tls,
        "tls_handshake_ms": sent.tls_handshake_ms,
        "client_encode_ms": sent.client_encode_ms,
        "client_f16_encode_ms": sent.client_f16_encode_ns as f64 / 1_000_000.0,
        "client_f16_decode_ms": sent.client_f16_decode_ns as f64 / 1_000_000.0,
        "response_headers_ms": sent.response_headers_ms,
        "response_body_ms": sent.response_body_ms,
        "client_decode_ms": sent.client_decode_ms,
    })
}

/// This node's gateway URLs, in the order a peer should try them.
///
/// Same preference rule as `gateway::self_dial_host`: the address pinned
/// by the operator is worth more than the LAN's, because across sites a `192.168.x.y` is
/// as useless as the loopback. The candidates enumerated from the interfaces come
/// after, and there the loopback does not enter: the tail is almost never this machine.
///
/// `pinned` is the address the operator pinned (`advertise_addr`), and only it may
/// be loopback: on a single machine, pointing at itself is the right configuration
/// and was the choice of whoever configured it. What cannot enter is the loopback that
/// shows up on its own, from the `127.0.0.1` fallback of `gateway::self_dial_host` or from the
/// interface enumeration: there the tail on another machine delivers to *its own*
/// loopback and the response is lost, with a 600 s deadline. With no usable URL
/// the list is empty and the plan falls back to the ring, which works.
pub fn self_reply_urls(
    pinned: Option<String>,
    hosts: impl IntoIterator<Item = String>,
    gateway_port: u16,
) -> Vec<String> {
    let mut urls: Vec<String> = Vec::new();
    let pinned_hosts = pinned.into_iter().map(|h| (h, true));
    for (host, pinned_by_operator) in pinned_hosts.chain(hosts.into_iter().map(|h| (h, false))) {
        let host = crate::network::ip_host(&host).to_string();
        if host.is_empty() || (!pinned_by_operator && is_unroutable_host(&host)) {
            continue;
        }
        // The scheme follows the cluster policy: these URLs are the ones the tail
        // will dial back, and in a TLS cluster this node's `/relay/result`
        // refuses cleartext coming from outside.
        let url = crate::peer_client::peer_base_url(
            &host,
            gateway_port,
            crate::peer_client::tls_enabled(),
        );
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

/// An address that is no use for another machine to reach us: the bind wildcard and
/// the loopback, in v4 and v6. The loopback is what misleads most, because
/// `gateway::self_dial_host` falls back to `127.0.0.1` when it finds no better
/// address: the list is non-empty, direct mode turns on, and the tail on another
/// machine delivers the result to *its own* loopback, where nobody is waiting.
fn is_unroutable_host(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip.is_unspecified(),
        // A name that is not an IP (a `--public-addr` with a domain, for example)
        // passes: resolving it here would be guessing.
        Err(_) => bare.eq_ignore_ascii_case("localhost"),
    }
}

// ── Lado de quem recebe o resultado (gateway) ─────────────────────────────────

/// Handler of `/v1/relay-result`: looks up the ticket and resolves the channel.
///
/// The route does not use the client's API key. The ticket is a UUID v4 drawn per
/// frame and known only to the chain; it is the credential of the delivery. A
/// result without a live ticket is refused with `410`, which is what tells whoever
/// delivered that the gateway has already given up.
pub fn handle_relay_result(inbox: &ReplyInbox, body: Bytes) -> Response {
    let (mut meta, _tensor) = match crate::wire::decode(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("invalid frame: {e}")).into_response(),
    };
    let Some(ticket) = meta
        .get("ticket")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "result without a ticket").into_response();
    };
    if meta
        .get("transport_update")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let Some(hop_index) = meta.get("hop_index").and_then(Value::as_u64) else {
            return (StatusCode::BAD_REQUEST, "telemetry without a hop index").into_response();
        };
        if inbox.record_transport(&ticket, hop_index as usize, meta) {
            return (StatusCode::OK, "ok").into_response();
        }
        return (StatusCode::GONE, "unknown ticket").into_response();
    }
    // The bytes that crossed the network on the way back. Only the receiver knows.
    meta["result_bytes"] = json!(body.len());
    if inbox.deliver(&ticket, meta) {
        (StatusCode::OK, "ok").into_response()
    } else {
        debug!("direct result for an unknown ticket (the gateway already gave up)");
        (StatusCode::GONE, "unknown ticket").into_response()
    }
}

/// Receives one batch from a remote decode producer. Delivery awaits the
/// bounded inbox queue, so a slow client propagates backpressure all the way
/// to the remote emitter.
pub async fn handle_remote_stream_result(inbox: &ReplyInbox, body: Bytes) -> Response {
    let (mut meta, _tensor) = match crate::wire::decode(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("invalid frame: {e}")).into_response(),
    };
    let Some(ticket) = meta
        .get("ticket")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "batch without a ticket").into_response();
    };
    meta["stream_batch_bytes"] = json!(body.len());
    if inbox.deliver_stream(&ticket, meta).await {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::GONE, "unknown stream").into_response()
    }
}

/// Waits for the result and runs a synchronous action when the head's ack
/// arrives. If the result arrives first, the ack continues in a detached
/// task: it is released without waiting, but the first hop's evidence is not
/// lost when the tail's response beats the head's HTTP response.
pub async fn await_result_with_ack<F, C>(
    ack: F,
    ticket: &mut Ticket,
    head_label: &str,
    deadline: Duration,
    on_ack: C,
) -> Result<Value>
where
    F: Future<Output = Result<crate::relay::SentFrame>> + Send + 'static,
    C: FnOnce(&crate::relay::SentFrame) + Send + 'static,
{
    let mut ack = Box::pin(ack);
    let mut on_ack = Some(on_ack);
    let deadline_duration = deadline;
    let deadline = tokio::time::sleep(deadline_duration);
    tokio::pin!(deadline);
    let mut acked = false;

    loop {
        tokio::select! {
            biased;
            got = &mut ticket.rx => {
                if let Some(on_ack) = on_ack.take() {
                    tokio::spawn(async move {
                        if let Ok(frame) = ack.await {
                            on_ack(&frame);
                        }
                    });
                }
                return got.map_err(|_| anyhow!("direct result lost before arriving"));
            }
            sent = &mut ack, if !acked => {
                acked = true;
                match sent {
                    // Old peer: instead of accepting and relaying, it bubbled the
                    // whole response. It counts as a result - it is what keeps
                    // a half-updated cluster working even if the
                    // capability negotiation slips through.
                    Ok(frame) if frame.meta.get("token_id").is_some() => {
                        if let Some(on_ack) = on_ack.take() {
                            on_ack(&frame);
                        }
                        return Ok(frame.meta)
                    }
                    Ok(frame) => {
                        if let Some(on_ack) = on_ack.take() {
                            on_ack(&frame);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            _ = &mut deadline => {
                return Err(anyhow!(
                    "no hop delivered the result in {:?} (chain dispatched at {head_label})",
                    deadline_duration
                ));
            }
        }
    }
}

/// The error a hop delivered in place of the result, if that is what arrived.
pub fn result_failure(meta: &Value) -> Option<crate::relay::RelayFailure> {
    serde_json::from_value(meta.get("error")?.clone()).ok()
}

// ── Delivering side (tail, or the hop that failed) ────────────────────────────

/// What happened to a delivery to the gateway.
///
/// It exists because "did not deliver" has two opposite meanings for whoever
/// emits a stream: the gateway already gave up (nobody to report to) and
/// no address accepted (it must be reported, or the gateway waits out the
/// whole frame deadline (`relay::frame_deadline`) for a failure this side already knows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// O gateway recebeu.
    Accepted,
    /// The gateway answered `410`: it already gave up on this frame.
    GatewayGone,
    /// No return address accepted the delivery.
    Failed,
}

impl Delivery {
    pub fn accepted(self) -> bool {
        matches!(self, Delivery::Accepted)
    }
}

/// Delivers a result to the gateway, trying each `reply_to` URL in order.
///
/// A machine may be reachable through several addresses and none of them is
/// certain from outside: the gateway announces all the ones it knows of itself and whoever delivers
/// finds out which one works. If all fail, the gateway closes at the deadline - there is no
/// third way, and inventing one would be worse than the wait.
pub async fn deliver_result(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    meta: Value,
) -> bool {
    deliver_meta_at(http, reply_to, RELAY_RESULT_PATH, meta)
        .await
        .accepted()
}

/// Delivers one remote decode batch to the stream inbox. The HTTP response is
/// intentionally awaited: the gateway's bounded queue is the backpressure
/// signal for the emitter.
pub async fn deliver_stream_batch(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    meta: Value,
) -> Delivery {
    deliver_meta_at(http, reply_to, REMOTE_STREAM_RESULT_PATH, meta).await
}

/// Receives a remote callback inside the persistent `/v1/link` channel. The internal
/// frame is a one-item batch so that the protocol itself carries the
/// route and keeps the channel's correlation; the item contains the same metadata that the
/// HTTP callback endpoint would receive.
pub async fn handle_remote_stream_link(inbox: &ReplyInbox, body: Bytes) -> Response {
    let (header, mut items) = match crate::wire::decode_batch_items(&body) {
        Ok(value) => value,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, format!("invalid frame: {error}")).into_response()
        }
    };
    if header.kind != REMOTE_STREAM_RESULT_PATH || items.len() != 1 {
        return (
            StatusCode::BAD_REQUEST,
            "remote return frame must contain one item",
        )
            .into_response();
    }
    let mut meta = items.remove(0).meta;
    let Some(ticket) = meta
        .get("ticket")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return (StatusCode::BAD_REQUEST, "batch without a ticket").into_response();
    };
    meta["stream_batch_bytes"] = json!(body.len());
    if inbox.deliver_stream(&ticket, meta).await {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::GONE, "unknown stream").into_response()
    }
}

/// Delivers `remote-stream` callbacks through a persistent channel. The first
/// failure deactivates the channel for the rest of this stream and marks each following
/// callback as an explicit fallback to per-request HTTP(S). A 410 ACK is
/// different: it proves the gateway cancelled the ticket, so the remote
/// producer receives `GatewayGone` and stops instead of continuing to generate. This keeps
/// the normal path free of per-token request/reply without turning a transient
/// drop into a storm of open attempts.
pub struct RemoteStreamDelivery {
    http: crate::peer_client::PeerClients,
    reply_to: Value,
    persistent_failure: Option<String>,
    callback_status: Option<crate::link::CallbackStatus>,
}

impl RemoteStreamDelivery {
    pub fn new(http: crate::peer_client::PeerClients, reply_to: Value) -> Self {
        Self {
            http,
            reply_to,
            persistent_failure: None,
            callback_status: None,
        }
    }

    pub async fn deliver(&mut self, meta: Value) -> Delivery {
        let link_transport = if crate::peer_client::tls_enabled() {
            "https-link"
        } else {
            "http"
        };
        if let Some(status) = &self.callback_status {
            if status.gateway_gone() {
                return Delivery::GatewayGone;
            }
            if status.transport_failed() {
                self.persistent_failure = Some(format!(
                    "{link_transport}: async ACK of the callback failed; using {link_transport}"
                ));
                self.callback_status = None;
            }
        }
        let Some(session_id) = meta
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|session_id| !session_id.trim().is_empty())
        else {
            warn!("remote-stream callback without session_id; refusing a synthetic identity");
            return Delivery::Failed;
        };
        let frame_id = meta
            .get("stream_id")
            .and_then(Value::as_str)
            .zip(
                meta.get("remote_stream_frame_index")
                    .and_then(Value::as_u64),
            )
            .map(|(stream_id, index)| format!("{stream_id}:{index}"))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let node_id = self.reply_to.get("node_id").and_then(Value::as_str);
        let urls: Vec<String> = self
            .reply_to
            .get("urls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();

        // With a route pinned up to the gateway (`route_plan`), it decides: HTTP uses the
        // persistent channel at that address; QUIC and tunnel go straight to the
        // per-request delivery, which follows the pinned route. Without a pinned route,
        // QUIC is the authoritative preference for a new delivery. Do not
        // silently keep using the persistent TCP link when the cluster opted
        // into QUIC: otherwise the MTP/remote-stream trace would describe a
        // different transport than the one the gateway requested.
        let _route_use = node_id
            .map(|id| crate::route_plan::book().begin_use(id, crate::route_plan::Plane::Gateway));
        let pinned = node_id
            .and_then(|id| crate::route_plan::book().active(id, crate::route_plan::Plane::Gateway));
        let use_link = match pinned.as_ref().map(|route| route.kind) {
            None => !self.http.effective_quic_enabled(),
            Some(crate::route_plan::RouteKind::Http) => true,
            Some(_) => false,
        };
        if pinned.is_some() && !use_link {
            return deliver_stream_batch(&self.http, &self.reply_to, meta).await;
        }
        let urls = match &pinned {
            Some(route) => vec![route.base_url.clone()],
            None => urls,
        };
        if self.persistent_failure.is_none() && use_link {
            let reason = match (node_id, urls.is_empty()) {
                (None, _) => Some(format!("{link_transport}: reply_to sem node_id")),
                (Some(_), true) => Some(format!("{link_transport}: reply_to without an address")),
                (Some(node_id), false) => {
                    let mut link_meta = meta.clone();
                    link_meta["ticket"] =
                        self.reply_to.get("ticket").cloned().unwrap_or(Value::Null);
                    link_meta["remote_stream_transport"] =
                        json!(if crate::peer_client::tls_enabled() {
                            link_transport
                        } else {
                            "http"
                        });
                    link_meta["remote_stream_transport_fallback"] = json!(false);
                    match crate::wire::encode_batch_items(
                        REMOTE_STREAM_RESULT_PATH,
                        &frame_id,
                        &[crate::wire::BatchPayloadItem {
                            item_id: frame_id.clone(),
                            session_id: session_id.to_string(),
                            meta: link_meta,
                            tensor: None,
                            dtype: crate::wire::WireDtype::F16,
                        }],
                    ) {
                        Ok(body) => match crate::link::send_body_without_response_with_session(
                            &self.http,
                            node_id,
                            "",
                            &urls,
                            &frame_id,
                            Bytes::from(body),
                            "remote-stream-return",
                            meta.get("session_id").and_then(Value::as_str),
                        )
                        .await
                        {
                            Ok(status) => {
                                self.callback_status = Some(status);
                                return Delivery::Accepted;
                            }
                            Err(error) => Some(format!("{link_transport}: {error}")),
                        },
                        Err(error) => Some(format!("{link_transport}: invalid frame: {error}")),
                    }
                }
            };
            self.persistent_failure = reason;
        }

        let mut fallback_meta = meta;
        let fallback_transport = if crate::peer_client::tls_enabled() {
            "https"
        } else {
            "http"
        };
        fallback_meta["remote_stream_transport"] = json!(fallback_transport);
        fallback_meta["remote_stream_transport_fallback"] = json!(true);
        fallback_meta["remote_stream_transport_fallback_reason"] =
            json!(self.persistent_failure.clone().unwrap_or_else(|| {
                format!("{fallback_transport}: persistent channel unavailable")
            }));
        deliver_stream_batch(&self.http, &self.reply_to, fallback_meta).await
    }
}

/// Delivers a transport evidence through the same authenticated return as the
/// result. The POST is small and stays outside the bytes of the measured traversal.
pub async fn deliver_transport(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    hop_index: usize,
    sent: &crate::relay::SentFrame,
) -> bool {
    let mut meta = transport_receipt(hop_index, sent);
    meta["transport_update"] = json!(true);
    deliver_meta_at(http, reply_to, RELAY_RESULT_PATH, meta)
        .await
        .accepted()
}

async fn deliver_meta_at(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    path: &str,
    meta: Value,
) -> Delivery {
    let Some(ticket) = reply_to.get("ticket").and_then(|v| v.as_str()) else {
        warn!("reply_to without a ticket — direct result discarded");
        return Delivery::Failed;
    };
    let mut base_meta = meta;
    base_meta["ticket"] = json!(ticket);

    let urls = reply_to
        .get("urls")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let urls: Vec<String> = urls
        .iter()
        .filter_map(|url| url.as_str().map(str::to_owned))
        .collect();
    let route_key = reply_to
        .get("node_id")
        .and_then(|value| value.as_str())
        .map(|node_id| format!("reply:{node_id}"))
        .unwrap_or_else(|| format!("reply:urls:{}", urls.join("\u{1f}")));
    // Without a node_id in the `reply_to` the sender runs an old binary: there is nothing to
    // pin, and the delivery goes through the shared client. In a strict cluster
    // it will be refused on the other side, which is the correct result - what
    // cannot happen is delivering in cleartext pretending it was verified.
    let client = match reply_to.get("node_id").and_then(|v| v.as_str()) {
        Some(id) => match http.for_peer(id) {
            Ok(c) => c,
            Err(e) => {
                warn!("no client to deliver the result to {id}: {e}");
                return Delivery::Failed;
            }
        },
        None => http.plain(),
    };
    if let Some(node_id) = reply_to.get("node_id").and_then(Value::as_str) {
        if let Some(delivery) =
            deliver_pinned(http, reply_to, node_id, path, &base_meta, &client).await
        {
            return delivery;
        }
    }
    let ordered = http.route_order(&route_key, &urls);
    let quic_addresses: Vec<String> = reply_to
        .get("quic_addresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let last_index = ordered.len().saturating_sub(1);
    let mut last = String::new();
    let mut quic_failed = false;
    for (index, url) in ordered.into_iter().enumerate() {
        let target = format!("{url}{path}");
        // The last candidate gets the long cap: there is no third way after
        // it, and giving up on a merely slow delivery costs the gateway's
        // whole deadline. See `LAST_DELIVERY_TIMEOUT`.
        let ceiling = if index == last_index {
            LAST_DELIVERY_TIMEOUT
        } else {
            DELIVERY_TIMEOUT
        };
        if http.quic_enabled() && reply_to.get("node_id").and_then(Value::as_str).is_some() {
            let attempt_meta = delivery_meta(
                &base_meta,
                path,
                "quic",
                quic_failed,
                quic_failed.then_some(last.as_str()),
                None,
            );
            let body = match encode_delivery_body(&attempt_meta) {
                Ok(bytes) => bytes,
                Err(error) => {
                    warn!("direct result could not be encoded: {error}");
                    return Delivery::Failed;
                }
            };
            match http
                .quic_post(
                    reply_to
                        .get("node_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    &url,
                    &quic_addresses,
                    path,
                    body.clone(),
                    None,
                    ceiling,
                )
                .await
            {
                Ok(response) if response.status.is_success() => {
                    http.remember_route_success(&route_key, &url);
                    return Delivery::Accepted;
                }
                Ok(response) if response.status == StatusCode::GONE => {
                    debug!("the gateway had already given up on the frame delivered at {target}");
                    return Delivery::GatewayGone;
                }
                Ok(response) => {
                    quic_failed = true;
                    debug!(
                        "{target} answered {} over QUIC; trying TCP",
                        response.status
                    );
                }
                Err(error) => {
                    quic_failed = true;
                    last = format!("{target} via QUIC: {error}");
                    debug!("QUIC fallback for direct delivery: {last}");
                }
            }
        }
        let transport = tcp_transport_for_url(&url);
        let attempt_meta = delivery_meta(
            &base_meta,
            path,
            transport,
            quic_failed,
            quic_failed.then_some(last.as_str()),
            None,
        );
        let body = match encode_delivery_body(&attempt_meta) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!("direct result could not be encoded: {error}");
                return Delivery::Failed;
            }
        };
        let request = client
            .post(&target)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body.clone())
            .timeout(ceiling);
        match request.send().await {
            Ok(r) if r.status().is_success() => {
                http.remember_route_success(&route_key, &url);
                return Delivery::Accepted;
            }
            // `410` is an answer, not a route failure: the gateway gave up on this
            // frame. Trying the next address would only find the same inbox.
            Ok(r) if r.status() == StatusCode::GONE => {
                debug!("the gateway had already given up on the frame delivered at {target}");
                return Delivery::GatewayGone;
            }
            Ok(r) => last = format!("{target} answered {}", r.status()),
            Err(e) => {
                http.remember_route_failure(&route_key, &url);
                last = format!("{target}: {e}");
            }
        }
    }
    // No direct URL got through. Behind NAT/CGNAT this is the normal case: the
    // `reply_to` URLs come from the gateway's local interfaces and are worth
    // nothing from outside its network. The libp2p tunnel is the same path the outbound
    // already uses, only on the return - without it the frame dies silently, and the gateway only
    // notices when the frame deadline expires.
    if let Some(delivery) = deliver_via_tunnel(reply_to, path, &base_meta, &last).await {
        return delivery;
    }

    // An empty reason was the signature of the empty candidate list defect.
    // Today `route_order` never empties the list, so only the case of the
    // `reply_to` carrying no address at all remains - and that is said with every
    // letter, not with an empty parenthesis.
    if last.is_empty() {
        last = "the reply_to brought no return address".to_string();
    }
    warn!("direct result did not reach the gateway ({last})");
    Delivery::Failed
}

/// How many pinned routes a delivery goes through before the full ladder.
const MAX_PINNED_DELIVERIES: usize = 8;

/// Delivers through the pinned route to the gateway (`route_plan`, `Gateway` plan).
/// A failure repeats the same route once and then moves to the next measured one;
/// `None` hands the delivery back to the full ladder of `reply_to`, which still tries
/// all the addresses and the tunnel. The plan belongs to whoever delivers: the return is its
/// own edge, and whoever measured the outbound knows nothing about it.
async fn deliver_pinned(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    node_id: &str,
    path: &str,
    base_meta: &Value,
    client: &reqwest::Client,
) -> Option<Delivery> {
    use crate::route_plan::{book, FailureOutcome, Plane, RouteKind};

    // The return leg to the gateway also pins the route while delivering.
    let _route_use = book().begin_use(node_id, Plane::Gateway);
    for _ in 0..MAX_PINNED_DELIVERIES {
        let route = book().active(node_id, Plane::Gateway)?;
        let delivered = match route.kind {
            RouteKind::Tunnel => deliver_via_tunnel(reply_to, path, base_meta, "").await,
            RouteKind::Http => {
                let attempt_meta = delivery_meta(
                    base_meta,
                    path,
                    tcp_transport_for_url(&route.base_url),
                    false,
                    None,
                    None,
                );
                let body = encode_delivery_body(&attempt_meta).ok()?;
                let target = format!("{}{path}", route.base_url.trim_end_matches('/'));
                match client
                    .post(&target)
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .body(body)
                    .timeout(LAST_DELIVERY_TIMEOUT)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => Some(Delivery::Accepted),
                    Ok(r) if r.status() == StatusCode::GONE => Some(Delivery::GatewayGone),
                    Ok(r) => {
                        debug!("pinned route {target} answered {}", r.status());
                        None
                    }
                    Err(error) => {
                        debug!("pinned route {target} failed: {error}");
                        None
                    }
                }
            }
            RouteKind::Quic => {
                let attempt_meta = delivery_meta(base_meta, path, "quic", false, None, None);
                let body = encode_delivery_body(&attempt_meta).ok()?;
                let addresses: Vec<String> = route.quic_addr.iter().cloned().collect();
                match http
                    .quic_post(
                        node_id,
                        &route.base_url,
                        &addresses,
                        path,
                        body,
                        None,
                        LAST_DELIVERY_TIMEOUT,
                    )
                    .await
                {
                    Ok(response) if response.status.is_success() => Some(Delivery::Accepted),
                    Ok(response) if response.status == StatusCode::GONE => {
                        Some(Delivery::GatewayGone)
                    }
                    Ok(response) => {
                        debug!("pinned QUIC route answered {}", response.status);
                        None
                    }
                    Err(error) => {
                        debug!("pinned QUIC route failed: {error}");
                        None
                    }
                }
            }
        };
        match delivered {
            Some(delivery) => {
                book().record_success(node_id, Plane::Gateway);
                return Some(delivery);
            }
            None => {
                if book().record_failure(node_id, Plane::Gateway) == FailureOutcome::Exhausted {
                    return None;
                }
            }
        }
    }
    None
}

/// Last way of the delivery: the libp2p tunnel to the gateway.
///
/// `None` when the `reply_to` carries no libp2p identity (gateway on a prior
/// binary) or when the tunnel itself does not deliver - in both cases the caller
/// moves on to the failure log and the gateway closes at the deadline. The route enters through the
/// closed list of `infer_tunnel::gateway_path_is_allowed`.
async fn deliver_via_tunnel(
    reply_to: &Value,
    path: &str,
    base_meta: &Value,
    direct_error: &str,
) -> Option<Delivery> {
    let peer_id = reply_to.get("libp2p_peer_id").and_then(Value::as_str)?;
    let addrs: Vec<String> = reply_to
        .get("libp2p_addrs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let tunnel = crate::peer_client::TunnelPeer::from_parts(peer_id, addrs)?;

    // The annotated transport is what actually carried the frame: publishing "http"
    // here would make the gateway's trace lie about the path of the return.
    let attempt_meta = delivery_meta(
        base_meta,
        path,
        "libp2p-tunnel",
        !direct_error.is_empty(),
        (!direct_error.is_empty()).then_some(direct_error),
        None,
    );
    let body = match encode_delivery_body(&attempt_meta) {
        Ok(bytes) => bytes,
        Err(error) => {
            warn!("direct result could not be encoded for the tunnel: {error}");
            return None;
        }
    };

    match crate::peer_client::tunnel_gateway_call(
        &tunnel,
        "POST",
        path,
        body,
        vec![(
            reqwest::header::CONTENT_TYPE.to_string(),
            "application/octet-stream".to_string(),
        )],
        LAST_DELIVERY_TIMEOUT,
    )
    .await
    {
        Ok(response) if response.is_success() => {
            debug!("direct result delivered through the libp2p tunnel ({direct_error})");
            Some(Delivery::Accepted)
        }
        Ok(response) if response.status == StatusCode::GONE.as_u16() => {
            debug!("the gateway had already given up on the frame delivered through the tunnel");
            Some(Delivery::GatewayGone)
        }
        Ok(response) => {
            warn!(
                "libp2p tunnel answered {} to the direct result",
                response.status
            );
            None
        }
        Err(error) => {
            warn!("direct result did not arrive through the libp2p tunnel: {error}");
            None
        }
    }
}

fn tcp_transport_for_url(url: &str) -> &'static str {
    if url.starts_with("https://") {
        "https"
    } else {
        "http"
    }
}

fn encode_delivery_body(meta: &Value) -> Result<Vec<u8>> {
    crate::wire::encode(meta, None, crate::wire::WireDtype::F16).map_err(|error| anyhow!("{error}"))
}

/// Attaches to the frame the transport that will actually be used in this attempt.
/// The metadata is encoded again per attempt so that a QUIC
/// -> TCP fallback is never published as QUIC in the gateway's trace.
fn delivery_meta(
    base: &Value,
    path: &str,
    transport: &str,
    fallback: bool,
    fallback_reason: Option<&str>,
    rtt_ms: Option<u64>,
) -> Value {
    let mut meta = base.clone();
    meta["delivery_transport"] = json!(transport);
    meta["delivery_transport_fallback"] = json!(fallback);
    if let Some(reason) = fallback_reason.filter(|reason| !reason.is_empty()) {
        meta["delivery_transport_fallback_reason"] = json!(reason);
    }
    if path == RELAY_RESULT_PATH
        && !meta
            .get("transport_update")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        let mut observation = json!({
            "transport": transport,
            "transport_fallback": fallback,
            "transport_fallback_reason": fallback_reason,
        });
        if let Some(rtt_ms) = rtt_ms {
            observation["round_trip_ms"] = json!(rtt_ms);
        }
        meta["pipeline_return_transport"] = observation;
    }
    if path == REMOTE_STREAM_RESULT_PATH {
        meta["remote_stream_transport"] = json!(transport);
        meta["remote_stream_transport_fallback"] = json!(fallback);
        if let Some(reason) = fallback_reason.filter(|reason| !reason.is_empty()) {
            meta["remote_stream_transport_fallback_reason"] = json!(reason);
        }
        if let Some(rtt_ms) = rtt_ms {
            meta["remote_stream_transport_rtt_ms"] = json!(rtt_ms);
        }
    }
    meta
}

/// Delivers a failure in place of the result, so the gateway names the node that
/// died instead of waiting out the whole deadline.
pub async fn deliver_failure(
    http: &crate::peer_client::PeerClients,
    reply_to: &Value,
    failure: crate::relay::RelayFailure,
) {
    let meta = json!({ "error": failure });
    deliver_result(http, reply_to, meta).await;
}

// ── Telemetry accumulated on the outbound ─────────────────────────────────────

/// A hop's contribution to the `hop_timings` that travels **on the outbound**.
///
/// In the bubbled ring each entry was assembled by whoever called, on the return. With no
/// return, each hop describes itself and pushes the list onward; the tail
/// delivers the set along with the result, in the same head→tail order that
/// `RequestTrace::record_hop_from_json` already expects.
///
/// `bytes_sent` is the frame that **entered** this hop, which is exactly what the
/// previous one put on the wire. The call that originated the entry sends a later
/// evidence with the clock and transport observed on the client.
pub fn hop_entry(processing_ms: &Value, node_timings: &Value, inbound_bytes: usize) -> Value {
    json!({
        "round_trip_ms": processing_ms.clone(),
        "processing_ms": processing_ms.clone(),
        "bytes_sent": inbound_bytes,
        "bytes_received": 0,
        "client_encode_ms": 0,
        "client_f16_encode_ms": 0,
        "client_f16_decode_ms": 0,
        "response_headers_ms": 0,
        "response_body_ms": 0,
        "client_decode_ms": 0,
        "node_timings": node_timings.clone(),
    })
}

/// Variant used by inference hops after the selected wire dtype is known.
/// `inbound_bytes` remains the complete frame size for transport telemetry;
/// the hidden-state counters isolate the tensor payload on either side of the
/// crossing. Direct mode records both sides because no caller waits for a
/// response frame from each hop.
pub fn hop_entry_with_state(
    processing_ms: &Value,
    node_timings: &Value,
    inbound_bytes: usize,
    hidden_state_bytes_received: usize,
    hidden_state_bytes_sent: usize,
    dtype: crate::wire::WireDtype,
) -> Value {
    let mut entry = hop_entry(processing_ms, node_timings, inbound_bytes);
    entry["hidden_state_bytes_sent"] = json!(hidden_state_bytes_sent);
    entry["hidden_state_bytes_received"] = json!(hidden_state_bytes_received);
    entry["hidden_state_dtype"] = json!(dtype.as_str());
    entry
}

/// Adds this hop's entry to the list that came on the outbound.
pub fn accumulate(inbound_meta: &Value, entry: Value) -> Vec<Value> {
    let mut hops: Vec<Value> = inbound_meta
        .get("hop_timings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    hops.push(entry);
    hops
}

/// Replaces a hop's estimate with the measurement of the call that originated it.
/// The index is the position of the destination in the chain, so no time of a
/// traversal can be booked on the first hop for convenience.
// Kept for the unit proof of the index mapping; the direct path in
// production records the measurement in the inbox with `record_transport`.
#[cfg(test)]
pub fn attach_transport(hops: &mut [Value], destination: usize, sent: &crate::relay::SentFrame) {
    let Some(hop) = hops.get_mut(destination) else {
        return;
    };
    hop["round_trip_ms"] = json!(sent.round_trip_ms);
    hop["bytes_sent"] = json!(sent.bytes_sent);
    hop["bytes_received"] = json!(sent.bytes_received);
    hop["client_encode_ms"] = json!(sent.client_encode_ms);
    hop["client_f16_encode_ms"] = json!(sent.client_f16_encode_ns as f64 / 1_000_000.0);
    hop["client_f16_decode_ms"] = json!(sent.client_f16_decode_ns as f64 / 1_000_000.0);
    hop["response_headers_ms"] = json!(sent.response_headers_ms);
    hop["response_body_ms"] = json!(sent.response_body_ms);
    hop["client_decode_ms"] = json!(sent.client_decode_ms);
    hop["tls"] = json!(sent.tls);
    hop["tls_handshake_ms"] = json!(sent.tls_handshake_ms);
    hop["transport_rtt_us"] = json!(sent.transport_rtt_us);
    hop["transport"] = json!(sent.telemetry_transport().as_str());
    hop["transport_fallback"] = json!(sent.transport_fallback);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_410_ack_from_the_callback_stops_the_stream_without_fallback() {
        use axum::{routing::post, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new()
            .route(
                crate::link::LINK_PATH,
                post(|body: axum::body::Body| async move {
                    crate::link::serve(body, |_path, _body| async move {
                        Ok(Response::builder()
                            .status(StatusCode::GONE)
                            .body(axum::body::Body::from("unknown stream"))
                            .unwrap())
                    })
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::disable());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).tcp_nodelay(true).await;
        });

        let url = format!("http://{addr}");
        let reply_to = json!({
            "ticket": "gone-ticket",
            "node_id": "gateway-gone",
            "urls": [url],
        });
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let mut delivery = RemoteStreamDelivery::new(clients, reply_to);
        let first = json!({
            "session_id": "session-gone",
            "stream_id": "stream-gone",
            "remote_stream_frame_index": 0,
            "accepted": [{"token_id": 1, "text": "x"}],
        });

        assert_eq!(delivery.deliver(first).await, Delivery::Accepted);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if delivery
                    .callback_status
                    .as_ref()
                    .is_some_and(crate::link::CallbackStatus::gateway_gone)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the 410 ACK must reach the emitter");

        assert_eq!(
            delivery
                .deliver(json!({
                    "session_id": "session-gone",
                    "stream_id": "stream-gone",
                    "remote_stream_frame_index": 1,
                    "accepted": [{"token_id": 2, "text": "y"}],
                }))
                .await,
            Delivery::GatewayGone
        );
    }

    #[tokio::test]
    async fn a_blackholed_reply_address_is_tried_only_once_across_stream_batches() {
        use axum::{body::Bytes, routing::post, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;

        let blackhole_hits = Arc::new(AtomicUsize::new(0));
        let blackhole_hits_task = blackhole_hits.clone();
        let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = blackhole.accept().await else {
                    break;
                };
                blackhole_hits_task.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _stream = stream;
                    std::future::pending::<()>().await;
                });
            }
        });

        let healthy_hits = Arc::new(AtomicUsize::new(0));
        let healthy_hits_task = healthy_hits.clone();
        let healthy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let healthy_addr = healthy_listener.local_addr().unwrap();
        let healthy = Router::new().route(
            REMOTE_STREAM_RESULT_PATH,
            post(move |_body: Bytes| {
                let healthy_hits = healthy_hits_task.clone();
                async move {
                    healthy_hits.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "ok")
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(healthy_listener, healthy).await;
        });

        let reply_to = json!({
            "ticket": "blackhole-ticket",
            "node_id": "gateway-blackhole-test",
            "urls": [
                format!("http://{blackhole_addr}"),
                format!("http://{healthy_addr}"),
            ],
        });
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        let started = Instant::now();

        assert!(deliver_stream_batch(&http, &reply_to, json!({"index": 1}))
            .await
            .accepted());
        assert!(deliver_stream_batch(&http, &reply_to, json!({"index": 2}))
            .await
            .accepted());

        assert_eq!(blackhole_hits.load(Ordering::SeqCst), 1);
        assert_eq!(healthy_hits.load(Ordering::SeqCst), 2);
        // What is proven is that the black hole was not paid twice: what
        // avoids it is the preference for the route that answered, not the quarantine.
        // The budget is the cap of one attempt, not a magic number.
        assert!(
            started.elapsed() < DELIVERY_TIMEOUT * 2,
            "the dead route was paid again: {:?}",
            started.elapsed()
        );
    }

    /// The 600 s defect, proven on the real delivery path: after the
    /// only return address accumulates enough failures for the
    /// quarantine, the next delivery **has** to keep being attempted. With
    /// the candidate list emptied, the `for` did not run, the warning went out
    /// with an empty reason and the gateway waited out the frame deadline.
    #[tokio::test]
    async fn a_quarantined_address_keeps_being_tried() {
        use axum::{body::Bytes, routing::post, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            REMOTE_STREAM_RESULT_PATH,
            post(move |_body: Bytes| {
                let hits = hits_task.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "ok")
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}");
        let reply_to = json!({
            "ticket": "quarentena-ticket",
            "node_id": "gateway-quarentena",
            "urls": [url.clone()],
        });
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(600));

        // Put the only address in quarantine from outside, as if three deliveries
        // in a row had blown the cap.
        let route_key = "reply:gateway-quarentena";
        for _ in 0..4 {
            http.remember_route_failure(route_key, &url);
        }

        assert_eq!(
            deliver_stream_batch(&http, &reply_to, json!({"index": 1})).await,
            Delivery::Accepted,
            "with the only address in quarantine the delivery must still be paid"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    /// A delivery that is slow, but ends in success, is not proof that the
    /// route died. The last candidate gets the long cap and success clears
    /// any earlier memory of failures.
    #[tokio::test]
    async fn a_slow_but_live_delivery_is_not_marked_dead() {
        use axum::{body::Bytes, routing::post, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            REMOTE_STREAM_RESULT_PATH,
            post(move |_body: Bytes| {
                let hits = hits_task.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    hits.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "ok")
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let url = format!("http://{addr}");
        let reply_to = json!({
            "ticket": "slow-live-ticket",
            "node_id": "gateway-slow-live",
            "urls": [url.clone()],
        });
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        let route_key = "reply:gateway-slow-live";

        assert_eq!(
            deliver_stream_batch(&http, &reply_to, json!({"index": 1})).await,
            Delivery::Accepted,
            "a slow delivery must still reach the gateway"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(http.route_failure_streak(route_key, &url), 0);
    }

    #[tokio::test]
    async fn direct_reply_prefers_quic_on_the_return_channel() {
        use axum::{body::Bytes, routing::post, Router};
        use tokio::sync::oneshot;

        let identity = crate::identity::Identity::generate();
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let router = Router::new().route(
            RELAY_RESULT_PATH,
            post(|_body: Bytes| async { StatusCode::OK }),
        );
        let (stop_tx, stop_rx) = oneshot::channel();
        let server_config = crate::quic::server_config(&identity).unwrap();
        let server = tokio::spawn(async move {
            crate::quic::serve(port, router, server_config, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
        });

        let http = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        http.set_quic(true);
        let reply_to = json!({
            "ticket": "quic-return-ticket",
            "node_id": identity.node_id.0,
            "urls": [format!("http://127.0.0.1:{port}")],
            "quic_addresses": [format!("127.0.0.1:{port}")],
        });
        assert!(deliver_result(&http, &reply_to, json!({"done": true})).await);

        stop_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_delivered_result_reaches_the_ticket_that_asked_for_it() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register();
        assert_eq!(inbox.outstanding(), 1);

        assert!(inbox.deliver(&ticket.id().to_string(), json!({"token_id": 7})));
        assert_eq!(inbox.outstanding(), 0, "delivering consumes the ticket");

        let got = (&mut ticket.rx).await.expect("result");
        assert_eq!(got["token_id"], 7);
    }

    #[tokio::test]
    async fn a_result_does_not_wait_but_late_transport_receipts_close_the_trace() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_with_expected_transport(2);
        let id = ticket.id().to_string();

        assert!(inbox.deliver(&id, json!({"token_id": 7})));
        let got = (&mut ticket.rx).await.expect("result");
        assert_eq!(got["transport_trace_complete"], false);
        assert!(got["transport_traversals"].as_array().unwrap().is_empty());
        assert!(inbox.record_transport(
            &id,
            1,
            json!({"hop_index": 1, "transport": "https", "round_trip_ms": 23})
        ));
        assert!(inbox.record_transport(
            &id,
            0,
            json!({"hop_index": 0, "transport": "libp2p-tunnel", "round_trip_ms": 11})
        ));
        let snapshot = inbox.trace_snapshot(&id).expect("snapshot updated");
        assert_eq!(snapshot["transport_trace_complete"], true);
        assert_eq!(snapshot["transport_traversals"][0]["hop_index"], 0);
        assert_eq!(snapshot["transport_traversals"][1]["hop_index"], 1);
    }

    #[tokio::test]
    async fn a_result_first_keeps_the_head_ack_for_transport_reconciliation() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_with_expected_transport(1);
        let ticket_id = ticket.id().to_string();
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_on_ack = notify.clone();
        let inbox_for_ack = inbox.clone();
        let ticket_for_ack = ticket_id.clone();
        let ack = async move {
            tokio::task::yield_now().await;
            Ok(crate::relay::SentFrame {
                meta: json!({"accepted": true}),
                tensor: None,
                round_trip_ms: 17,
                bytes_sent: 100,
                bytes_received: 20,
                hidden_state_bytes_sent: 0,
                hidden_state_bytes_received: 0,
                hidden_state_dtype: crate::wire::WireDtype::F16,
                client_f16_encode_ns: 0,
                client_f16_decode_ns: 0,
                client_encode_ms: 1,
                request_body_send_us: None,
                response_headers_ms: 2,
                response_body_ms: 3,
                client_decode_ms: 4,
                connection_state: crate::peer_client::ConnectionState::Unknown,
                tls: false,
                tls_handshake_ms: 0,
                transport_rtt_us: None,
                transport: crate::relay::InferenceTransport::Libp2pTunnel,
                transport_fallback: false,
                transport_fallback_reason: None,
                profiling_spans: Vec::new(),
            })
        };
        let deliver_inbox = inbox.clone();
        let deliver_ticket = ticket_id.clone();
        tokio::spawn(async move {
            deliver_inbox.deliver(&deliver_ticket, json!({"token_id": 7}));
        });

        let got = await_result_with_ack(
            ack,
            &mut ticket,
            "head",
            Duration::from_secs(1),
            move |sent| {
                assert_eq!(sent.round_trip_ms, 17);
                assert!(inbox_for_ack.record_transport(
                    &ticket_for_ack,
                    0,
                    transport_receipt(0, sent),
                ));
                notify_on_ack.notify_one();
            },
        )
        .await
        .expect("direct result");
        assert_eq!(got["token_id"], 7);
        assert_eq!(got["transport_trace_complete"], false);

        tokio::time::timeout(Duration::from_secs(1), notify.notified())
            .await
            .expect("late ACK processed");
        let snapshot = inbox.trace_snapshot(&ticket_id).expect("snapshot updated");
        assert_eq!(snapshot["transport_trace_complete"], true);
        assert_eq!(snapshot["transport_traversals"][0]["round_trip_ms"], 17);
    }

    #[test]
    fn duplicate_transport_receipts_are_not_counted_twice() {
        let inbox = ReplyInbox::new();
        let ticket = inbox.register_with_expected_transport(1);
        let id = ticket.id().to_string();
        let receipt = json!({"hop_index": 0, "transport": "https", "round_trip_ms": 23});

        assert!(inbox.record_transport(&id, 0, receipt.clone()));
        assert!(!inbox.record_transport(&id, 0, receipt));
    }

    #[tokio::test]
    async fn a_result_includes_transport_receipts_that_arrived_first() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_with_expected_transport(2);
        let id = ticket.id().to_string();

        assert!(inbox.record_transport(
            &id,
            1,
            json!({"hop_index": 1, "transport": "https", "round_trip_ms": 23})
        ));
        assert!(inbox.record_transport(
            &id,
            0,
            json!({"hop_index": 0, "transport": "libp2p-tunnel", "round_trip_ms": 11})
        ));
        assert!(inbox.deliver(&id, json!({"token_id": 7})));

        let got = (&mut ticket.rx).await.expect("result");
        assert_eq!(got["transport_trace_complete"], true);
        assert_eq!(got["transport_traversals"][0]["hop_index"], 0);
        assert_eq!(got["transport_traversals"][1]["hop_index"], 1);
    }

    #[tokio::test]
    async fn a_remote_stream_is_bounded_and_applies_backpressure() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_stream();
        let id = ticket.id().to_string();
        for index in 0..REMOTE_STREAM_QUEUE_CAPACITY {
            assert!(inbox.deliver_stream(&id, json!({"index": index})).await);
        }
        assert_eq!(inbox.stream_outstanding(), 1);

        let mut pending = Box::pin(inbox.deliver_stream(&id, json!({"index": 99})));
        tokio::select! {
            result = &mut pending => panic!("the full queue cannot accept another batch: {result}"),
            got = ticket.recv() => assert_eq!(got.expect("first batch")["index"], 0),
        }
        assert!(pending.await);
    }

    #[tokio::test]
    async fn a_terminal_remote_stream_batch_removes_the_ticket_after_delivery() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_stream();
        let id = ticket.id().to_string();

        assert!(inbox.deliver_stream(&id, json!({"done": true})).await);
        assert_eq!(inbox.stream_outstanding(), 0);
        assert_eq!(ticket.recv().await.expect("terminal batch")["done"], true);
    }

    #[tokio::test]
    async fn a_cancelled_remote_stream_refuses_late_batches() {
        let inbox = ReplyInbox::new();
        let id = {
            let ticket = inbox.register_stream();
            ticket.id().to_string()
        };

        assert!(!inbox.deliver_stream(&id, json!({"accepted": []})).await);
        assert_eq!(inbox.stream_outstanding(), 0);
    }

    #[tokio::test]
    async fn the_remote_stream_callback_delivers_the_wire_batch_to_its_ticket() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_stream();
        let body = crate::wire::encode(
            &json!({
                "ticket": ticket.id(),
                "accepted": [{"token_id": 4, "text": "ok"}],
                "done": true,
            }),
            None,
            crate::wire::WireDtype::F16,
        )
        .expect("encodable batch");

        let response = handle_remote_stream_result(&inbox, Bytes::from(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let received = ticket.recv().await.expect("batch received");
        assert_eq!(received["accepted"][0]["token_id"], 4);
        assert_eq!(
            received["stream_batch_bytes"].as_u64().unwrap_or(0) > 0,
            true
        );
    }

    #[tokio::test]
    async fn the_persistent_remote_stream_frame_reaches_its_ticket() {
        let inbox = ReplyInbox::new();
        let mut ticket = inbox.register_stream();
        let body = crate::wire::encode_batch_items(
            REMOTE_STREAM_RESULT_PATH,
            "stream-1:0",
            &[crate::wire::BatchPayloadItem {
                item_id: "stream-1:0".into(),
                session_id: "session-1".into(),
                meta: json!({
                    "ticket": ticket.id(),
                    "accepted": [{"token_id": 4, "text": "ok", "token_index": 0}],
                    "done": true,
                }),
                tensor: None,
                dtype: crate::wire::WireDtype::F16,
            }],
        )
        .expect("encodable persistent frame");

        let response = handle_remote_stream_link(&inbox, Bytes::from(body)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let received = ticket.recv().await.expect("frame received");
        assert_eq!(received["accepted"][0]["token_id"], 4);
        assert_eq!(
            received["stream_batch_bytes"].as_u64().unwrap_or(0) > 0,
            true
        );
    }

    /// The guard has to clean the inbox by itself: the error path and the expired
    /// deadline path leave by `return`, and a forgotten ticket holds memory for
    /// every lost request.
    #[test]
    fn a_dropped_ticket_leaves_no_entry_behind() {
        let inbox = ReplyInbox::new();
        {
            let _ticket = inbox.register();
            assert_eq!(inbox.outstanding(), 1);
        }
        assert_eq!(inbox.outstanding(), 0);
    }

    /// A hop that answers after the gateway has given up must not be
    /// confused with the next frame, nor bring anything down.
    #[test]
    fn a_late_result_is_refused_instead_of_landing_on_another_frame() {
        let inbox = ReplyInbox::new();
        let delayed = {
            let ticket = inbox.register();
            ticket.id().to_string()
        };
        let _next = inbox.register();

        assert!(!inbox.deliver(&delayed, json!({"token_id": 1})));
        assert_eq!(inbox.outstanding(), 1, "the next ticket stays intact");
    }

    #[test]
    fn a_reply_url_never_points_at_the_wildcard_address() {
        let urls = self_reply_urls(
            None,
            vec![
                "0.0.0.0:42777".into(),
                "192.168.1.9:42777".into(),
                "[2804:f30::1]:42777".into(),
                "192.168.1.9:42777".into(),
            ],
            8080,
        );
        assert_eq!(
            urls,
            vec![
                "http://192.168.1.9:8080".to_string(),
                "http://[2804:f30::1]:8080".to_string(),
            ]
        );
    }

    /// Loopback is the address of the machine of whoever answers, not of this one. A list
    /// that only has loopback must end up empty so `DirectReply::new` refuses
    /// direct mode and the plan falls back to the ring, which works: turned on, the frame
    /// would go out never to return and the request would only end at the 600 s deadline.
    #[test]
    fn a_loopback_only_host_list_falls_back_to_the_ring() {
        let urls = self_reply_urls(
            None,
            vec![
                "127.0.0.1:42777".into(),
                "127.0.0.53:42777".into(),
                "[::1]:42777".into(),
                "localhost:42777".into(),
            ],
            8080,
        );
        assert!(
            urls.is_empty(),
            "loopback does not reach another machine: {urls:?}"
        );
        assert!(
            DirectReply::new(ReplyInbox::new(), urls, "n1".into()).is_none(),
            "without a usable address the plan must fall to the ring"
        );

        // The loopback the operator pinned is another story: on a single machine,
        // pointing at itself is the right configuration and was their choice.
        assert_eq!(
            self_reply_urls(
                Some("127.0.0.1:42777".into()),
                vec!["127.0.0.1:42777".into()],
                8080
            ),
            vec!["http://127.0.0.1:8080".to_string()]
        );
    }

    /// The `reply_to` has to carry the gateway's libp2p identity: without it the
    /// tail behind NAT has no way to return the frame, and the gateway closes
    /// at the deadline without saying why.
    #[test]
    fn the_reply_to_carries_the_gateway_libp2p_identity() {
        let reply = DirectReply::new(
            ReplyInbox::new(),
            vec!["http://192.168.100.210:8080".to_string()],
            "n1".into(),
        )
        .expect("address present")
        .with_libp2p(
            Some("12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN".to_string()),
            vec!["/ip4/203.0.113.7/udp/52777/quic-v1".to_string()],
        );

        let value = reply.reply_to("t1");

        assert_eq!(
            value["libp2p_peer_id"].as_str(),
            Some("12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN")
        );
        assert_eq!(
            value["libp2p_addrs"][0].as_str(),
            Some("/ip4/203.0.113.7/udp/52777/quic-v1")
        );
    }

    /// A gateway on a prior binary does not send the fields, and the delivery has to
    /// proceed without them instead of breaking.
    #[tokio::test]
    async fn without_a_libp2p_identity_the_tunnel_delivery_is_not_attempted() {
        let reply_to = json!({ "ticket": "t1", "urls": [] });

        assert!(
            deliver_via_tunnel(&reply_to, RELAY_RESULT_PATH, &json!({}), "no route")
                .await
                .is_none()
        );
    }

    #[test]
    fn a_direct_transport_receipt_updates_only_its_destination_hop() {
        let mut hops = vec![
            hop_entry(&json!(10), &json!({}), 1_000),
            hop_entry(&json!(20), &json!({}), 2_000),
        ];
        let sent = crate::relay::SentFrame {
            meta: json!({ "processing_ms": 20, "node_timings": {} }),
            tensor: None,
            round_trip_ms: 47,
            bytes_sent: 2_000,
            bytes_received: 123,
            hidden_state_bytes_sent: 0,
            hidden_state_bytes_received: 0,
            hidden_state_dtype: crate::wire::WireDtype::F16,
            client_f16_encode_ns: 0,
            client_f16_decode_ns: 0,
            client_encode_ms: 3,
            request_body_send_us: None,
            response_headers_ms: 4,
            response_body_ms: 5,
            client_decode_ms: 6,
            connection_state: crate::peer_client::ConnectionState::Unknown,
            tls: false,
            tls_handshake_ms: 0,
            transport_rtt_us: None,
            transport: crate::relay::InferenceTransport::Libp2pTunnel,
            transport_fallback: false,
            transport_fallback_reason: None,
            profiling_spans: Vec::new(),
        };

        attach_transport(&mut hops, 1, &sent);

        assert_eq!(hops[0]["round_trip_ms"], 10);
        assert_eq!(hops[1]["round_trip_ms"], 47);
        assert_eq!(hops[1]["bytes_sent"], 2_000);
        assert_eq!(hops[1]["bytes_received"], 123);
        assert_eq!(hops[1]["transport"], "libp2p-tunnel");
    }
}
