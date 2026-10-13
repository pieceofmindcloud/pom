//! The stage link of a plugin engine split across machines.
//!
//! An inference service that publishes `stages: true` also runs POM layer
//! slices: each machine of the deployment loads its slice in the plugin's
//! engine, the earlier stage hands the later one the hidden state of every
//! token, and the last stage (the head, the sampling) serves the requests -
//! the gateway sends the prompt there ([`crate::plugin_engine::plugin_of_plan`]).
//!
//! The engines never listen on the network. The node carries their link:
//!
//! - **The later stage's machine** ([`prepare`]) finds the machine of the same
//!   deployment whose slice ends where its own starts, takes that stage's link
//!   secret from it (`GET /api/internal/plugins/:code/engine/link/:deploy`) and
//!   opens a relay on loopback. `engine.load` receives
//!   `link: {token, prev_port}`; the engine connects to `127.0.0.1:prev_port`.
//! - **The relay** reads the engine's frames - a little-endian `u32` length,
//!   then that many bytes, the same framing both ways - and posts each one,
//!   signed, to `POST /api/internal/plugins/:code/engine/link/:deploy/:conn/:seq`
//!   on the earlier stage's machine, along the route already measured to it.
//!   The answer's body is the answer frame. An empty body (to `.../:conn`)
//!   closes `conn`.
//! - **In order, several at once** (the micro-pipelining): the engine may send
//!   frames before it has read the answers to the earlier ones, as a prompt's
//!   pieces. The relay keeps up to the deployment's
//!   `prefill_max_inflight_chunks` frames of a connection posted at once
//!   (`link.max_inflight` tells the engine) and writes their answers back in
//!   the order of the frames; `seq` numbers them from 0, and the earlier
//!   machine passes a frame to its engine only after the frame before it, so
//!   its engine reads the next piece while the answer to the last one travels.
//! - **The envelope** of every frame between an engine and its node: a kind
//!   byte, then `1` the bytes as they are, or `2` a tensor - `u32` meta
//!   length, the meta, then `f32` values. Between the machines a tensor
//!   crosses in the deployment's `hidden_state_dtype`, encoded by the POM's
//!   own `wire` (f16, f8, groupwise int4), and reaches the engine as `f32`
//!   again; an opaque frame crosses as it is. A tensor that the precision
//!   cannot carry (f8 past its range) crosses that once as `f32`.
//! - **The earlier stage's machine** ([`link_frame_route`]) keeps one loopback
//!   connection per `conn` to the port its plugin gives in `engine.link`
//!   (`{status, port, token}`) and passes the frames through. Its own
//!   `engine.load` gets `link: {}`: the plugin makes the secret.
//!
//! The node reads nothing of a frame but its envelope. Only a live peer serving
//! the next slice of the same deployment gets the secret or a frame through.

use crate::gateway::GatewayState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::stream::{FuturesOrdered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{info, warn};

/// The largest frame either way: a prompt piece's hidden states.
pub(crate) const MAX_FRAME: usize = 64 << 20;
/// One frame may run a long prompt piece through the earlier stage.
const FRAME_TIMEOUT: Duration = Duration::from_secs(900);
/// How long a later stage waits for the earlier one to load its slice.
const WAIT_EARLIER: Duration = Duration::from_secs(3600);
const RETRY: Duration = Duration::from_secs(3);
/// Loopback connections the earlier stage's machine keeps open at once.
const MAX_CONNECTIONS: usize = 16;
/// Frames of one engine connection posted to the earlier machine at once: the
/// deployment's `prefill_max_inflight_chunks`, within the prefill's own range.
fn max_in_flight(cfg: &crate::runtime::NodeConfig) -> usize {
    (cfg.prefill_max_inflight_chunks as usize)
        .clamp(1, crate::prefill::PrefillOverlapWindow::MAX_INFLIGHT_CHUNKS)
}

const KIND_BYTES: u8 = 1;
const KIND_TENSOR: u8 = 2;

/// An engine's frame as it crosses between the machines: a tensor in
/// `dtype`, through the POM's `wire` (its meta as hex in the header), and an
/// opaque frame as it is.
fn to_network(frame: Vec<u8>, dtype: crate::wire::WireDtype) -> Result<Vec<u8>, String> {
    match frame.first() {
        Some(&KIND_BYTES) => Ok(frame),
        Some(&KIND_TENSOR) => {
            let meta_len = frame
                .get(1..5)
                .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
                .ok_or("a tensor frame without its meta length")?;
            let values = frame
                .get(5 + meta_len..)
                .filter(|values| values.len() % 4 == 0)
                .ok_or("a tensor frame whose values are not f32")?;
            let values: Vec<f32> = values
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            let meta = json!({ "meta": hex::encode(&frame[5..5 + meta_len]) });
            let shape = vec![1, values.len()];
            let encoded = crate::wire::encode(&meta, Some((&values, shape.clone())), dtype)
                .or_else(|_| {
                    crate::wire::encode(&meta, Some((&values, shape)), crate::wire::WireDtype::F32)
                })
                .map_err(|error| error.to_string())?;
            let mut out = Vec::with_capacity(1 + encoded.len());
            out.push(KIND_TENSOR);
            out.extend_from_slice(&encoded);
            Ok(out)
        }
        _ => Err("a frame without the link's kind byte".into()),
    }
}

/// The inverse of [`to_network`]: the frame the engine reads, its tensor in `f32`.
fn from_network(body: &[u8]) -> Result<Vec<u8>, String> {
    match body.first() {
        Some(&KIND_BYTES) => Ok(body.to_vec()),
        Some(&KIND_TENSOR) => {
            let (meta, tensor) =
                crate::wire::decode(&body[1..]).map_err(|error| error.to_string())?;
            let meta = meta["meta"]
                .as_str()
                .and_then(|text| hex::decode(text).ok())
                .ok_or("a tensor without its meta")?;
            let values = tensor.map(|(values, _)| values).unwrap_or_default();
            let mut out = Vec::with_capacity(5 + meta.len() + values.len() * 4);
            out.push(KIND_TENSOR);
            out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
            out.extend_from_slice(&meta);
            for value in values {
                out.extend_from_slice(&value.to_le_bytes());
            }
            Ok(out)
        }
        _ => Err("a frame without the link's kind byte".into()),
    }
}

static GATEWAY: OnceLock<GatewayState> = OnceLock::new();

/// The gateway state, for the runtime supervisor that starts the relay.
pub(crate) fn install(state: GatewayState) {
    let _ = GATEWAY.set(state);
}

/// Whether this deployment is one stage of a model split across machines.
pub(crate) fn is_stage(cfg: &crate::runtime::NodeConfig) -> bool {
    cfg.total_layers > 0 && (cfg.start_layer > 0 || cfg.end_layer + 1 < cfg.total_layers)
}

/// A slug the node accepts as a deployment or connection id in a path.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// The earlier stage of `cfg` among the live peers: same deployment, the
/// slice that ends just before this one, served by the same plugin.
async fn earlier_peer(
    state: &GatewayState,
    cfg: &crate::runtime::NodeConfig,
    code: &str,
) -> Option<crate::network::PeerInfo> {
    state.all_peers().await.into_iter().find(|peer| {
        peer.node_id != state.network.node_id
            && peer.deploy_id == cfg.deploy_id
            && peer.end_layer + 1 == cfg.start_layer
            && crate::plugin_engine::plugin_of_capabilities(&peer.inference_frame_capabilities)
                == Some(code)
    })
}

/// The loopback relay of a later stage; it ends when dropped.
pub(crate) struct Relay {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The `link` that `engine.load` receives, and for a later stage the relay
/// behind its `prev_port`. Waits for the earlier stage's machine; `stopped`
/// ends the wait.
pub(crate) async fn prepare(
    cfg: &crate::runtime::NodeConfig,
    code: &str,
    stopped: impl Fn() -> bool,
) -> anyhow::Result<(Value, Option<Relay>)> {
    if cfg.start_layer == 0 {
        return Ok((json!({}), None));
    }
    let state = GATEWAY
        .get()
        .ok_or_else(|| anyhow::anyhow!("the node's gateway is not up"))?
        .clone();
    if !valid_id(&cfg.deploy_id) {
        anyhow::bail!("a split model needs its deployment id");
    }
    let deadline = Instant::now() + WAIT_EARLIER;
    let mut announced = false;
    let token = loop {
        if stopped() {
            anyhow::bail!("stopped while waiting for the earlier stage");
        }
        if let Some(peer) = earlier_peer(&state, cfg, code).await {
            match link_secret(&state, &peer, code, &cfg.deploy_id).await {
                Ok(token) => break token,
                Err(error) if !announced => {
                    info!(plugin = %code, peer = %peer.node_id, %error, "waiting for the earlier stage's link");
                    announced = true;
                }
                Err(_) => {}
            }
        } else if !announced {
            info!(plugin = %code, start_layer = cfg.start_layer, "waiting for the machine of the earlier stage");
            announced = true;
        }
        if Instant::now() > deadline {
            anyhow::bail!(
                "no machine of this deployment serves layers up to {} with the plugin '{code}'",
                cfg.start_layer.saturating_sub(1)
            );
        }
        tokio::time::sleep(RETRY).await;
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let relay_cfg = cfg.clone();
    let relay_code = code.to_owned();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let _ = stream.set_nodelay(true);
            tokio::spawn(relay_connection(
                state.clone(),
                relay_cfg.clone(),
                relay_code.clone(),
                stream,
            ));
        }
    });
    Ok((
        json!({ "token": token, "prev_port": port, "max_inflight": max_in_flight(cfg) }),
        Some(Relay { task }),
    ))
}

async fn link_secret(
    state: &GatewayState,
    peer: &crate::network::PeerInfo,
    code: &str,
    deploy_id: &str,
) -> Result<String, String> {
    let response = state
        .control_get(
            peer,
            &format!("/api/internal/plugins/{code}/engine/link/{deploy_id}"),
            Duration::from_secs(10),
        )
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        return Err(body["error"].as_str().unwrap_or("refused").to_owned());
    }
    body["token"]
        .as_str()
        .filter(|token| valid_token(token))
        .map(str::to_owned)
        .ok_or_else(|| "the earlier stage gave no link secret".into())
}

fn valid_token(token: &str) -> bool {
    (16..=256).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_graphic())
}

async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    match stream.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

async fn write_frame(
    stream: &mut (impl AsyncWrite + Unpin),
    payload: &[u8],
) -> std::io::Result<()> {
    stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

/// One engine connection of a later stage: every frame goes to the earlier
/// stage's machine and its answer comes back, up to [`MAX_IN_FLIGHT`] frames
/// at once, the answers in the order of the frames. A frame the earlier
/// machine cannot take yet (its stage still loading) is sent again.
async fn relay_connection(
    state: GatewayState,
    cfg: crate::runtime::NodeConfig,
    code: String,
    stream: TcpStream,
) {
    let conn = uuid::Uuid::new_v4().simple().to_string();
    let base = format!(
        "/api/internal/plugins/{code}/engine/link/{}/{conn}",
        cfg.deploy_id
    );
    let (mut reader, mut writer) = stream.into_split();
    // the reader is a task of its own: a frame half read is never dropped by the select below
    let in_flight = max_in_flight(&cfg);
    let (frames, mut incoming) = tokio::sync::mpsc::channel::<Vec<u8>>(in_flight);
    let reading = {
        let code = code.clone();
        tokio::spawn(async move {
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(frame)) => {
                        if frames.send(frame).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        warn!(plugin = %code, %error, "stage link: the engine's frame");
                        break;
                    }
                }
            }
        })
    };
    let peer = Arc::new(tokio::sync::Mutex::new(None::<crate::network::PeerInfo>));
    let mut pending = FuturesOrdered::new();
    let mut seq: u64 = 0;
    let mut open = true;
    loop {
        tokio::select! {
            frame = incoming.recv(), if open && pending.len() < in_flight => match frame {
                Some(frame) => {
                    pending.push_back(forward(
                        state.clone(),
                        cfg.clone(),
                        code.clone(),
                        Arc::clone(&peer),
                        format!("{base}/{seq}"),
                        frame,
                    ));
                    seq += 1;
                }
                None => open = false,
            },
            answer = pending.next(), if !pending.is_empty() => match answer {
                Some(Some(answer)) => {
                    if write_frame(&mut writer, &answer).await.is_err() {
                        break;
                    }
                }
                _ => break,
            },
            else => break,
        }
    }
    reading.abort();
    drop(pending);
    let target = peer.lock().await.clone();
    if let Some(target) = target {
        let _ = send(&state, &target, &base, Vec::new()).await;
    }
}

/// One frame to the earlier machine at `path`, tried again while that machine
/// cannot take it yet; `None` gives up on the connection.
/// The phase a frame belongs to, for the trace: the prompt's frames come before the first token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LinkPhase {
    Prefill,
    Decode,
}

/// What the links to an earlier stage carried. A frame's `round_trip_ms` is the time from sending it to its
/// answer; `served_ms` is the time the earlier machine spent on it, its own work and the stages below it (it
/// reports it in the answer); `network_ms` is the rest - the wire and the two relays.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct LinkStats {
    pub(crate) calls: u32,
    pub(crate) round_trip_ms: u64,
    pub(crate) max_round_trip_ms: u64,
    pub(crate) served_ms: u64,
    pub(crate) network_ms: u64,
    pub(crate) bytes_sent: u64,
    pub(crate) bytes_received: u64,
}

impl LinkStats {
    fn add(&mut self, other: &LinkStats) {
        self.calls += other.calls;
        self.round_trip_ms += other.round_trip_ms;
        self.max_round_trip_ms = self.max_round_trip_ms.max(other.max_round_trip_ms);
        self.served_ms += other.served_ms;
        self.network_ms += other.network_ms;
        self.bytes_sent += other.bytes_sent;
        self.bytes_received += other.bytes_received;
    }
}

/// The traffic of one channel (QUIC, HTTP, the libp2p tunnel or the local loopback) towards one earlier node, as
/// aggregates: the frames, their round trips and network times (sum, minimum, maximum) and the bytes. The trace
/// reports the mean of each.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ChannelStats {
    pub(crate) channel: String,
    pub(crate) calls: u32,
    pub(crate) fallback_calls: u32,
    pub(crate) round_trip_sum_ms: u64,
    pub(crate) round_trip_min_ms: u64,
    pub(crate) round_trip_max_ms: u64,
    pub(crate) network_sum_ms: u64,
    pub(crate) network_min_ms: u64,
    pub(crate) network_max_ms: u64,
    pub(crate) bytes_sent: u64,
    pub(crate) bytes_received: u64,
}

impl ChannelStats {
    fn one(channel: &str, round_trip_ms: u64, network_ms: u64, sent: u64, received: u64) -> Self {
        Self {
            channel: channel.to_owned(),
            calls: 1,
            fallback_calls: 0,
            round_trip_sum_ms: round_trip_ms,
            round_trip_min_ms: round_trip_ms,
            round_trip_max_ms: round_trip_ms,
            network_sum_ms: network_ms,
            network_min_ms: network_ms,
            network_max_ms: network_ms,
            bytes_sent: sent,
            bytes_received: received,
        }
    }

    fn add(&mut self, other: &ChannelStats) {
        if other.calls == 0 {
            return;
        }
        if self.calls == 0 {
            *self = other.clone();
            return;
        }
        self.calls += other.calls;
        self.fallback_calls += other.fallback_calls;
        self.round_trip_sum_ms += other.round_trip_sum_ms;
        self.round_trip_min_ms = self.round_trip_min_ms.min(other.round_trip_min_ms);
        self.round_trip_max_ms = self.round_trip_max_ms.max(other.round_trip_max_ms);
        self.network_sum_ms += other.network_sum_ms;
        self.network_min_ms = self.network_min_ms.min(other.network_min_ms);
        self.network_max_ms = self.network_max_ms.max(other.network_max_ms);
        self.bytes_sent += other.bytes_sent;
        self.bytes_received += other.bytes_received;
    }
}

/// The header an answer to a frame carries: how long this machine spent on it, and the links below it, so the
/// requester's trace has the whole chain of stages (each machine forwards what its own downstream reported).
pub(crate) const ENVELOPE_HEADER: &str = "x-pom-stage-telemetry";

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct StageEnvelope {
    pub(crate) served_ms: u64,
    #[serde(default)]
    pub(crate) links: Vec<StageLinkWire>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct StageLinkWire {
    pub(crate) node: String,
    #[serde(flatten)]
    pub(crate) stats: LinkStats,
    #[serde(default)]
    pub(crate) channels: Vec<ChannelStats>,
}

/// A link total as one machine reports it to the next one up (the `done` event of its generation): the earlier
/// node it talked to, the phase its frames were in, the totals and the channels they used.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ReportedLink {
    pub(crate) node: String,
    pub(crate) phase: LinkPhase,
    #[serde(flatten)]
    pub(crate) stats: LinkStats,
    #[serde(default)]
    pub(crate) channels: Vec<ChannelStats>,
}

/// Per (plugin code, earlier node id, phase) totals, drained by the generation that used them.
fn link_stats() -> &'static Mutex<HashMap<(String, String, LinkPhase), LinkStats>> {
    static STATS: OnceLock<Mutex<HashMap<(String, String, LinkPhase), LinkStats>>> = OnceLock::new();
    STATS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per (plugin code, earlier node id, phase, channel) totals of the same frames.
fn channel_stats() -> &'static Mutex<HashMap<(String, String, LinkPhase, String), ChannelStats>> {
    static STATS: OnceLock<Mutex<HashMap<(String, String, LinkPhase, String), ChannelStats>>> =
        OnceLock::new();
    STATS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The time this machine spent waiting on the stages below it, per plugin and phase, since it was last taken. A
/// frame's own work is its time here minus the waits its own relays made meanwhile.
fn own_waits() -> &'static Mutex<HashMap<(String, LinkPhase), u64>> {
    static WAITS: OnceLock<Mutex<HashMap<(String, LinkPhase), u64>>> = OnceLock::new();
    WAITS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn add_own_wait(code: &str, phase: LinkPhase, ms: u64) {
    if let Ok(mut waits) = own_waits().lock() {
        *waits.entry((code.to_owned(), phase)).or_insert(0) += ms;
    }
}

/// The waits so far, without taking them.
pub(crate) fn peek_own_wait(code: &str, phase: LinkPhase) -> u64 {
    own_waits()
        .lock()
        .ok()
        .and_then(|waits| waits.get(&(code.to_owned(), phase)).copied())
        .unwrap_or(0)
}

/// Takes the waits of `phase`: the head reads them once per generation.
pub(crate) fn take_own_wait(code: &str, phase: LinkPhase) -> u64 {
    own_waits()
        .lock()
        .ok()
        .and_then(|mut waits| waits.remove(&(code.to_owned(), phase)))
        .unwrap_or(0)
}

/// The phase and the token count of a request a later stage sends, from the engine's stage protocol: `P` is a prompt
/// piece of n tokens (prefill), `W` a verify window of T tokens (decode). The frame is the envelope's kind byte, the
/// tag, an i64 position, then the u32 count (native byte order, little-endian on the machines the engine runs on).
pub(crate) fn frame_phase_and_tokens(frame: &[u8]) -> Option<(LinkPhase, u32)> {
    if frame.len() < 14 || frame[0] != KIND_BYTES {
        return None;
    }
    let phase = match frame[1] {
        b'P' => LinkPhase::Prefill,
        b'W' => LinkPhase::Decode,
        _ => return None,
    };
    let n = u32::from_le_bytes([frame[10], frame[11], frame[12], frame[13]]);
    (n > 0).then_some((phase, n))
}

/// The phase each plugin's frames are in now (a generation starts in the prompt).
fn phases() -> &'static Mutex<HashMap<String, LinkPhase>> {
    static PHASES: OnceLock<Mutex<HashMap<String, LinkPhase>>> = OnceLock::new();
    PHASES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn set_phase(code: &str, phase: LinkPhase) {
    if let Ok(mut phases) = phases().lock() {
        phases.insert(code.to_owned(), phase);
    }
}

fn current_phase(code: &str) -> LinkPhase {
    phases()
        .lock()
        .ok()
        .and_then(|phases| phases.get(code).copied())
        .unwrap_or(LinkPhase::Prefill)
}

/// The channel a frame to `node_id` leaves by, as the route book has it now.
pub(crate) fn channel_of(node_id: &str, own_node_id: &str) -> &'static str {
    if node_id == own_node_id {
        return "local";
    }
    match crate::route_plan::book().active(node_id, crate::route_plan::Plane::Gateway) {
        Some(route) => route.kind.as_str(),
        None => "http",
    }
}

/// One exchange with `node_id` over `channel`: the time from sending to answer (`elapsed`), the time the other side
/// spent on it (`served_ms`), and the bytes each way. The network part is what is left.
pub(crate) fn record_leg(
    code: &str,
    node_id: &str,
    phase: LinkPhase,
    elapsed: Duration,
    sent: u64,
    received: u64,
    served_ms: u64,
    channel: &str,
) {
    let round_trip_ms = elapsed.as_millis() as u64;
    let network_ms = round_trip_ms.saturating_sub(served_ms);
    let stats = LinkStats {
        calls: 1,
        round_trip_ms,
        max_round_trip_ms: round_trip_ms,
        served_ms,
        network_ms,
        bytes_sent: sent,
        bytes_received: received,
    };
    let channel_stats = ChannelStats::one(channel, round_trip_ms, network_ms, sent, received);
    merge_in(code, node_id, phase, stats, std::slice::from_ref(&channel_stats));
}

/// One frame answered by the earlier machine `node_id`, in the current phase.
fn record_link(
    code: &str,
    node_id: &str,
    elapsed: Duration,
    sent: u64,
    received: u64,
    served_ms: u64,
    channel: &str,
) {
    let phase = current_phase(code);
    add_own_wait(code, phase, elapsed.as_millis() as u64);
    record_leg(code, node_id, phase, elapsed, sent, received, served_ms, channel);
}

/// Totals a machine below us reported in its answer, filed under the phase this machine is in.
fn merge_downstream(code: &str, node_id: &str, stats: LinkStats, channels: &[ChannelStats]) {
    merge_in(code, node_id, current_phase(code), stats, channels);
}

fn merge_in(code: &str, node_id: &str, phase: LinkPhase, stats: LinkStats, channels: &[ChannelStats]) {
    if let Ok(mut totals) = link_stats().lock() {
        totals
            .entry((code.to_owned(), node_id.to_owned(), phase))
            .or_default()
            .add(&stats);
    }
    if let Ok(mut totals) = channel_stats().lock() {
        for channel in channels {
            totals
                .entry((
                    code.to_owned(),
                    node_id.to_owned(),
                    phase,
                    channel.channel.clone(),
                ))
                .and_modify(|total| total.add(channel))
                .or_insert_with(|| channel.clone());
        }
    }
}

/// Takes the link totals of `code`, per node and phase.
pub(crate) fn take_link_stats(code: &str) -> Vec<(String, LinkPhase, LinkStats)> {
    let Ok(mut totals) = link_stats().lock() else {
        return Vec::new();
    };
    let keys: Vec<(String, String, LinkPhase)> = totals
        .keys()
        .filter(|(plugin, _, _)| plugin == code)
        .cloned()
        .collect();
    keys.into_iter()
        .filter_map(|key| totals.remove(&key).map(|value| (key.1, key.2, value)))
        .collect()
}

/// Takes the per-channel totals of `code`, per node, phase and channel.
pub(crate) fn take_channel_stats(code: &str) -> Vec<(String, LinkPhase, ChannelStats)> {
    let Ok(mut totals) = channel_stats().lock() else {
        return Vec::new();
    };
    let keys: Vec<(String, String, LinkPhase, String)> = totals
        .keys()
        .filter(|(plugin, _, _, _)| plugin == code)
        .cloned()
        .collect();
    keys.into_iter()
        .filter_map(|key| totals.remove(&key).map(|value| (key.1, key.2, value)))
        .collect()
}

/// What this machine answers upward: the totals below it, summed over the phases (the requester files them).
fn take_downstream_links(code: &str) -> Vec<StageLinkWire> {
    let mut summed: HashMap<String, LinkStats> = HashMap::new();
    for (node, _phase, stats) in take_link_stats(code) {
        summed.entry(node).or_default().add(&stats);
    }
    let mut channels: HashMap<String, Vec<ChannelStats>> = HashMap::new();
    for (node, _phase, channel) in take_channel_stats(code) {
        let list = channels.entry(node).or_default();
        match list.iter_mut().find(|known| known.channel == channel.channel) {
            Some(known) => known.add(&channel),
            None => list.push(channel),
        }
    }
    for node in channels.keys() {
        summed.entry(node.clone()).or_default();
    }
    summed
        .into_iter()
        .map(|(node, stats)| StageLinkWire {
            channels: channels.remove(&node).unwrap_or_default(),
            node,
            stats,
        })
        .collect()
}

/// The answer to a frame, with the earlier machine's envelope when it sent one.
struct StageAnswer {
    body: Vec<u8>,
    envelope: StageEnvelope,
}

fn parse_envelope(headers: &reqwest::header::HeaderMap) -> StageEnvelope {
    headers
        .get(ENVELOPE_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| serde_json::from_str::<StageEnvelope>(text).ok())
        .unwrap_or_default()
}

/// Takes this machine's link totals for `code` as reports, for the `done` event.
pub(crate) fn drain_reported_links(code: &str) -> Vec<ReportedLink> {
    let mut channels: HashMap<(String, LinkPhase), Vec<ChannelStats>> = HashMap::new();
    for (node, phase, channel) in take_channel_stats(code) {
        channels.entry((node, phase)).or_default().push(channel);
    }
    take_link_stats(code)
        .into_iter()
        .map(|(node, phase, stats)| ReportedLink {
            channels: channels.remove(&(node.clone(), phase)).unwrap_or_default(),
            node,
            phase,
            stats,
        })
        .collect()
}

/// Files a total a machine above us reported (its own links and the ones below it) under its own phase.
pub(crate) fn merge_reported_link(code: &str, link: ReportedLink) {
    merge_in(code, &link.node, link.phase, link.stats, &link.channels);
}

async fn forward(
    state: GatewayState,
    cfg: crate::runtime::NodeConfig,
    code: String,
    peer: Arc<tokio::sync::Mutex<Option<crate::network::PeerInfo>>>,
    path: String,
    frame: Vec<u8>,
) -> Option<Vec<u8>> {
    // The engine's own request names the phase its frames belong to, whatever the stream has told this machine.
    if let Some((phase, _)) = frame_phase_and_tokens(&frame) {
        set_phase(&code, phase);
    }
    let frame = match to_network(frame, cfg.hidden_state_dtype) {
        Ok(frame) => frame,
        Err(error) => {
            warn!(plugin = %code, %error, "stage link: the engine's frame");
            return None;
        }
    };
    let deadline = Instant::now() + WAIT_EARLIER;
    loop {
        let target = {
            let mut peer = peer.lock().await;
            if peer.is_none() {
                *peer = earlier_peer(&state, &cfg, &code).await;
            }
            peer.clone()
        };
        let attempt = match &target {
            None => Err(Refusal::Retry(
                "the earlier stage's machine is not in the network".to_owned(),
            )),
            Some(target) => {
                let started = Instant::now();
                let sent = frame.len() as u64;
                let channel = channel_of(&target.node_id, &state.network.node_id);
                match send(&state, target, &path, frame.clone()).await {
                    Ok(answer) => {
                        record_link(
                            &code,
                            &target.node_id,
                            started.elapsed(),
                            sent,
                            answer.body.len() as u64,
                            answer.envelope.served_ms,
                            channel,
                        );
                        for link in answer.envelope.links {
                            merge_downstream(&code, &link.node, link.stats, &link.channels);
                        }
                        Ok(answer.body)
                    }
                    Err(refusal) => Err(refusal),
                }
            }
        };
        match attempt {
            Ok(answer) => return from_network(&answer)
                .map_err(
                    |error| warn!(plugin = %code, %error, "stage link: the earlier stage's answer"),
                )
                .ok(),
            Err(Refusal::Retry(error)) if Instant::now() < deadline => {
                warn!(plugin = %code, %error, "stage link: trying again");
                *peer.lock().await = None;
                tokio::time::sleep(RETRY).await;
            }
            Err(Refusal::Retry(error) | Refusal::Final(error)) => {
                warn!(plugin = %code, %error, "stage link: giving up");
                return None;
            }
        }
    }
}

/// Why a frame got no answer: worth sending again, or not (the earlier
/// machine has already passed a frame of that number on, or lost its link).
enum Refusal {
    Retry(String),
    Final(String),
}

/// A frame that the earlier machine answered, or why not. A refusal of the
/// frame itself (`4xx` other than a wait) is not tried again by the caller
/// any differently: the engine sees the connection fail and starts over.
async fn send(
    state: &GatewayState,
    peer: &crate::network::PeerInfo,
    path: &str,
    frame: Vec<u8>,
) -> Result<StageAnswer, Refusal> {
    let response = state
        .control_post_bytes_routed(peer, path, frame, FRAME_TIMEOUT)
        .await
        .map_err(|error| Refusal::Retry(error.to_string()))?;
    let status = response.status();
    let envelope = parse_envelope(response.headers());
    let body = response
        .bytes()
        .await
        .map_err(|error| Refusal::Retry(error.to_string()))?;
    if status.is_success() {
        return Ok(StageAnswer {
            body: body.to_vec(),
            envelope,
        });
    }
    let message = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| status.to_string());
    Err(if status == StatusCode::GONE {
        Refusal::Final(message)
    } else {
        Refusal::Retry(message)
    })
}

// ── The earlier stage's machine ──────────────────────────────────────────────

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, axum::Json(json!({ "error": error }))).into_response()
}

/// Whether the signed caller may use this machine's stage link: this machine
/// serves `deploy` with plugin `code`, and the caller is a live peer serving
/// the slice that starts just after this one.
async fn authorize(
    state: &GatewayState,
    auth: &crate::auth::InternalNodeAuth,
    code: &str,
    deploy: &str,
) -> Result<String, Response> {
    let Some(principal) = auth.principal() else {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "a signed peer call is required",
        ));
    };
    if !crate::plugin_engine::valid_code(code) || !valid_id(deploy) {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "invalid plugin or deployment",
        ));
    }
    let Some(cfg) = state.runtime.config().await else {
        return Err(refuse(StatusCode::CONFLICT, "this machine serves no model"));
    };
    if cfg.inference_service.as_deref() != Some(code) || cfg.deploy_id != deploy {
        return Err(refuse(
            StatusCode::CONFLICT,
            "this machine does not serve that deployment with that inference service",
        ));
    }
    let next = state.network.alive_peers().await.into_iter().any(|peer| {
        peer.node_id == principal.node_id
            && peer.deploy_id == deploy
            && peer.start_layer == cfg.end_layer + 1
    });
    if !next {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "the caller does not serve the next stage of this deployment",
        ));
    }
    Ok(principal.node_id.clone())
}

async fn engine_link(state: &GatewayState, code: &str) -> Result<Value, Response> {
    state
        .plugins
        .query_audit(code, "engine.link", json!({}))
        .await
        .ok_or_else(|| {
            refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "the plugin is not active on this machine",
            )
        })
}

/// `GET /api/internal/plugins/:code/engine/link/:deploy`: the link secret of
/// this machine's earlier stage, for the machine of the next stage.
pub(crate) async fn link_secret_route(
    auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
    Path((code, deploy)): Path<(String, String)>,
) -> Response {
    if let Err(refusal) = authorize(&state, &auth, &code, &deploy).await {
        return refusal;
    }
    let link = match engine_link(&state, &code).await {
        Ok(link) => link,
        Err(refusal) => return refusal,
    };
    match link["token"].as_str().filter(|token| valid_token(token)) {
        Some(token) => axum::Json(json!({ "token": token })).into_response(),
        None => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "the earlier stage is not loading a slice yet",
        ),
    }
}

/// A loopback connection to this machine's engine: the stream and the number
/// of the next frame it takes (`seq`); `broken` once the engine closed it.
struct LinkIo {
    stream: TcpStream,
    next: u64,
    broken: bool,
}

struct Link {
    io: tokio::sync::Mutex<LinkIo>,
    turn: tokio::sync::Notify,
}

type Connection = Arc<Link>;

/// The connection's stream once frame `seq` is the next one: the frames of a
/// connection are posted together and may arrive in any order.
async fn turn(link: &Link, seq: u64) -> Result<tokio::sync::MutexGuard<'_, LinkIo>, &'static str> {
    loop {
        let notified = link.turn.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let io = link.io.lock().await;
            if io.broken {
                return Err("the earlier stage closed the link");
            }
            if io.next == seq {
                return Ok(io);
            }
            if io.next > seq {
                return Err("a frame of that number was already passed on");
            }
        }
        notified.await;
    }
}

fn connections() -> &'static Mutex<HashMap<(String, String), (Instant, Connection)>> {
    static CONNECTIONS: OnceLock<Mutex<HashMap<(String, String), (Instant, Connection)>>> =
        OnceLock::new();
    CONNECTIONS.get_or_init(Default::default)
}

fn take_connection(key: &(String, String)) -> Option<Connection> {
    let mut map = connections()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    map.get_mut(key).map(|(used, connection)| {
        *used = Instant::now();
        Arc::clone(connection)
    })
}

fn keep_connection(key: (String, String), connection: Connection) {
    let mut map = connections()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    while map.len() >= MAX_CONNECTIONS {
        let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (used, _))| *used)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        map.remove(&oldest);
    }
    map.insert(key, (Instant::now(), connection));
}

fn drop_connection(key: &(String, String)) {
    connections()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(key);
}

/// `POST /api/internal/plugins/:code/engine/link/:deploy/:conn`: one frame of
/// the next stage's engine for this machine's earlier stage, from a node that
/// numbers none; the answer frame is the body. An empty body closes the
/// connection.
pub(crate) async fn link_frame_route(
    auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
    Path((code, deploy, conn)): Path<(String, String, String)>,
    body: Bytes,
) -> Response {
    link_frame(auth, state, code, deploy, conn, None, body).await
}

/// `POST /api/internal/plugins/:code/engine/link/:deploy/:conn/:seq`: frame
/// number `seq` of the connection, passed to the engine after frame `seq - 1`.
pub(crate) async fn link_frame_seq_route(
    auth: crate::auth::InternalNodeAuth,
    State(state): State<GatewayState>,
    Path((code, deploy, conn, seq)): Path<(String, String, String, u64)>,
    body: Bytes,
) -> Response {
    link_frame(auth, state, code, deploy, conn, Some(seq), body).await
}

/// The answer to a frame: this machine's time on it and the links below it, in the envelope header.
fn stage_answer(code: &str, body: Vec<u8>, started: Instant) -> Response {
    let envelope = StageEnvelope {
        served_ms: started.elapsed().as_millis() as u64,
        links: take_downstream_links(code),
    };
    let mut response = (
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        body,
    )
        .into_response();
    if let Some(value) = serde_json::to_string(&envelope)
        .ok()
        .and_then(|text| axum::http::HeaderValue::from_str(&text).ok())
    {
        response.headers_mut().insert(ENVELOPE_HEADER, value);
    }
    response
}

async fn link_frame(
    auth: crate::auth::InternalNodeAuth,
    state: GatewayState,
    code: String,
    deploy: String,
    conn: String,
    seq: Option<u64>,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let caller = match authorize(&state, &auth, &code, &deploy).await {
        Ok(caller) => caller,
        Err(refusal) => return refusal,
    };
    if !valid_id(&conn) {
        return refuse(StatusCode::BAD_REQUEST, "invalid connection");
    }
    if body.len() > MAX_FRAME {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "frame too large");
    }
    let Some(dtype) = state
        .runtime
        .config()
        .await
        .map(|cfg| cfg.hidden_state_dtype)
    else {
        return refuse(StatusCode::CONFLICT, "this machine serves no model");
    };
    let key = (caller, conn);
    if body.is_empty() {
        drop_connection(&key);
        return StatusCode::OK.into_response();
    }
    let connection = match take_connection(&key) {
        Some(connection) => connection,
        // a numbered frame past the first finds no connection: it was closed or dropped, its engine has moved on
        None if seq.is_some_and(|seq| seq > 0) => {
            return refuse(StatusCode::GONE, "the earlier stage's link was reset")
        }
        None => {
            let link = match engine_link(&state, &code).await {
                Ok(link) => link,
                Err(refusal) => return refusal,
            };
            let port = link["port"]
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port > 0);
            let (Some("ready"), Some(port)) = (link["status"].as_str(), port) else {
                return refuse(
                    StatusCode::SERVICE_UNAVAILABLE,
                    link["error"]
                        .as_str()
                        .unwrap_or("the earlier stage is still loading"),
                );
            };
            let stream = match TcpStream::connect(("127.0.0.1", port)).await {
                Ok(stream) => stream,
                Err(error) => {
                    return refuse(
                        StatusCode::SERVICE_UNAVAILABLE,
                        &format!("the earlier stage does not answer: {error}"),
                    )
                }
            };
            let _ = stream.set_nodelay(true);
            let connection = Arc::new(Link {
                io: tokio::sync::Mutex::new(LinkIo {
                    stream,
                    next: 0,
                    broken: false,
                }),
                turn: tokio::sync::Notify::new(),
            });
            keep_connection(key.clone(), Arc::clone(&connection));
            connection
        }
    };
    let mut io = match seq {
        None => connection.io.lock().await,
        Some(seq) => match tokio::time::timeout(FRAME_TIMEOUT, turn(&connection, seq)).await {
            Ok(Ok(io)) => io,
            Ok(Err(error)) => return refuse(StatusCode::GONE, error),
            Err(_) => {
                return refuse(
                    StatusCode::GATEWAY_TIMEOUT,
                    "the frames before this one did not come",
                )
            }
        },
    };
    let frame = match from_network(&body) {
        Ok(frame) => frame,
        Err(error) => {
            drop(io);
            return refuse(StatusCode::BAD_REQUEST, &error);
        }
    };
    // The frame's phase and token count are what this machine's speed is measured on.
    let tokens = frame_phase_and_tokens(&frame);
    // A prompt that starts at position 0 begins a generation: what an earlier one left unreported is dropped, so a
    // generation that ended without its totals cannot be counted in this one.
    if frame.len() >= 10 && frame[0] == KIND_BYTES && frame[1] == b'P' && frame[2..10] == [0u8; 8] {
        let _ = take_link_stats(&code);
        let _ = take_channel_stats(&code);
        let _ = take_own_wait(&code, LinkPhase::Prefill);
        let _ = take_own_wait(&code, LinkPhase::Decode);
    }
    if let Some((phase, _)) = tokens {
        set_phase(&code, phase);
    }
    let waits_before = tokens.map_or(0, |(phase, _)| peek_own_wait(&code, phase));
    let exchanged = tokio::time::timeout(FRAME_TIMEOUT, async {
        write_frame(&mut io.stream, &frame).await?;
        read_frame(&mut io.stream).await
    })
    .await;
    let response = match exchanged {
        Ok(Ok(Some(answer))) => {
            io.next += 1;
            match to_network(answer, dtype) {
                Ok(answer) => {
                    if let Some((phase, count)) = tokens {
                        // This machine's own work on the frame: its time here minus the waits its relays made.
                        let waited = peek_own_wait(&code, phase).saturating_sub(waits_before);
                        let own_ms = started.elapsed().as_millis() as f64 - waited as f64;
                        super::engine_telemetry::record_frame(&code, phase, own_ms, count);
                    }
                    stage_answer(&code, answer, started)
                }
                Err(error) => refuse(StatusCode::BAD_GATEWAY, &error),
            }
        }
        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
            io.broken = true;
            drop_connection(&key);
            refuse(StatusCode::BAD_GATEWAY, "the earlier stage closed the link")
        }
    };
    drop(io);
    connection.turn.notify_waiters();
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_is_a_stage_and_a_whole_model_is_not() {
        let base =
            crate::runtime::NodeConfig::from_params(&crate::webui::RuntimeParams::default(), true);
        let whole = crate::runtime::NodeConfig {
            total_layers: 48,
            start_layer: 0,
            end_layer: 47,
            ..base.clone()
        };
        assert!(!is_stage(&whole));
        let head = crate::runtime::NodeConfig {
            end_layer: 26,
            ..whole.clone()
        };
        let tail = crate::runtime::NodeConfig {
            start_layer: 27,
            ..whole.clone()
        };
        assert!(is_stage(&head) && is_stage(&tail));
    }

    #[test]
    fn ids_and_tokens_are_checked() {
        assert!(valid_id("d-f151dfc9") && valid_id("0123abcd"));
        for bad in ["", "a/b", "a b", "..", &"x".repeat(129)] {
            assert!(!valid_id(bad), "{bad}");
        }
        assert!(valid_token(&"k".repeat(32)));
        assert!(!valid_token("short") && !valid_token(&format!("{} x", "k".repeat(20))));
    }

    #[tokio::test]
    async fn frames_round_trip_with_their_length() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let echo = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(Some(frame)) = read_frame(&mut stream).await {
                write_frame(&mut stream, &frame).await.unwrap();
            }
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        for payload in [b"W\x01\x02".to_vec(), vec![7u8; 100_000]] {
            write_frame(&mut client, &payload).await.unwrap();
            assert_eq!(read_frame(&mut client).await.unwrap().unwrap(), payload);
        }
        drop(client);
        echo.await.unwrap();
    }

    fn tensor_frame(meta: &[u8], values: &[f32]) -> Vec<u8> {
        let mut frame = vec![KIND_TENSOR];
        frame.extend_from_slice(&(meta.len() as u32).to_le_bytes());
        frame.extend_from_slice(meta);
        for value in values {
            frame.extend_from_slice(&value.to_le_bytes());
        }
        frame
    }

    #[test]
    fn a_tensor_crosses_in_the_deployments_precision_and_comes_back_as_f32() {
        let values: Vec<f32> = (0..1024).map(|i| ((i as f32) * 0.37).sin() * 3.0).collect();
        let frame = tensor_frame(&[0], &values);
        for (dtype, tolerance) in [
            (crate::wire::WireDtype::F32, 0.0),
            (crate::wire::WireDtype::F16, 0.01),
            (crate::wire::WireDtype::F8, 0.2),
            (crate::wire::WireDtype::I4G128, 0.3),
        ] {
            let network = to_network(frame.clone(), dtype).unwrap();
            if dtype != crate::wire::WireDtype::F32 {
                assert!(
                    network.len() < frame.len() * 3 / 4,
                    "{dtype:?} is smaller on the wire"
                );
            }
            let back = from_network(&network).unwrap();
            assert_eq!(back.len(), frame.len());
            assert_eq!(
                &back[..6],
                &frame[..6],
                "kind and meta come back as they went"
            );
            for (sent, got) in frame[6..].chunks_exact(4).zip(back[6..].chunks_exact(4)) {
                let sent = f32::from_le_bytes(sent.try_into().unwrap());
                let got = f32::from_le_bytes(got.try_into().unwrap());
                assert!(
                    (sent - got).abs() <= tolerance,
                    "{dtype:?}: {sent} came back as {got}"
                );
            }
        }
    }

    #[test]
    fn opaque_frames_cross_as_they_are_and_f8_falls_back_past_its_range() {
        let opaque = vec![KIND_BYTES, b'P', 1, 2, 3];
        assert_eq!(
            to_network(opaque.clone(), crate::wire::WireDtype::F8).unwrap(),
            opaque
        );
        assert_eq!(from_network(&opaque).unwrap(), opaque);
        let big = tensor_frame(&[0, 7], &[1000.0, -2.0]);
        let back =
            from_network(&to_network(big.clone(), crate::wire::WireDtype::F8).unwrap()).unwrap();
        assert_eq!(
            back, big,
            "a value past f8's range crosses that once as f32"
        );
        assert!(to_network(vec![b'P', 0], crate::wire::WireDtype::F16).is_err());
        assert!(to_network(
            vec![KIND_TENSOR, 9, 0, 0, 0, 1],
            crate::wire::WireDtype::F16
        )
        .is_err());
    }

    #[tokio::test]
    async fn numbered_frames_take_their_turn() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let link = Arc::new(Link {
            io: tokio::sync::Mutex::new(LinkIo {
                stream,
                next: 0,
                broken: false,
            }),
            turn: tokio::sync::Notify::new(),
        });
        let second = {
            let link = Arc::clone(&link);
            tokio::spawn(async move { turn(&link, 1).await.map(|io| io.next) })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!second.is_finished(), "frame 1 waits for frame 0");
        {
            let mut io = turn(&link, 0).await.unwrap();
            io.next += 1;
        }
        link.turn.notify_waiters();
        assert_eq!(second.await.unwrap(), Ok(1));
        assert!(
            turn(&link, 0).await.is_err(),
            "frame 0 was already passed on"
        );
        link.io.lock().await.broken = true;
        assert!(
            turn(&link, 1).await.is_err(),
            "a broken link takes no frame"
        );
    }
}

#[cfg(test)]
mod link_stats_tests {
    use super::*;

    #[test]
    fn link_totals_are_taken_once_per_plugin_and_node() {
        record_link("strata-link-test-a", "node-1", Duration::from_millis(40), 100, 200, 25, "quic");
        record_link("strata-link-test-a", "node-1", Duration::from_millis(10), 50, 60, 4, "quic");
        record_link("strata-link-test-b", "node-1", Duration::from_millis(5), 1, 1, 0, "http");
        assert_eq!(
            take_link_stats("strata-link-test-a"),
            vec![(
                "node-1".to_string(),
                LinkPhase::Prefill,
                LinkStats {
                    calls: 2,
                    round_trip_ms: 50,
                    max_round_trip_ms: 40,
                    served_ms: 29,
                    network_ms: 21,
                    bytes_sent: 150,
                    bytes_received: 260,
                },
            )]
        );
        assert!(take_link_stats("strata-link-test-a").is_empty());
        assert_eq!(take_link_stats("strata-link-test-b").len(), 1);
    }

    #[test]
    fn downstream_totals_are_filed_in_the_current_phase() {
        set_phase("strata-link-test-c", LinkPhase::Decode);
        merge_downstream(
            "strata-link-test-c",
            "node-2",
            LinkStats {
                calls: 1,
                round_trip_ms: 9,
                ..Default::default()
            },
            &[],
        );
        let taken = take_link_stats("strata-link-test-c");
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].1, LinkPhase::Decode);
        assert_eq!(taken[0].2.round_trip_ms, 9);
    }

    #[test]
    fn the_envelope_survives_the_header() {
        let envelope = StageEnvelope {
            served_ms: 7,
            links: vec![StageLinkWire {
                node: "node-3".to_string(),
                stats: LinkStats {
                    calls: 1,
                    round_trip_ms: 12,
                    ..Default::default()
                },
                channels: Vec::new(),
            }],
        };
        let text = serde_json::to_string(&envelope).unwrap();
        let back: StageEnvelope = serde_json::from_str(&text).unwrap();
        assert_eq!(back.served_ms, 7);
        assert_eq!(back.links[0].node, "node-3");
        assert_eq!(back.links[0].stats.round_trip_ms, 12);
    }

    #[test]
    fn channels_are_kept_apart_and_aggregated() {
        record_link("strata-chan-test", "node-7", Duration::from_millis(20), 10, 10, 5, "quic");
        record_link("strata-chan-test", "node-7", Duration::from_millis(30), 10, 10, 5, "quic");
        record_link("strata-chan-test", "node-7", Duration::from_millis(9), 4, 4, 1, "tunnel");
        let mut taken = take_channel_stats("strata-chan-test");
        taken.sort_by(|a, b| a.2.channel.cmp(&b.2.channel));
        assert_eq!(taken.len(), 2);
        let quic = &taken[0].2;
        assert_eq!(quic.channel, "quic");
        assert_eq!(quic.calls, 2);
        assert_eq!(quic.round_trip_sum_ms, 50);
        assert_eq!(quic.round_trip_min_ms, 20);
        assert_eq!(quic.round_trip_max_ms, 30);
        assert_eq!(quic.network_sum_ms, 40);
        assert_eq!(quic.bytes_sent, 20);
        assert_eq!(taken[1].2.channel, "tunnel");
        assert_eq!(taken[1].2.calls, 1);
    }

    #[test]
    fn a_stage_request_tells_its_phase_and_token_count() {
        let mut prompt = vec![KIND_BYTES, b'P'];
        prompt.extend_from_slice(&0i64.to_le_bytes());
        prompt.extend_from_slice(&5u32.to_le_bytes());
        assert_eq!(frame_phase_and_tokens(&prompt), Some((LinkPhase::Prefill, 5)));
        let mut window = vec![KIND_BYTES, b'W'];
        window.extend_from_slice(&7i64.to_le_bytes());
        window.extend_from_slice(&3u32.to_le_bytes());
        assert_eq!(frame_phase_and_tokens(&window), Some((LinkPhase::Decode, 3)));
        let mut commit = vec![KIND_BYTES, b'C'];
        commit.extend_from_slice(&2u32.to_le_bytes());
        assert_eq!(frame_phase_and_tokens(&commit), None, "a commit carries no tokens of its own");
        assert_eq!(frame_phase_and_tokens(&[KIND_BYTES, b'P']), None);
    }
}
