//! Persistent link between two stages of the pipeline (`transport/link-v1`).
//!
//! The commit of remote batches removed the per-token traversal from the
//! **gateway → head** stretch: a control frame goes once and the tokens come back in
//! batches by callback. The head's loop also uses the same channel for each
//! activation of the next hop when the capability is present - `POST /v1/step`
//! remains the logical route, but the HTTP request and the framing live
//! in one persistent stream per edge.
//!
//! The transformer's causal dependency forbids getting ahead of a session's next
//! token, but it does not force the call to be rebuilt. Here the pipeline edge
//! gets **one channel**: a single `POST /v1/link` whose outbound body is a stream
//! of binary frames and whose return body is another stream of binary frames. The
//! network underneath is the same as always - the client pinned per peer, the same
//! credential, the same pooled TCP/TLS connection. What ceases to exist is the
//! request abstraction rebuilt per activation.
//!
//! ## Framing
//!
//! Each frame, in both directions:
//!
//! ```text
//! [4B BE size of the rest][2B BE status][2B BE size of the id][id][body]
//! ```
//!
//! * `status` is `0` on the way out and the HTTP code on the way back. A refusal
//!   from the peer arrives this way, with the same meaning it would have on the
//!   per-request path, and becomes [`crate::relay::HttpRefusal`] in the caller.
//! * `id` is the `batch_id` of the `frame/v1-batch` frame, which is the identity
//!   that [`crate::relay::BatchCoordinator`] already uses. It is what correlates
//!   outbound and return, and it is what allows **several sessions on the same
//!   channel**: frames do not come back in order, they come back identified.
//! * `body` is the whole `frame/v1-batch`. The route goes in the frame's own
//!   `kind`, so there is no duplicated path in the envelope.
//!
//! ## Degradation
//!
//! A transport failure on the channel **does not kill the generation**: the call
//! goes back through the usual path (libp2p tunnel, then HTTP or HTTPS per
//! request) with `transport_fallback=true`, and the dead channel leaves the pool.
//! A destination that refused to open is quarantined for
//! [`OPEN_FAILURE_COOLDOWN`], so that the attempt to open is not paid per token
//! in a cluster where the direct address does not serve.
//!
//! ## Cleanup
//!
//! Every pending delivery lives in a per-channel registry and leaves it on the
//! guard's `Drop` - on any exit path: reply, error, deadline, cancellation
//! by the caller. When the channel dies, the reader drains the whole registry
//! with an error before disappearing, and no caller is left hanging.

use crate::wire;
use anyhow::{anyhow, Result};
use axum::body::Bytes;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// Route of the persistent channel. It sits on the inference port, under the same
/// credential as the other `/v1` routes.
pub const LINK_PATH: &str = "/v1/link";

/// Fixed header of the envelope: status and id size.
const ENVELOPE_HEAD: usize = 4;

/// Ceiling of a channel frame. It leaves room over the maximum inference body for
/// the envelope; a frame larger than this is a protocol error and brings the channel down.
const MAX_LINK_FRAME_BYTES: usize = wire::MAX_FRAME_PAYLOAD_BYTES + 4096;

/// How many outbound frames fit in flight before the sender waits. It is the
/// channel's backpressure: a slow hop holds back whoever feeds it instead of
/// letting memory grow.
const OUTBOUND_QUEUE: usize = 64;

// The deadline of a call on the channel is that of the current frame
// (`relay::frame_time_left`), the same as all of the attempt's transports.

/// How long a destination goes without a new attempt to open a channel after
/// an open failure. It must follow the route memory: they are the same
/// decision of not paying again for a URL that failed.
const OPEN_FAILURE_COOLDOWN: Duration = crate::peer_client::ROUTE_FAILURE_COOLDOWN;

/// It only applies to receiving the opening headers. After the channel opens,
/// the body stays alive until `CHANNEL_LIFETIME`, without turning a long
/// generation into a prematurely interrupted connection.
#[cfg(not(test))]
const LINK_OPEN_TIMEOUT: Duration = Duration::from_millis(400);

#[cfg(test)]
const LINK_OPEN_TIMEOUT: Duration = Duration::from_millis(50);

/// Delivery boundary of the persistent channel. Failures on opening happen
/// before any inference frame; after the frame is enqueued, the
/// reply may be lost even though the peer has already processed the request.
#[derive(Debug)]
pub(crate) struct LinkDeliveryError {
    pub before_frame: bool,
    message: String,
}

impl LinkDeliveryError {
    fn before_frame(message: impl Into<String>) -> Self {
        Self {
            before_frame: true,
            message: message.into(),
        }
    }

    fn after_frame(error: impl std::fmt::Display) -> Self {
        Self {
            before_frame: false,
            message: error.to_string(),
        }
    }
}

impl std::fmt::Display for LinkDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LinkDeliveryError {}

/// Lifetime ceiling of a channel. The peer's HTTP client is built with a total
/// timeout, and the streamed body counts toward it: without an explicit ceiling the channel
/// would die at the common request timeout. When this deadline expires, the channel
/// drops, the in-flight call degrades to the per-request path and the next
/// channel is opened on the next call.
const CHANNEL_LIFETIME: Duration = Duration::from_secs(12 * 3600);

/// What a traversal through the channel returns. It mirrors what the per-request
/// path measures, minus what does not exist here: there are no per-token
/// response headers to time.
#[derive(Debug)]
pub struct LinkResponse {
    pub body: Vec<u8>,
    pub round_trip_ms: u64,
    pub bytes_sent: u64,
    /// Whether the concrete route that opened/reused this channel is TLS.
    /// The caller must use this value instead of the planned primary URL:
    /// candidates can change which route actually carried the frame.
    pub tls: bool,
    pub profiling_spans: Vec<crate::prefill::TelemetrySpan>,
}

// ── Envelope ─────────────────────────────────────────────────────────────────

/// Builds `[size][status][id][body]`.
pub fn encode_envelope(status: u16, id: &str, body: &[u8]) -> Result<Vec<u8>> {
    if id.len() > u16::MAX as usize {
        return Err(anyhow!("frame identifier too long"));
    }
    let rest = ENVELOPE_HEAD
        .checked_add(id.len())
        .and_then(|len| len.checked_add(body.len()))
        .ok_or_else(|| anyhow!("channel frame exceeds the numeric limit"))?;
    if rest > MAX_LINK_FRAME_BYTES {
        return Err(anyhow!(
            "channel frame exceeds the limit of {MAX_LINK_FRAME_BYTES} bytes"
        ));
    }
    let mut out = Vec::with_capacity(4 + rest);
    out.extend_from_slice(&(rest as u32).to_be_bytes());
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(&(id.len() as u16).to_be_bytes());
    out.extend_from_slice(id.as_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// A frame already separated from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub status: u16,
    pub id: String,
    pub body: Vec<u8>,
}

/// Accumulates bytes from a stream and returns whole frames as they complete.
///
/// Kept apart from the transport on purpose: it is the only part of the channel
/// with offset arithmetic, and the one that needs its own test against
/// arbitrary fragmentation.
#[derive(Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// Next whole frame, if it has already arrived. `Ok(None)` is "not yet";
    /// `Err` is a broken protocol and the channel does not continue.
    pub fn next(&mut self) -> Result<Option<Envelope>> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let rest = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;
        if rest > MAX_LINK_FRAME_BYTES {
            return Err(anyhow!(
                "channel frame announces {rest} bytes, above the limit of {MAX_LINK_FRAME_BYTES}"
            ));
        }
        if rest < ENVELOPE_HEAD {
            return Err(anyhow!("channel frame too short: {rest} bytes"));
        }
        let total = 4 + rest;
        if self.buffer.len() < total {
            return Ok(None);
        }
        let status = u16::from_be_bytes([self.buffer[4], self.buffer[5]]);
        let id_len = u16::from_be_bytes([self.buffer[6], self.buffer[7]]) as usize;
        let id_end = 8 + id_len;
        if id_end > total {
            return Err(anyhow!("frame identifier exceeds the frame"));
        }
        let id = String::from_utf8(self.buffer[8..id_end].to_vec())
            .map_err(|_| anyhow!("frame identifier is not UTF-8"))?;
        let body = self.buffer[id_end..total].to_vec();
        self.buffer.drain(..total);
        Ok(Some(Envelope { status, id, body }))
    }
}

// ── Lado servidor ────────────────────────────────────────────────────────────

/// Serves a channel: reads frames from the request body, dispatches each one by
/// the route the frame itself names (`kind`) and returns the replies through the
/// response body, identified by `batch_id`.
///
/// Inference frames are dispatched in their own tasks: a slow item of
/// one session does not block another's frame. `remote-stream` callbacks are
/// forwarded through a bounded serial queue, since their order is the order of
/// the tokens observed by the gateway; this preserves causality without turning
/// the ACK into a wait on the producer.
pub fn serve<D, F>(body: axum::body::Body, dispatch: D) -> axum::response::Response
where
    D: Fn(String, Bytes) -> F + Send + Sync + 'static,
    F: std::future::Future<Output = Result<axum::response::Response>> + Send + 'static,
{
    let dispatch = Arc::new(dispatch);
    let (out_tx, out_rx) = futures::channel::mpsc::channel::<Bytes>(OUTBOUND_QUEUE);
    let (callback_tx, mut callback_rx) =
        futures::channel::mpsc::channel::<Envelope>(OUTBOUND_QUEUE);
    let callback_dispatch = dispatch.clone();
    let mut callback_out_tx = out_tx.clone();
    tokio::spawn(async move {
        while let Some(envelope) = callback_rx.next().await {
            let reply = serve_one(callback_dispatch.as_ref(), &envelope).await;
            if callback_out_tx.send(Bytes::from(reply)).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut stream = body.into_data_stream();
        let mut reader = FrameReader::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    debug!("inference channel stopped reading: {error}");
                    break;
                }
            };
            reader.push(&chunk);
            loop {
                match reader.next() {
                    Ok(Some(envelope)) => {
                        if is_remote_stream_callback(&envelope.body) {
                            if callback_tx.clone().send(envelope).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        let dispatch = dispatch.clone();
                        let mut out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            let reply = serve_one(dispatch.as_ref(), &envelope).await;
                            let _ = out_tx.send(Bytes::from(reply)).await;
                        });
                    }
                    Ok(None) => break,
                    Err(error) => {
                        warn!("inference channel received an invalid frame: {error}");
                        return;
                    }
                }
            }
        }
    });

    let stream = out_rx.map(Ok::<_, std::io::Error>);
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("content-type", "application/octet-stream")
        .body(axum::body::Body::from_stream(stream))
        .expect("channel response is always buildable")
}

fn is_remote_stream_callback(body: &[u8]) -> bool {
    wire::peek_batch_header(body)
        .is_ok_and(|header| header.kind == crate::direct::REMOTE_STREAM_RESULT_PATH)
}

async fn serve_one<D, F>(dispatch: &D, envelope: &Envelope) -> Vec<u8>
where
    D: Fn(String, Bytes) -> F,
    F: std::future::Future<Output = Result<axum::response::Response>>,
{
    let path = match wire::peek_batch_header(&envelope.body) {
        Ok(header) => header.kind,
        Err(error) => return refusal(&envelope.id, 400, format!("invalid frame: {error}")),
    };
    let response = match dispatch(path, Bytes::from(envelope.body.clone())).await {
        Ok(response) => response,
        Err(error) => return refusal(&envelope.id, 400, error.to_string()),
    };
    let status = response.status().as_u16();
    let bytes =
        match axum::body::to_bytes(response.into_body(), wire::MAX_FRAME_PAYLOAD_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => return refusal(&envelope.id, 500, format!("unreadable response: {error}")),
        };
    match encode_envelope(status, &envelope.id, &bytes) {
        Ok(frame) => frame,
        Err(error) => refusal(&envelope.id, 500, error.to_string()),
    }
}

fn refusal(id: &str, status: u16, message: String) -> Vec<u8> {
    encode_envelope(status, id, message.as_bytes()).unwrap_or_else(|_| {
        // The identifier is the only free-size field, and it came from the wire
        // already bounded. If it still does not fit, the empty frame keeps the channel
        // consistent instead of silencing the call.
        encode_envelope(status, "", b"").expect("an empty envelope always fits")
    })
}

// ── Lado cliente ─────────────────────────────────────────────────────────────

struct Pending {
    reply: oneshot::Sender<std::result::Result<Envelope, String>>,
}

struct Channel {
    outbound: futures::channel::mpsc::Sender<Bytes>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    alive: Arc<AtomicBool>,
}

/// Asynchronous result of a callback's ACK. Sending stays off the critical
/// path, but the remote producer needs to know when the gateway
/// answered `410 Gone` so as not to keep generating in an abandoned session.
#[derive(Clone)]
pub struct CallbackStatus {
    gateway_gone: Arc<AtomicBool>,
    transport_failed: Arc<AtomicBool>,
}

impl CallbackStatus {
    fn new() -> Self {
        Self {
            gateway_gone: Arc::new(AtomicBool::new(false)),
            transport_failed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn gateway_gone(&self) -> bool {
        self.gateway_gone.load(Ordering::Acquire)
    }

    pub(crate) fn transport_failed(&self) -> bool {
        self.transport_failed.load(Ordering::Acquire)
    }
}

/// Removes the pending delivery on **any** exit path: reply,
/// error, deadline, or the caller giving up. Without this a cancellation would leave the
/// registry growing with tickets that nobody waits for any more.
struct PendingGuard {
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&self.id);
        }
    }
}

#[derive(Default)]
struct Pool {
    channels: tokio::sync::Mutex<HashMap<String, Arc<Channel>>>,
    /// Guards only the opening of an edge. The network is never awaited while
    /// the channel map is held; different keys may open in parallel.
    opening: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    blocked: Mutex<HashMap<String, Instant>>,
    /// Openings per destination, not a process-wide total: the bench needs to
    /// prove that **that** edge opened just one connection, and a global counter
    /// would count the neighboring tests' edges.
    opened: Mutex<HashMap<String, u64>>,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(Pool::default)
}

fn opening_lock(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let Ok(mut opening) = pool().opening.lock() else {
        return Arc::new(tokio::sync::Mutex::new(()));
    };
    opening
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn target_key(node_id: &str, base_url: &str) -> String {
    format!("{node_id}@{}", base_url.trim_end_matches('/'))
}

/// How many channels this edge opened. It exists for the bench to prove that the
/// link is **one** for many tokens, and not one per token. In production,
/// what says which way the traversal went is the trace's `transport`.
#[cfg(test)]
pub fn channels_opened(node_id: &str, base_url: &str) -> u64 {
    pool()
        .opened
        .lock()
        .ok()
        .and_then(|opened| opened.get(&target_key(node_id, base_url)).copied())
        .unwrap_or(0)
}

/// Whether this edge has a live channel in the pool.
#[cfg(test)]
pub async fn has_channel(node_id: &str, base_url: &str) -> bool {
    pool()
        .channels
        .lock()
        .await
        .contains_key(&target_key(node_id, base_url))
}

fn quarantined(key: &str) -> bool {
    let Ok(mut blocked) = pool().blocked.lock() else {
        return false;
    };
    match blocked.get(key) {
        Some(until) if *until > Instant::now() => true,
        Some(_) => {
            blocked.remove(key);
            false
        }
        None => false,
    }
}

fn quarantine(key: &str) {
    if let Ok(mut blocked) = pool().blocked.lock() {
        blocked.insert(key.to_string(), Instant::now() + OPEN_FAILURE_COOLDOWN);
    }
}

async fn forget(key: &str, channel: &Arc<Channel>) {
    let mut channels = pool().channels.lock().await;
    if channels
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, channel))
    {
        channels.remove(key);
    }
}

async fn channel_for(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    session_id: Option<&str>,
) -> Result<(Arc<Channel>, bool)> {
    let key = target_key(node_id, base_url);
    {
        let channels = pool().channels.lock().await;
        if let Some(channel) = channels.get(&key) {
            if channel.alive.load(Ordering::Relaxed) {
                return Ok((channel.clone(), false));
            }
        }
    }
    if quarantined(&key) {
        return Err(anyhow!("channel with {node_id} quarantined after a failure"));
    }

    // The guard is per edge. Two traversals of the same edge share the
    // opening, but a slow attempt to another destination does not stop this
    // whole process.
    let opening = opening_lock(&key);
    let _opening = opening.lock().await;
    {
        let channels = pool().channels.lock().await;
        if let Some(channel) = channels.get(&key) {
            if channel.alive.load(Ordering::Relaxed) {
                return Ok((channel.clone(), false));
            }
        }
    }

    if quarantined(&key) {
        return Err(anyhow!("channel with {node_id} quarantined after a failure"));
    }

    match open(clients, node_id, base_url, session_id).await {
        Ok(channel) => {
            if let Ok(mut opened) = pool().opened.lock() {
                *opened.entry(key.clone()).or_default() += 1;
            }
            pool().channels.lock().await.insert(key, channel.clone());
            Ok((channel, true))
        }
        Err(error) => {
            quarantine(&key);
            Err(error)
        }
    }
}

async fn open(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    session_id: Option<&str>,
) -> Result<Arc<Channel>> {
    let http = clients.for_peer(node_id)?;
    let url = format!("{}{}", base_url.trim_end_matches('/'), LINK_PATH);
    let (outbound, rx) = futures::channel::mpsc::channel::<Bytes>(OUTBOUND_QUEUE);
    let request = http
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        // The client's timeout applies to the whole request, and ours lasts
        // the whole generation. See `CHANNEL_LIFETIME`.
        .timeout(CHANNEL_LIFETIME)
        .body(reqwest::Body::wrap_stream(rx.map(Ok::<_, std::io::Error>)));
    let response = clients
        .authorize_inference_request_for_session(request, session_id)
        .await
        .send();
    let response = tokio::time::timeout(LINK_OPEN_TIMEOUT, response)
        .await
        .map_err(|_| anyhow!("channel with {node_id} did not open before the connection timeout"))?
        .map_err(|error| anyhow!("channel with {node_id} did not open: {error}"))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "channel with {node_id} refused with HTTP {}",
            response.status()
        ));
    }

    let pending: Arc<Mutex<HashMap<String, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
    let alive = Arc::new(AtomicBool::new(true));
    let channel = Arc::new(Channel {
        outbound,
        pending: pending.clone(),
        alive: alive.clone(),
    });

    let key = target_key(node_id, base_url);
    let node = node_id.to_string();
    let reader_channel = channel.clone();
    tokio::spawn(async move {
        let mut stream = response.bytes_stream();
        let mut reader = FrameReader::new();
        let mut cause = String::from("channel closed by the peer");
        'outer: while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    cause = format!("channel read failed: {error}");
                    break;
                }
            };
            reader.push(&chunk);
            loop {
                match reader.next() {
                    Ok(Some(envelope)) => {
                        let waiting = pending
                            .lock()
                            .ok()
                            .and_then(|mut map| map.remove(&envelope.id));
                        if let Some(waiting) = waiting {
                            let _ = waiting.reply.send(Ok(envelope));
                        } else {
                            debug!(
                                peer = %node,
                                id = %envelope.id,
                                "channel response arrived after giving up"
                            );
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        cause = format!("invalid frame on the channel: {error}");
                        break 'outer;
                    }
                }
            }
        }
        alive.store(false, Ordering::Relaxed);
        forget(&key, &reader_channel).await;
        let waiting: Vec<Pending> = pending
            .lock()
            .map(|mut map| map.drain().map(|(_, value)| value).collect())
            .unwrap_or_default();
        for entry in waiting {
            let _ = entry.reply.send(Err(cause.clone()));
        }
    });

    debug!(peer = node_id, url = %url, "inference channel open");
    Ok(channel)
}

/// Sends a whole `frame/v1-batch` frame through the persistent channel and waits for the
/// reply correlated by `batch_id`.
///
/// Returns an error on any transport failure - it is the caller who decides to
/// degrade to the per-request path. An HTTP refusal from the peer comes back as
/// [`crate::relay::HttpRefusal`], the same as on the per-request path, because it
/// is the peer's considered answer and switching transport does not change it.
#[cfg(test)]
pub async fn send_batch(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
) -> Result<LinkResponse> {
    send_batch_with_candidates(clients, node_id, base_url, &[], batch_id, body, peer_label).await
}

/// Opens the persistent channel through the first healthy advertised route. The opening
/// is the only phase in which the address is not yet memorized as healthy;
/// a connection failure is recorded and the next alternative takes over already in this
/// call. After a channel opens, `send_batch` keeps its semantics of not
/// repeating an already sent frame on another route.
#[allow(dead_code)]
pub async fn send_batch_with_candidates(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
) -> Result<LinkResponse> {
    send_batch_with_session(
        clients, node_id, base_url, candidates, batch_id, body, peer_label, None,
    )
    .await
}

pub async fn send_batch_with_session(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    session_id: Option<&str>,
) -> Result<LinkResponse> {
    send_batch_with_session_profile(
        clients, node_id, base_url, candidates, batch_id, body, peer_label, session_id, None,
    )
    .await
}

/// Variant used by the detailed trace. Normal mode passes `None` and does not
/// allocate spans or do additional work on the hot path.
pub async fn send_batch_with_session_profile(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    session_id: Option<&str>,
    profiling: Option<crate::prefill::TelemetryContext>,
) -> Result<LinkResponse> {
    let mut route_candidates = Vec::with_capacity(candidates.len() + 1);
    if !base_url.is_empty() {
        route_candidates.push(base_url.to_string());
    }
    let additional: Vec<String> = candidates
        .iter()
        .filter(|candidate| !route_candidates.contains(candidate))
        .cloned()
        .collect();
    route_candidates.extend(additional);
    let route_key = format!("inference:{node_id}");
    let ordered = clients.route_order(&route_key, &route_candidates);
    let mut last_error = String::new();
    // A dead first URL must not consume the entire fallback window and hide
    // a healthy advertised address. Bound the route ladder to two short
    // opens, independently from the lifetime of the inference request.
    let open_deadline = Instant::now() + LINK_OPEN_TIMEOUT.saturating_mul(2);
    for base in ordered {
        let Some(open_budget) = open_deadline.checked_duration_since(Instant::now()) else {
            last_error = "channel open budget exhausted".to_string();
            break;
        };
        let setup_started = Instant::now();
        let (channel, opened) = match tokio::time::timeout(
            open_budget,
            channel_for(clients, node_id, &base, session_id),
        )
        .await
        {
            Ok(Ok(channel)) => channel,
            Ok(Err(error)) => {
                last_error = error.to_string();
                clients.remember_route_failure(&route_key, &base);
                continue;
            }
            Err(_) => {
                last_error = format!(
                    "channel did not open within the budget of {} ms",
                    LINK_OPEN_TIMEOUT.as_millis()
                );
                quarantine(&target_key(node_id, &base));
                clients.remember_route_failure(&route_key, &base);
                continue;
            }
        };
        match send_batch_on_channel(
            &channel,
            node_id,
            batch_id,
            body.clone(),
            peer_label,
            profiling.as_ref(),
        )
        .await
        {
            Ok(mut response) => {
                set_effective_link_transport(&mut response, &base);
                if opened {
                    if let Some(context) = profiling.as_ref() {
                        response.profiling_spans.push(context.span(
                            "https-link-setup",
                            setup_started,
                            Instant::now(),
                            None,
                            Some(effective_link_transport(&base).into()),
                        ));
                    }
                }
                clients.remember_route_success(&route_key, &base);
                return Ok(response);
            }
            Err(error) if error.downcast_ref::<crate::relay::HttpRefusal>().is_some() => {
                return Err(error)
            }
            Err(error) => {
                clients.remember_route_failure(&route_key, &base);
                return Err(anyhow::Error::new(LinkDeliveryError::after_frame(error)));
            }
        }
    }
    Err(anyhow::Error::new(LinkDeliveryError::before_frame(
        format!("channel with {node_id} did not open within the budget: {last_error}"),
    )))
}

/// ACK HTTP variant exercised by the persistent channel's tests. The
/// `remote-stream` production path uses the variant that does not wait for the ACK.
#[cfg(test)]
async fn send_body_with_session(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    frame_id: &str,
    body: Bytes,
    peer_label: &str,
    session_id: Option<&str>,
) -> Result<LinkResponse> {
    let mut route_candidates = Vec::with_capacity(candidates.len() + 1);
    if !base_url.is_empty() {
        route_candidates.push(base_url.to_string());
    }
    let additional: Vec<String> = candidates
        .iter()
        .filter(|candidate| !route_candidates.contains(candidate))
        .cloned()
        .collect();
    route_candidates.extend(additional);
    let route_key = format!("reply:{node_id}");
    let mut last_error = String::new();
    for base in clients.route_order(&route_key, &route_candidates) {
        let (channel, _opened) = match channel_for(clients, node_id, &base, session_id).await {
            Ok(channel) => channel,
            Err(error) => {
                last_error = error.to_string();
                clients.remember_route_failure(&route_key, &base);
                continue;
            }
        };
        match send_frame_on_channel(
            &channel,
            node_id,
            frame_id,
            body.clone(),
            peer_label,
            false,
            None,
        )
        .await
        {
            Ok(response) => {
                let mut response = response;
                set_effective_link_transport(&mut response, &base);
                clients.remember_route_success(&route_key, &base);
                return Ok(response);
            }
            Err(error) => {
                clients.remember_route_failure(&route_key, &base);
                return Err(error);
            }
        }
    }
    Err(anyhow!(
        "return channel with {node_id} did not open through any route: {last_error}"
    ))
}

fn effective_link_transport(base_url: &str) -> &'static str {
    if base_url.starts_with("https://") {
        "https-link"
    } else {
        "http-link"
    }
}

fn set_effective_link_transport(response: &mut LinkResponse, base_url: &str) {
    response.tls = base_url.starts_with("https://");
    let transport = effective_link_transport(base_url).to_owned();
    for span in &mut response.profiling_spans {
        span.transport = Some(transport.clone());
    }
}

/// Enqueues a callback frame on the persistent channel and does not wait for the
/// corresponding HTTP ACK. The channel itself stays full-duplex: the reader task
/// drains the replies and `pending` tracks asynchronous failures. The outbound
/// queue limited to [`OUTBOUND_QUEUE`] keeps local backpressure without imposing
/// a round-trip per token on the remote producer.
pub async fn send_body_without_response_with_session(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base_url: &str,
    candidates: &[String],
    frame_id: &str,
    body: Bytes,
    peer_label: &str,
    session_id: Option<&str>,
) -> Result<CallbackStatus> {
    let mut route_candidates = Vec::with_capacity(candidates.len() + 1);
    if !base_url.is_empty() {
        route_candidates.push(base_url.to_string());
    }
    let additional: Vec<String> = candidates
        .iter()
        .filter(|candidate| !route_candidates.contains(candidate))
        .cloned()
        .collect();
    route_candidates.extend(additional);
    let route_key = format!("reply:{node_id}");
    let mut last_error = String::new();
    for base in clients.route_order(&route_key, &route_candidates) {
        let (channel, _opened) = match channel_for(clients, node_id, &base, session_id).await {
            Ok(channel) => channel,
            Err(error) => {
                last_error = error.to_string();
                clients.remember_route_failure(&route_key, &base);
                continue;
            }
        };
        let channel_key = target_key(node_id, &base);
        match send_frame_without_response(
            &channel,
            node_id,
            frame_id,
            body.clone(),
            peer_label,
            channel_key,
        )
        .await
        {
            Ok(status) => {
                clients.remember_route_success(&route_key, &base);
                return Ok(status);
            }
            Err(error) => {
                last_error = error.to_string();
                clients.remember_route_failure(&route_key, &base);
            }
        }
    }
    Err(anyhow!(
        "return channel with {node_id} did not enqueue through any route: {last_error}"
    ))
}

async fn send_batch_on_channel(
    channel: &Arc<Channel>,
    node_id: &str,
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    profiling: Option<&crate::prefill::TelemetryContext>,
) -> Result<LinkResponse> {
    send_frame_on_channel(
        channel, node_id, batch_id, body, peer_label, true, profiling,
    )
    .await
}

async fn send_frame_on_channel(
    channel: &Arc<Channel>,
    node_id: &str,
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    validate_response_batch: bool,
    profiling: Option<&crate::prefill::TelemetryContext>,
) -> Result<LinkResponse> {
    let frame = encode_envelope(0, batch_id, &body)?;
    let bytes_sent = frame.len() as u64;

    let (reply_tx, reply_rx) = oneshot::channel();
    {
        let mut map = channel
            .pending
            .lock()
            .map_err(|_| anyhow!("channel delivery record corrupted"))?;
        if map.contains_key(batch_id) {
            return Err(anyhow!("repeated batch_id on the channel: {batch_id}"));
        }
        map.insert(batch_id.to_string(), Pending { reply: reply_tx });
    }
    let _guard = PendingGuard {
        pending: channel.pending.clone(),
        id: batch_id.to_string(),
    };

    let started = Instant::now();
    let send_started = started;
    channel
        .outbound
        .clone()
        .send(Bytes::from(frame))
        .await
        .map_err(|_| anyhow!("channel with {node_id} closed before the send"))?;
    let send_finished = Instant::now();

    let envelope = match tokio::time::timeout(crate::relay::frame_time_left(), reply_rx).await {
        Ok(Ok(Ok(envelope))) => envelope,
        Ok(Ok(Err(error))) => return Err(anyhow!("{peer_label}: {error}")),
        Ok(Err(_)) => return Err(anyhow!("{peer_label}: channel closed without answering")),
        Err(_) => return Err(anyhow!("{peer_label}: channel did not answer in time")),
    };
    let received = Instant::now();
    if !(200..300).contains(&envelope.status) {
        return Err(anyhow::Error::new(crate::relay::HttpRefusal {
            status: envelope.status,
            body: String::from_utf8_lossy(&envelope.body).into_owned(),
            peer_label: peer_label.to_string(),
        }));
    }
    if validate_response_batch {
        wire::validate_batch_bytes(&envelope.body)?;
    }
    let mut profiling_spans = Vec::new();
    if let Some(context) = profiling {
        profiling_spans.push(context.span(
            "frame-send",
            send_started,
            send_finished,
            None,
            Some("https-link".into()),
        ));
        profiling_spans.push(context.span(
            "transport-round-trip",
            started,
            received,
            None,
            Some("https-link".into()),
        ));
        profiling_spans.push(context.span(
            "frame-receive",
            received,
            received,
            None,
            Some("https-link".into()),
        ));
    }
    Ok(LinkResponse {
        body: envelope.body,
        round_trip_ms: started.elapsed().as_millis() as u64,
        bytes_sent,
        tls: false,
        profiling_spans,
    })
}

/// Sends a callback frame without turning the receiver's ACK into a barrier
/// on the sender. The ACK stays tied to a `Pending`, so that the reader
/// task observes HTTP 4xx/5xx and loss of the channel, but the generation
/// path does not wait for it.
async fn send_frame_without_response(
    channel: &Arc<Channel>,
    node_id: &str,
    batch_id: &str,
    body: Bytes,
    peer_label: &str,
    channel_key: String,
) -> Result<CallbackStatus> {
    if !channel.alive.load(Ordering::Acquire) {
        return Err(anyhow!("channel with {node_id} is already closed"));
    }
    let frame = encode_envelope(0, batch_id, &body)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    {
        let mut map = channel
            .pending
            .lock()
            .map_err(|_| anyhow!("channel delivery record corrupted"))?;
        if map.contains_key(batch_id) {
            return Err(anyhow!("repeated batch_id on the channel: {batch_id}"));
        }
        map.insert(batch_id.to_string(), Pending { reply: reply_tx });
    }
    let guard = PendingGuard {
        pending: channel.pending.clone(),
        id: batch_id.to_string(),
    };
    channel
        .outbound
        .clone()
        .send(Bytes::from(frame))
        .await
        .map_err(|_| anyhow!("{peer_label}: channel closed before the send"))?;

    let pending = channel.pending.clone();
    let channel = channel.clone();
    let node_id = node_id.to_string();
    let batch_id = batch_id.to_string();
    let status = CallbackStatus::new();
    let ack_status = status.clone();
    // Read before the `spawn`: the detached task does not inherit the frame's deadline.
    let ack_timeout = crate::relay::frame_time_left();
    tokio::spawn(async move {
        let failed = match tokio::time::timeout(ack_timeout, reply_rx).await {
            Ok(Ok(Ok(envelope))) if (200..300).contains(&envelope.status) => None,
            Ok(Ok(Ok(envelope))) => {
                if envelope.status == 410 {
                    ack_status.gateway_gone.store(true, Ordering::Release);
                } else {
                    ack_status.transport_failed.store(true, Ordering::Release);
                }
                Some(format!("peer answered HTTP {}", envelope.status))
            }
            Ok(Ok(Err(error))) => {
                ack_status.transport_failed.store(true, Ordering::Release);
                Some(error)
            }
            Ok(Err(_)) => {
                ack_status.transport_failed.store(true, Ordering::Release);
                Some("channel closed without answering".to_string())
            }
            Err(_) => {
                ack_status.transport_failed.store(true, Ordering::Release);
                Some("channel did not answer the ACK in time".to_string())
            }
        };
        drop(guard);
        if let Some(error) = failed {
            channel.alive.store(false, Ordering::Release);
            if let Ok(mut entries) = pending.lock() {
                entries.remove(&batch_id);
            }
            forget(&channel_key, &channel).await;
            warn!(peer = %node_id, batch = %batch_id, %error, "async ACK of the callback failed; channel discarded");
        }
    });
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::Router;
    use serde_json::json;

    fn batch(kind: &str, batch_id: &str, item_id: &str) -> Vec<u8> {
        wire::encode_batch_items(
            kind,
            batch_id,
            &[wire::BatchPayloadItem {
                item_id: item_id.into(),
                session_id: "s".into(),
                meta: json!({"session_id": "s"}),
                tensor: Some((vec![0.5f32; 8], vec![1, 8])),
                dtype: wire::WireDtype::F16,
            }],
        )
        .expect("valid frame")
    }

    #[test]
    fn the_reader_reassembles_frames_split_at_any_point() {
        let a = encode_envelope(0, "um", b"corpo-um").unwrap();
        let b = encode_envelope(503, "dois", b"corpo-dois").unwrap();
        let mut stream = a.clone();
        stream.extend_from_slice(&b);

        for cut in 1..stream.len() {
            let mut reader = FrameReader::new();
            reader.push(&stream[..cut]);
            let mut got = Vec::new();
            while let Some(envelope) = reader.next().unwrap() {
                got.push(envelope);
            }
            reader.push(&stream[cut..]);
            while let Some(envelope) = reader.next().unwrap() {
                got.push(envelope);
            }
            assert_eq!(got.len(), 2, "cut at {cut} lost a frame");
            assert_eq!(got[0].id, "um");
            assert_eq!(got[0].status, 0);
            assert_eq!(got[0].body, b"corpo-um");
            assert_eq!(got[1].id, "dois");
            assert_eq!(got[1].status, 503);
            assert_eq!(got[1].body, b"corpo-dois");
        }
    }

    #[test]
    fn the_reader_refuses_a_frame_above_the_cap() {
        let mut reader = FrameReader::new();
        reader.push(&(u32::MAX).to_be_bytes());
        assert!(reader.next().is_err());
    }

    async fn spawn_link_server(
        answer: impl Fn(String, Bytes) -> axum::response::Response + Clone + Send + Sync + 'static,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route(
            LINK_PATH,
            post(move |body: axum::body::Body| {
                let answer = answer.clone();
                async move {
                    serve(body, move |path, bytes| {
                        let answer = answer.clone();
                        async move { Ok(answer(path, bytes)) }
                    })
                }
            })
            .layer(axum::extract::DefaultBodyLimit::disable()),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).tcp_nodelay(true).await;
        });
        format!("http://{addr}")
    }

    /// Echoes the received frame as the reply, answering `delay_ms` later.
    fn echo_after(
        delay: std::collections::HashMap<String, u64>,
    ) -> impl Fn(String, Bytes) -> axum::response::Response + Clone + Send + Sync + 'static {
        move |path: String, body: Bytes| {
            let header = wire::peek_batch_header(&body).expect("valid frame");
            let (_, items) = wire::decode_batch_items(&body).expect("valid items");
            let delay = delay.get(&header.batch_id).copied().unwrap_or(0);
            let out = wire::encode_batch_items(
                path,
                header.batch_id,
                &items
                    .into_iter()
                    .map(|item| wire::BatchPayloadItem {
                        item_id: item.item_id,
                        session_id: "s".into(),
                        meta: item.meta,
                        tensor: item.tensor,
                        dtype: item.dtype,
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("valid response");
            let response = axum::response::Response::builder()
                .status(200)
                .header("content-type", "application/octet-stream")
                .body(axum::body::Body::from(out))
                .unwrap();
            if delay == 0 {
                response
            } else {
                // A synchronous handler cannot sleep; the delay is simulated
                // by returning a body that only closes later.
                let (mut tx, rx) = futures::channel::mpsc::channel::<Bytes>(1);
                let (parts, body) = response.into_parts();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                    let _ = tx.send(bytes).await;
                });
                axum::response::Response::from_parts(
                    parts,
                    axum::body::Body::from_stream(rx.map(Ok::<_, std::io::Error>)),
                )
            }
        }
    }

    #[tokio::test]
    async fn many_traversals_share_one_channel() {
        let url = spawn_link_server(echo_after(Default::default())).await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        for index in 0..8 {
            let id = format!("batch-{index}");
            let body = batch("/v1/step", &id, "item-0");
            let response = send_batch(&clients, "hop", &url, &id, Bytes::from(body), "hop")
                .await
                .expect("traversal through the channel");
            let header = wire::peek_batch_header(&response.body).unwrap();
            assert_eq!(header.batch_id, id);
        }
        assert_eq!(
            channels_opened("hop", &url),
            1,
            "eight traversals cannot open eight links"
        );
    }

    #[tokio::test]
    async fn a_persistent_callback_accepts_an_ack_that_is_not_a_batch() {
        let url = spawn_link_server(|_path: String, _body: Bytes| {
            axum::response::Response::builder()
                .status(200)
                .body(axum::body::Body::from("callback accepted"))
                .unwrap()
        })
        .await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let body = batch("/v1/remote-stream-result", "frame-0", "frame-0");

        let response = send_body_with_session(
            &clients,
            "gateway",
            &url,
            &[],
            "frame-0",
            Bytes::from(body),
            "remote-stream-return",
            None,
        )
        .await
        .expect("the callback must reuse the persistent channel");

        assert_eq!(response.body, b"callback accepted");
        assert_eq!(channels_opened("gateway", &url), 1);
    }

    #[tokio::test]
    async fn persistent_callbacks_do_not_wait_for_an_ack_per_frame() {
        let mut delays = std::collections::HashMap::new();
        for index in 0..8 {
            delays.insert(format!("frame-{index}"), 200u64);
        }
        let url = spawn_link_server(echo_after(delays)).await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let started = Instant::now();

        for index in 0..8 {
            let id = format!("frame-{index}");
            send_body_without_response_with_session(
                &clients,
                "callback-full-duplex",
                &url,
                &[],
                &id,
                Bytes::from(batch("/v1/remote-stream-result", &id, &id)),
                "remote-stream-return",
                Some("session-full-duplex"),
            )
            .await
            .expect("the frame must be enqueued without waiting for the ACK");
        }

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(150),
            "the emitter waited for an ACK per frame: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn an_async_callback_signals_a_vanished_gateway() {
        let url = spawn_link_server(|_path: String, _body: Bytes| {
            axum::response::Response::builder()
                .status(410)
                .body(axum::body::Body::from("unknown stream"))
                .unwrap()
        })
        .await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let status = send_body_without_response_with_session(
            &clients,
            "gateway-gone",
            &url,
            &[],
            "gone-frame",
            Bytes::from(batch(
                "/v1/remote-stream-result",
                "gone-frame",
                "gone-frame",
            )),
            "remote-stream-return",
            Some("session-gone"),
        )
        .await
        .expect("the callback must be enqueued without waiting for the ACK");

        tokio::time::timeout(Duration::from_secs(1), async {
            while !status.gateway_gone() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the async 410 ACK must be observed");
        assert!(!status.transport_failed());
    }

    #[tokio::test]
    async fn different_sessions_do_not_block_each_other_on_the_same_channel() {
        let mut delays = std::collections::HashMap::new();
        delays.insert("lenta".to_string(), 300u64);
        let url = spawn_link_server(echo_after(delays)).await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(10));

        let slow_body = batch("/v1/step", "lenta", "item-0");
        let fast_body = batch("/v1/step", "rapida", "item-0");
        let slow_clients = clients.clone();
        let slow_url = url.clone();
        // The slow session takes 300 ms; its frame declares a deadline that fits.
        let slow = tokio::spawn(crate::relay::with_frame_deadline(
            Instant::now() + Duration::from_secs(2),
            async move {
                send_batch(
                    &slow_clients,
                    "hop",
                    &slow_url,
                    "lenta",
                    Bytes::from(slow_body),
                    "hop",
                )
                .await
                .map(|response| response.round_trip_ms)
            },
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        let started = Instant::now();
        let fast = send_batch(
            &clients,
            "hop",
            &url,
            "rapida",
            Bytes::from(fast_body),
            "hop",
        )
        .await
        .expect("the fast session crosses");
        let fast_elapsed = started.elapsed();
        let slow_ms = slow.await.unwrap().expect("the slow session crosses");

        assert!(
            fast_elapsed < Duration::from_millis(200),
            "the fast session waited {fast_elapsed:?} for the slow one - the channel is head-of-line blocking"
        );
        assert!(
            slow_ms >= 250,
            "the slow session should have taken long, got {slow_ms}ms"
        );
        let _ = fast;
    }

    #[tokio::test]
    async fn a_peer_refusal_comes_back_as_an_http_refusal() {
        let url = spawn_link_server(|_path: String, _body: Bytes| {
            axum::response::Response::builder()
                .status(400)
                .body(axum::body::Body::from("unknown session"))
                .unwrap()
        })
        .await;
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let body = batch("/v1/step", "b", "item-0");
        let error = send_batch(&clients, "hop", &url, "b", Bytes::from(body), "hop")
            .await
            .expect_err("a refusal cannot become a success");
        let refusal = error
            .downcast_ref::<crate::relay::HttpRefusal>()
            .expect("a peer refusal preserves the status");
        assert_eq!(refusal.status, 400);
        assert!(refusal.body.contains("unknown session"));
    }

    /// A frame refused by the dispatcher - which is how the credential
    /// re-checked per frame refuses - becomes a refusal of **that frame**, and the
    /// channel keeps serving the following ones.
    #[tokio::test]
    async fn a_frame_refused_by_the_dispatcher_does_not_bring_down_the_channel() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let refuse = Arc::new(AtomicBool::new(true));
        let server = refuse.clone();
        let router = Router::new().route(
            LINK_PATH,
            post(move |body: axum::body::Body| {
                let server = server.clone();
                async move {
                    serve(body, move |path, bytes| {
                        let server = server.clone();
                        async move {
                            if server.swap(false, Ordering::Relaxed) {
                                return Err(anyhow!("invalid inference credential"));
                            }
                            let header = wire::peek_batch_header(&bytes).unwrap();
                            let (_, items) = wire::decode_batch_items(&bytes).unwrap();
                            let out = wire::encode_batch_items(
                                path,
                                header.batch_id,
                                &items
                                    .into_iter()
                                    .map(|item| wire::BatchPayloadItem {
                                        item_id: item.item_id,
                                        session_id: "s".into(),
                                        meta: item.meta,
                                        tensor: item.tensor,
                                        dtype: item.dtype,
                                    })
                                    .collect::<Vec<_>>(),
                            )
                            .unwrap();
                            Ok(axum::response::Response::builder()
                                .status(200)
                                .body(axum::body::Body::from(out))
                                .unwrap())
                        }
                    })
                }
            })
            .layer(axum::extract::DefaultBodyLimit::disable()),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).tcp_nodelay(true).await;
        });
        let url = format!("http://{addr}");
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));

        let error = send_batch(
            &clients,
            "estrito",
            &url,
            "refused",
            Bytes::from(batch("/v1/step", "refused", "item-0")),
            "estrito",
        )
        .await
        .expect_err("a frame without a valid credential must be refused");
        let refusal = error
            .downcast_ref::<crate::relay::HttpRefusal>()
            .expect("a per-frame refusal preserves the status");
        assert_eq!(refusal.status, 400);
        assert!(refusal.body.contains("credential"));

        let accepted = send_batch(
            &clients,
            "estrito",
            &url,
            "aceito",
            Bytes::from(batch("/v1/step", "aceito", "item-0")),
            "estrito",
        )
        .await
        .expect("the next frame uses the same channel, which stays alive");
        assert_eq!(
            wire::peek_batch_header(&accepted.body).unwrap().batch_id,
            "aceito"
        );
        assert_eq!(channels_opened("estrito", &url), 1);
    }

    #[tokio::test]
    async fn opening_a_dead_edge_does_not_block_another() {
        use tokio::sync::Notify;

        let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap();
        let accepted = Arc::new(Notify::new());
        let accepted_task = accepted.clone();
        tokio::spawn(async move {
            let (stream, _) = blackhole.accept().await.expect("accepted the dead open");
            accepted_task.notify_one();
            let _stream = stream;
            std::future::pending::<()>().await;
        });

        let healthy = spawn_link_server(echo_after(Default::default())).await;
        let dead = format!("http://{blackhole_addr}");
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let dead_clients = clients.clone();
        let dead_task = tokio::spawn(async move {
            send_batch(
                &dead_clients,
                "aresta-morta",
                &dead,
                "dead",
                Bytes::from(batch("/v1/step", "dead", "item-0")),
                "aresta-morta",
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), accepted.notified())
            .await
            .expect("the dead open started");

        let started = Instant::now();
        let healthy_result = tokio::time::timeout(
            LINK_OPEN_TIMEOUT / 2,
            send_batch(
                &clients,
                "aresta-saudavel",
                &healthy,
                "healthy",
                Bytes::from(batch("/v1/step", "healthy", "item-0")),
                "aresta-saudavel",
            ),
        )
        .await
        .expect("opening another edge cannot wait for the blackhole")
        .expect("the healthy edge must cross");

        assert_eq!(
            wire::peek_batch_header(&healthy_result.body)
                .expect("valid response")
                .batch_id,
            "healthy"
        );
        println!(
            "link_lab: healthy edge answered while the dead open was in flight in {:?}",
            started.elapsed()
        );
        assert!(
            started.elapsed() < LINK_OPEN_TIMEOUT / 2,
            "the healthy edge was stalled for {:?}",
            started.elapsed()
        );
        assert!(dead_task.await.unwrap().is_err());
        assert_eq!(channels_opened("aresta-saudavel", &healthy), 1);
    }

    #[tokio::test]
    async fn an_unreachable_destination_fails_fast_and_is_quarantined() {
        // Closed port: the channel does not open.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}");
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(2));
        let body = batch("/v1/step", "b", "item-0");
        assert!(send_batch(
            &clients,
            "morto",
            &url,
            "b",
            Bytes::from(body.clone()),
            "morto"
        )
        .await
        .is_err());
        let started = Instant::now();
        let error = send_batch(&clients, "morto", &url, "b2", Bytes::from(body), "morto")
            .await
            .expect_err("the second attempt also fails");
        assert!(
            error.to_string().contains("quarantined"),
            "the second attempt should stop at the failure memory, got: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "the quarantine exists to not pay for the dead connection again"
        );
    }

    #[tokio::test]
    async fn a_blackhole_open_falls_to_the_healthy_route_and_is_not_repeated() {
        let blackhole_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let blackhole_hits_task = blackhole_hits.clone();
        let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = blackhole.accept().await {
                blackhole_hits_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let _stream = stream;
                    std::future::pending::<()>().await;
                });
            }
        });

        let healthy = spawn_link_server(echo_after(std::collections::HashMap::new())).await;
        let bad = format!("http://{blackhole_addr}");
        let candidates = vec![healthy.clone()];
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(600));
        let started = Instant::now();
        for batch_id in ["blackhole-1", "blackhole-2"] {
            let response = send_batch_with_candidates(
                &clients,
                "blackhole-fallback",
                &bad,
                &candidates,
                batch_id,
                Bytes::from(batch("/v1/step", batch_id, "item-0")),
                "blackhole-fallback",
            )
            .await
            .expect("the healthy route must take over the channel");
            assert_eq!(
                wire::peek_batch_header(&response.body).unwrap().batch_id,
                batch_id
            );
        }
        assert_eq!(blackhole_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(channels_opened("blackhole-fallback", &healthy), 1);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "the dead open was paid again: {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_dead_channel_leaves_no_pending_delivery() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Server that accepts the channel and vanishes without replying.
        let router = Router::new().route(
            LINK_PATH,
            post(|_body: axum::body::Body| async move {
                let (tx, rx) = futures::channel::mpsc::channel::<Bytes>(1);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    drop(tx);
                });
                axum::response::Response::builder()
                    .status(200)
                    .body(axum::body::Body::from_stream(
                        rx.map(Ok::<_, std::io::Error>),
                    ))
                    .unwrap()
            })
            .layer(axum::extract::DefaultBodyLimit::disable()),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).tcp_nodelay(true).await;
        });
        let url = format!("http://{addr}");
        let clients = crate::peer_client::PeerClients::new(Duration::from_secs(5));
        let body = batch("/v1/step", "b", "item-0");
        let error = send_batch(&clients, "mudo", &url, "b", Bytes::from(body), "mudo")
            .await
            .expect_err("a channel that dies must return an error, not hang");
        assert!(
            error.to_string().contains("mudo"),
            "the error must name the peer: {error}"
        );
        assert!(
            !has_channel("mudo", &url).await,
            "the dead channel must leave the pool so the next call reopens"
        );
    }
}
