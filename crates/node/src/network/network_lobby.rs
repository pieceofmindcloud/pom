//! Lobby: the room a public network opens to machines that are not members.
//!
//! A candidate talks to the network's principal before joining, so the admin
//! can decide on admission. The candidate is not in the cluster: it has no API
//! key, no gossip announce and no admission. What it has is its node key, and
//! the Noise handshake already proves it - the principal derives the
//! candidate's `node_id` from the transport `PeerId` and never from the frame.
//!
//! The protocol is pull only. A candidate posts a text or fetches the recent
//! history, and every request carries its hardware card; the principal never
//! opens a stream towards a candidate and keeps no session beyond a presence
//! entry that expires. Inside the network the lobby is the `lobby` room of the
//! network Chat: the principal turns a candidate's text into an ordinary
//! relayed `ChatAction`, and members answer through the usual authenticated
//! star.
//!
//! [`LobbyClient`] is the candidate's side. It is a small swarm of its own
//! (dial only, relay client and DCuTR) so it also runs in setup mode, where
//! the node has no cluster swarm yet.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::prelude::*;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{
    dcutr, identify, noise, relay, request_response, tcp, yamux, Multiaddr, PeerId, StreamProtocol,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

pub const NETWORK_LOBBY_PROTOCOL: &str = "/pom/network-lobby/1.0.0";
/// Room id of the lobby inside the network Chat.
pub const LOBBY_ROOM: &str = "lobby";

/// Bounds of the lobby. They are much tighter than the network Chat's because
/// the sender is not a member: the frame is accepted before any credential.
pub struct LobbyLimits;

impl LobbyLimits {
    pub const FRAME_BYTES: usize = 64 * 1024;
    /// Bound of an answer. Larger than a request's because a directory answer
    /// carries whole network descriptions, trust document included; it is
    /// read by the side that asked, from a peer it chose to dial.
    pub const RESPONSE_BYTES: usize = 2 * 1024 * 1024;
    /// Networks in one directory answer.
    pub const DIRECTORY_NETWORKS: usize = 64;
    pub const TEXT_BYTES: usize = 2 * 1024;
    pub const MESSAGE_ID_BYTES: usize = 64;
    pub const CARD_TEXT_BYTES: usize = 96;
    pub const CARD_GPUS: usize = 16;
    /// Messages returned to a candidate on every request.
    pub const HISTORY: usize = 50;
    /// Texts one candidate may post per [`Self::POST_WINDOW`].
    pub const POSTS_PER_WINDOW: u32 = 6;
    pub const POST_WINDOW: Duration = Duration::from_secs(30);
    /// Requests (posts and fetches) one candidate may send per second.
    pub const REQUESTS_PER_SECOND: u32 = 4;
    /// Candidates tracked at once; a new one beyond this is refused.
    pub const GUESTS: usize = 64;
    /// Model names published in the network's public stats.
    pub const INFO_MODELS: usize = 64;
    /// `Info` requests one machine may send per [`Self::INFO_WINDOW`].
    pub const INFO_PER_WINDOW: u32 = 6;
    pub const INFO_WINDOW: Duration = Duration::from_secs(10);
    /// Machines whose `Info` rate is tracked at once.
    pub const INFO_CALLERS: usize = 4096;
}

/// How long a candidate stays listed after its last request.
pub const GUEST_TTL: Duration = Duration::from_secs(90);
/// How often the principal re-announces a present candidate to the members.
pub const GUEST_REANNOUNCE: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LobbyGpu {
    pub name: String,
    /// `None` on unified-memory machines, where dedicated VRAM does not apply.
    #[serde(default)]
    pub vram_total_mb: Option<u64>,
}

/// The hardware a candidate shows to the network. It is self-declared: the
/// principal bounds its size and nothing else, and the screen says so.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LobbyCard {
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub cpu_count: u32,
    #[serde(default)]
    pub ram_total_mb: u64,
    #[serde(default)]
    pub gpus: Vec<LobbyGpu>,
}

fn bounded(text: &str) -> String {
    let text = text.trim();
    if text.len() <= LobbyLimits::CARD_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = LobbyLimits::CARD_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

impl LobbyCard {
    /// The card of this machine, from the same sample the Dashboard shows.
    pub fn from_system(system: &crate::sysmon::SystemLoad) -> Self {
        Self {
            hostname: system.hostname.clone(),
            os: system.os.to_string(),
            arch: system.arch.to_string(),
            cpu_count: system.cpu_count as u32,
            ram_total_mb: system.ram.total_mb.max(0.0) as u64,
            gpus: system
                .gpus
                .iter()
                .map(|gpu| LobbyGpu {
                    name: gpu.name.clone(),
                    vram_total_mb: gpu.vram_total_mb.map(|mb| mb.max(0.0) as u64),
                })
                .collect(),
        }
        .sanitized()
    }

    /// Clamps every field. Applied on both ends: a card from the wire is never
    /// stored or relayed as received.
    pub fn sanitized(mut self) -> Self {
        self.hostname = bounded(&self.hostname);
        self.os = bounded(&self.os);
        self.arch = bounded(&self.arch);
        self.gpus.truncate(LobbyLimits::CARD_GPUS);
        for gpu in &mut self.gpus {
            gpu.name = bounded(&gpu.name);
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LobbyRequest {
    /// Say something in the lobby.
    Post {
        card: LobbyCard,
        message_id: String,
        text: String,
    },
    /// Read the recent history. Also what keeps the candidate listed.
    Fetch { card: LobbyCard },
    /// Ask what the network publishes about itself (`NetworkInfo`). It is not
    /// part of the conversation: no card, no presence, and it is answered
    /// under its own switch, with the lobby chat on or off.
    Info,
    /// Ask a node which public network it belongs to. Answers what its
    /// unauthenticated `GET /api/public/network` answers, over the keyed
    /// connection, so it also reaches a node behind NAT.
    Describe,
    /// Ask a node which public networks it knows. Answered by nodes that
    /// serve as a relay or belong to a public network: both hear the
    /// announces of every cluster that meets on them, which is what a machine
    /// that has joined nothing lacks (`DirectoryBoard`).
    Directory,
}

/// Relay keys (`pk:...`) every installation asks for the public networks, in
/// addition to `POM_DIRECTORY_RELAYS` and to the relays already configured.
/// A key here must parse as a pkarr key: `default_directory_relays_are_keys`.
pub const DEFAULT_DIRECTORY_RELAYS: &[&str] =
    &["pk:kyqrhnypyqfu7ic43ej1i7a5kjmyh8nxxrpumzmxjqk6dty4xq6o"];

/// Keys from [`DEFAULT_DIRECTORY_RELAYS`] and the `DIRECTORY_RELAYS`
/// environment variable (comma separated), without repeats.
pub fn default_directory_relays() -> Vec<String> {
    let mut keys: Vec<String> = DEFAULT_DIRECTORY_RELAYS
        .iter()
        .map(|key| key.to_string())
        .collect();
    if let Ok(value) = crate::brand::env("DIRECTORY_RELAYS") {
        keys.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(str::to_string),
        );
    }
    let mut seen = std::collections::HashSet::new();
    keys.retain(|key| seen.insert(key.clone()));
    keys
}

/// What a public network chose to publish about itself: the same three facts
/// the Public networks screen shows for the local network.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkInfo {
    /// Models known to the cluster, sorted.
    pub models: Vec<String>,
    pub node_count: u32,
    /// Sum of the dedicated VRAM the nodes report; `None` when none reports.
    #[serde(default)]
    pub vram_total_mb: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LobbyMessage {
    pub message_id: String,
    pub sender_node_id: String,
    /// Hostname as the principal knows it; empty when it does not.
    pub sender_name: String,
    /// `true` when the sender is a member of the network, not a candidate.
    pub member: bool,
    pub text: String,
    pub sent_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LobbyResponse {
    pub accepted: bool,
    #[serde(default)]
    pub error: Option<String>,
    /// The `node_id` the principal derived for the caller.
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub messages: Vec<LobbyMessage>,
    /// Only in the answer to `Info`. `None` with `accepted: true` means the
    /// principal has not computed it yet; asking again shortly gets it.
    #[serde(default)]
    pub info: Option<NetworkInfo>,
    /// The answer to `Describe` (zero or one entry) and to `Directory`. What
    /// arrives here is a claim: the asker checks each entry against its own
    /// trust document (`gateway::validated_listed_network`).
    #[serde(default)]
    pub networks: Vec<crate::gateway::PublicNetworkView>,
    /// `Describe` and `Directory` are answered from a board filled on demand;
    /// `true` means it is being filled and asking again shortly gets it.
    #[serde(default)]
    pub pending: bool,
}

impl LobbyResponse {
    pub fn rejected(error: impl Into<String>) -> Self {
        Self {
            accepted: false,
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

/// `node_id` of the peer on the other end of an authenticated connection:
/// `SHA256(pubkey)` of the Ed25519 key inside its `PeerId`.
pub fn node_id_of_peer(peer: &PeerId) -> Option<String> {
    let multihash = peer.as_ref();
    // Code 0x00 = identity: the digest is the protobuf-encoded public key.
    if multihash.code() != 0x00 {
        return None;
    }
    let public = libp2p::identity::PublicKey::try_decode_protobuf(multihash.digest()).ok()?;
    let ed25519 = public.try_into_ed25519().ok()?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&ed25519.to_bytes()).ok()?;
    Some(crate::identity::NodeId::from_pubkey(&key).0)
}

// ── Principal side: presence and rate ────────────────────────────────────────

struct Guest {
    card: LobbyCard,
    seen: Instant,
    announced: Instant,
    second_started: Instant,
    second_count: u32,
    window_started: Instant,
    window_posts: u32,
}

/// What the principal should do after seeing a candidate's request.
#[derive(Debug, PartialEq, Eq)]
pub enum GuestSeen {
    /// Known and recently announced: nothing to tell the members.
    Known,
    /// New, changed or due: relay its card to the members.
    Announce,
}

/// Presence of the candidates in the lobby. On the principal it is fed by the
/// requests themselves; on a member it is fed by the relayed presence frames.
#[derive(Default)]
pub struct LobbyGuests {
    guests: Mutex<HashMap<String, Guest>>,
}

impl LobbyGuests {
    /// Records one request of `node_id`, applying the request rate. `post`
    /// also counts against the text rate.
    pub fn admit(
        &self,
        node_id: &str,
        card: &LobbyCard,
        post: bool,
    ) -> std::result::Result<GuestSeen, &'static str> {
        let now = Instant::now();
        let mut guests = self.guests.lock().expect("lobby guests lock");
        guests.retain(|_, guest| now.duration_since(guest.seen) < GUEST_TTL);
        if !guests.contains_key(node_id) {
            if guests.len() >= LobbyLimits::GUESTS {
                return Err("lobby_full");
            }
            guests.insert(
                node_id.to_string(),
                Guest {
                    card: card.clone(),
                    seen: now,
                    announced: now,
                    second_started: now,
                    second_count: 1,
                    window_started: now,
                    window_posts: u32::from(post),
                },
            );
            return Ok(GuestSeen::Announce);
        }
        let guest = guests.get_mut(node_id).expect("guest checked above");
        if now.duration_since(guest.second_started) >= Duration::from_secs(1) {
            guest.second_started = now;
            guest.second_count = 0;
        }
        if guest.second_count >= LobbyLimits::REQUESTS_PER_SECOND {
            return Err("rate_limited");
        }
        guest.second_count += 1;
        if post {
            if now.duration_since(guest.window_started) >= LobbyLimits::POST_WINDOW {
                guest.window_started = now;
                guest.window_posts = 0;
            }
            if guest.window_posts >= LobbyLimits::POSTS_PER_WINDOW {
                return Err("rate_limited");
            }
            guest.window_posts += 1;
        }
        guest.seen = now;
        let changed = guest.card != *card;
        if changed || now.duration_since(guest.announced) >= GUEST_REANNOUNCE {
            guest.card = card.clone();
            guest.announced = now;
            return Ok(GuestSeen::Announce);
        }
        Ok(GuestSeen::Known)
    }

    /// Records a presence relayed by the principal (member side).
    pub fn remember(&self, node_id: &str, card: LobbyCard) {
        let now = Instant::now();
        let mut guests = self.guests.lock().expect("lobby guests lock");
        guests.retain(|_, guest| now.duration_since(guest.seen) < GUEST_TTL);
        if !guests.contains_key(node_id) && guests.len() >= LobbyLimits::GUESTS {
            return;
        }
        guests.insert(
            node_id.to_string(),
            Guest {
                card,
                seen: now,
                announced: now,
                second_started: now,
                second_count: 0,
                window_started: now,
                window_posts: 0,
            },
        );
    }

    /// Candidates seen within [`GUEST_TTL`], ordered by `node_id`.
    pub fn snapshot(&self) -> Vec<(String, LobbyCard)> {
        let now = Instant::now();
        let mut guests = self.guests.lock().expect("lobby guests lock");
        guests.retain(|_, guest| now.duration_since(guest.seen) < GUEST_TTL);
        let mut listed: Vec<(String, LobbyCard)> = guests
            .iter()
            .map(|(node_id, guest)| (node_id.clone(), guest.card.clone()))
            .collect();
        listed.sort_by(|left, right| left.0.cmp(&right.0));
        listed
    }

    pub fn hostname(&self, node_id: &str) -> Option<String> {
        self.guests
            .lock()
            .expect("lobby guests lock")
            .get(node_id)
            .map(|guest| guest.card.hostname.clone())
    }
}

// ── Principal side: published stats ──────────────────────────────────────────

/// How old the published stats may get before a request asks for a refresh.
pub const INFO_REFRESH: Duration = Duration::from_secs(30);
/// Past this age the stats are not served at all.
pub const INFO_MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// The stats a principal publishes, and who is asking for them.
///
/// The swarm loop answers from this slot and never computes: the numbers come
/// from the cluster view, which reads every peer over HTTP. A request that
/// finds the slot stale wakes the gateway task that fills it
/// (`webui::spawn_public_stats_publisher`), so nothing is computed while
/// nobody asks.
#[derive(Default)]
pub struct PublicStats {
    slot: Mutex<Option<(Instant, NetworkInfo)>>,
    callers: Mutex<HashMap<String, (Instant, u32)>>,
    wanted: tokio::sync::Notify,
}

impl PublicStats {
    /// Applies the per-machine rate of `Info`.
    pub fn allow(&self, node_id: &str) -> bool {
        let now = Instant::now();
        let mut callers = self.callers.lock().expect("public stats callers lock");
        if callers.len() >= LobbyLimits::INFO_CALLERS {
            callers.retain(|_, (started, _)| {
                now.duration_since(*started) < LobbyLimits::INFO_WINDOW
            });
            if callers.len() >= LobbyLimits::INFO_CALLERS {
                return false;
            }
        }
        let entry = callers.entry(node_id.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= LobbyLimits::INFO_WINDOW {
            *entry = (now, 0);
        }
        if entry.1 >= LobbyLimits::INFO_PER_WINDOW {
            return false;
        }
        entry.1 += 1;
        true
    }

    /// The stats to serve now, asking for a refresh when they are stale.
    pub fn current(&self) -> Option<NetworkInfo> {
        let slot = self.slot.lock().expect("public stats slot lock").clone();
        let age = slot.as_ref().map(|(at, _)| at.elapsed());
        if age.is_none_or(|age| age >= INFO_REFRESH) {
            self.wanted.notify_one();
        }
        slot.filter(|(at, _)| at.elapsed() < INFO_MAX_AGE)
            .map(|(_, info)| info)
    }

    pub fn publish(&self, mut info: NetworkInfo) {
        info.models.truncate(LobbyLimits::INFO_MODELS);
        *self.slot.lock().expect("public stats slot lock") = Some((Instant::now(), info));
    }

    /// Drops what was published, when the operator turns the switch off.
    pub fn clear(&self) {
        *self.slot.lock().expect("public stats slot lock") = None;
    }

    /// Resolves when a request found the stats missing or stale.
    pub async fn wanted(&self) {
        self.wanted.notified().await;
    }
}

// ── Any node: what it tells about public networks ────────────────────────────

type Listed = Vec<crate::gateway::PublicNetworkView>;

/// What a node answers to `Describe` and `Directory`.
///
/// Like [`PublicStats`], the swarm loop only reads it: the listing resolves
/// pkarr records and talks to other nodes. A request that finds a slot
/// missing or stale wakes `gateway::spawn_directory_board_publisher`.
#[derive(Default)]
pub struct DirectoryBoard {
    /// This node's own public network, if it is in one.
    own: Mutex<Option<(Instant, Listed)>>,
    /// The networks this node knows; inner `None` when it does not answer
    /// directory queries (neither a relay nor in a public network).
    known: Mutex<Option<(Instant, Option<Listed>)>>,
    callers: Mutex<HashMap<String, (Instant, u32)>>,
    wanted: tokio::sync::Notify,
}

/// What the board has for one request.
#[derive(Debug, PartialEq)]
pub enum BoardAnswer {
    Ready(Listed),
    /// Not filled yet (or too old to serve); a refresh was requested.
    Pending,
    /// This node does not answer that question.
    Disabled,
}

impl DirectoryBoard {
    /// Per-machine rate, shared by `Describe` and `Directory`.
    pub fn allow(&self, node_id: &str) -> bool {
        let now = Instant::now();
        let mut callers = self.callers.lock().expect("directory board callers lock");
        if callers.len() >= LobbyLimits::INFO_CALLERS {
            callers.retain(|_, (started, _)| {
                now.duration_since(*started) < LobbyLimits::INFO_WINDOW
            });
            if callers.len() >= LobbyLimits::INFO_CALLERS {
                return false;
            }
        }
        let entry = callers.entry(node_id.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= LobbyLimits::INFO_WINDOW {
            *entry = (now, 0);
        }
        if entry.1 >= LobbyLimits::INFO_PER_WINDOW {
            return false;
        }
        entry.1 += 1;
        true
    }

    fn stale(at: Option<Instant>) -> bool {
        at.is_none_or(|at| at.elapsed() >= INFO_REFRESH)
    }

    pub fn own(&self) -> BoardAnswer {
        let slot = self.own.lock().expect("directory board own lock").clone();
        if Self::stale(slot.as_ref().map(|(at, _)| *at)) {
            self.wanted.notify_one();
        }
        match slot {
            Some((at, listed)) if at.elapsed() < INFO_MAX_AGE => BoardAnswer::Ready(listed),
            _ => BoardAnswer::Pending,
        }
    }

    pub fn known(&self) -> BoardAnswer {
        let slot = self.known.lock().expect("directory board known lock").clone();
        if Self::stale(slot.as_ref().map(|(at, _)| *at)) {
            self.wanted.notify_one();
        }
        match slot {
            Some((at, Some(listed))) if at.elapsed() < INFO_MAX_AGE => BoardAnswer::Ready(listed),
            Some((at, None)) if at.elapsed() < INFO_MAX_AGE => BoardAnswer::Disabled,
            _ => BoardAnswer::Pending,
        }
    }

    /// Stores a fresh board. `known` is `None` when this node does not answer
    /// directory queries. The list is cut to what one answer can carry.
    pub fn publish(&self, own: Listed, known: Option<Listed>) {
        let now = Instant::now();
        *self.own.lock().expect("directory board own lock") = Some((now, own));
        *self.known.lock().expect("directory board known lock") =
            Some((now, known.map(fit_directory_answer)));
    }

    /// Resolves when a request found the board missing or stale.
    pub async fn wanted(&self) {
        self.wanted.notified().await;
    }
}

/// Keeps as many networks as fit one answer frame, in the order given.
fn fit_directory_answer(mut listed: Listed) -> Listed {
    listed.truncate(LobbyLimits::DIRECTORY_NETWORKS);
    // Leave room for the envelope around the list.
    let budget = LobbyLimits::RESPONSE_BYTES - 64 * 1024;
    let mut used = 0_usize;
    let mut kept = 0_usize;
    for network in &listed {
        used += serde_json::to_vec(network).map(|bytes| bytes.len()).unwrap_or(budget);
        if used > budget {
            break;
        }
        kept += 1;
    }
    listed.truncate(kept);
    listed
}

// ── Codec ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct LobbyCodec;

async fn read_msg<T, R>(io: &mut R, bound: usize) -> std::io::Result<T>
where
    T: serde::de::DeserializeOwned,
    R: AsyncRead + Unpin + Send,
{
    let mut len_buf = [0_u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > bound {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "lobby frame exceeds the size bound",
        ));
    }
    let mut data = vec![0_u8; len];
    io.read_exact(&mut data).await?;
    serde_json::from_slice(&data)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))
}

async fn write_msg<T, W>(io: &mut W, message: &T, bound: usize) -> std::io::Result<()>
where
    T: Serialize,
    W: AsyncWrite + Unpin + Send,
{
    let data = serde_json::to_vec(message)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    if data.len() > bound {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "lobby frame exceeds the size bound",
        ));
    }
    io.write_all(&(data.len() as u32).to_be_bytes()).await?;
    io.write_all(&data).await
}

#[async_trait]
impl request_response::Codec for LobbyCodec {
    type Protocol = StreamProtocol;
    type Request = LobbyRequest;
    type Response = LobbyResponse;

    async fn read_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io, LobbyLimits::FRAME_BYTES).await
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io, LobbyLimits::RESPONSE_BYTES).await
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        request: Self::Request,
    ) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &request, LobbyLimits::FRAME_BYTES).await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        response: Self::Response,
    ) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &response, LobbyLimits::RESPONSE_BYTES).await
    }
}

/// The lobby on a cluster swarm: it only answers, it never asks.
pub fn network_lobby_behaviour() -> request_response::Behaviour<LobbyCodec> {
    request_response::Behaviour::new(
        [(
            StreamProtocol::new(NETWORK_LOBBY_PROTOCOL),
            request_response::ProtocolSupport::Inbound,
        )],
        request_response::Config::default().with_request_timeout(Duration::from_secs(10)),
    )
}

// ── Candidate side ───────────────────────────────────────────────────────────

/// How long a resolved publisher address is reused before asking pkarr again.
const DIAL_CACHE_TTL: Duration = Duration::from_secs(60);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(NetworkBehaviour)]
struct LobbyClientBehaviour {
    relay: relay::client::Behaviour,
    identify: identify::Behaviour,
    dcutr: dcutr::Behaviour,
    lobby: request_response::Behaviour<LobbyCodec>,
}

struct ClientCommand {
    peer_id: PeerId,
    addresses: Vec<Multiaddr>,
    request: LobbyRequest,
    reply: oneshot::Sender<std::result::Result<LobbyResponse, String>>,
}

/// Dial-only libp2p client that reaches a network's principal by its key.
pub struct LobbyClient {
    commands: mpsc::Sender<ClientCommand>,
    pkarr: crate::pkarr_node::PkarrNode,
    dial_cache: Mutex<HashMap<String, (Instant, Vec<Multiaddr>)>>,
    node_id: String,
}

impl LobbyClient {
    /// Builds the swarm over the node identity and starts its task. It
    /// listens on nothing: the same key may already be listening in the
    /// cluster swarm, and a candidate only ever dials.
    pub fn start(identity: &crate::identity::Identity) -> Result<Arc<Self>> {
        let pkarr = crate::pkarr_node::PkarrNode::from_signing_key(&identity.signing_key)?;
        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(identity.libp2p_keypair())
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_quic()
            .with_dns()?
            .with_relay_client(noise::Config::new, yamux::Config::default)?
            .with_behaviour(|key, relay| LobbyClientBehaviour {
                relay,
                identify: identify::Behaviour::new(
                    identify::Config::new("/pom/1.0.0".to_string(), key.public())
                        .with_cache_size(0),
                ),
                dcutr: dcutr::Behaviour::new(key.public().to_peer_id()),
                lobby: request_response::Behaviour::new(
                    [(
                        StreamProtocol::new(NETWORK_LOBBY_PROTOCOL),
                        request_response::ProtocolSupport::Outbound,
                    )],
                    request_response::Config::default()
                        .with_request_timeout(Duration::from_secs(15)),
                ),
            })?
            .with_swarm_config(|config| {
                config.with_idle_connection_timeout(Duration::from_secs(60))
            })
            .build();

        let (commands, mut receiver) = mpsc::channel::<ClientCommand>(16);
        tokio::spawn(async move {
            let mut pending: HashMap<
                request_response::OutboundRequestId,
                oneshot::Sender<std::result::Result<LobbyResponse, String>>,
            > = HashMap::new();
            loop {
                tokio::select! {
                    command = receiver.recv() => {
                        let Some(command) = command else { break };
                        for address in command.addresses {
                            swarm.add_peer_address(command.peer_id, address);
                        }
                        let id = swarm
                            .behaviour_mut()
                            .lobby
                            .send_request(&command.peer_id, command.request);
                        pending.insert(id, command.reply);
                    }
                    event = swarm.select_next_some() => {
                        let SwarmEvent::Behaviour(LobbyClientBehaviourEvent::Lobby(event)) = event
                        else {
                            continue;
                        };
                        match event {
                            request_response::Event::Message {
                                message: request_response::Message::Response { request_id, response },
                                ..
                            } => {
                                if let Some(reply) = pending.remove(&request_id) {
                                    let _ = reply.send(Ok(response));
                                }
                            }
                            request_response::Event::OutboundFailure { request_id, error, .. } => {
                                if let Some(reply) = pending.remove(&request_id) {
                                    let _ = reply.send(Err(error.to_string()));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            commands,
            pkarr,
            dial_cache: Mutex::new(HashMap::new()),
            node_id: identity.node_id.0.clone(),
        }))
    }

    /// This machine's `node_id`, so the screen can tell its own messages.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    async fn dial_addresses(&self, key: &str) -> Vec<Multiaddr> {
        if let Some((at, addresses)) = self
            .dial_cache
            .lock()
            .expect("lobby dial cache lock")
            .get(key)
            .cloned()
        {
            if at.elapsed() < DIAL_CACHE_TTL {
                return addresses;
            }
        }
        let addresses = tokio::time::timeout(RESOLVE_TIMEOUT, self.pkarr.resolve_dial_addrs(key))
            .await
            .unwrap_or_default();
        if !addresses.is_empty() {
            self.dial_cache
                .lock()
                .expect("lobby dial cache lock")
                .insert(key.to_string(), (Instant::now(), addresses.clone()));
        }
        addresses
    }

    /// Sends one request to the principal whose key is `key` (`pk:...`). The
    /// address is resolved through pkarr at dial time and never stored.
    pub async fn request(&self, key: &str, request: LobbyRequest) -> Result<LobbyResponse> {
        let peer_id = crate::pkarr_node::peer_id_from_uri(key)
            .context("a chave do publicador é inválida")?;
        let addresses = self.dial_addresses(key).await;
        if addresses.is_empty() {
            anyhow::bail!("publisher_unresolved");
        }
        let result = self.request_at(peer_id, addresses, request).await;
        if matches!(&result, Err(error) if error.to_string().starts_with("publisher_unreachable")) {
            // A stale address is the usual cause; resolve again next time.
            self.dial_cache
                .lock()
                .expect("lobby dial cache lock")
                .remove(key);
        }
        result
    }

    /// Sends one request to `peer_id` at already resolved addresses.
    async fn request_at(
        &self,
        peer_id: PeerId,
        addresses: Vec<Multiaddr>,
        request: LobbyRequest,
    ) -> Result<LobbyResponse> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .try_send(ClientCommand {
                peer_id,
                addresses,
                request,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("lobby_busy"))?;
        match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
            Ok(Ok(Ok(response))) => Ok(response),
            Ok(Ok(Err(error))) => Err(anyhow::anyhow!("publisher_unreachable: {error}")),
            _ => Err(anyhow::anyhow!("publisher_timeout")),
        }
    }
}

static CLIENT: tokio::sync::OnceCell<Arc<LobbyClient>> = tokio::sync::OnceCell::const_new();

/// The process-wide candidate client, started on first use. It is the same in
/// setup mode and on a configured node: both own the node key and nothing else
/// is needed.
pub async fn client() -> Result<Arc<LobbyClient>> {
    CLIENT
        .get_or_try_init(|| async {
            let identity = crate::identity::Identity::load_or_create(&crate::identity::key_path());
            LobbyClient::start(&identity)
        })
        .await
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(hostname: &str) -> LobbyCard {
        LobbyCard {
            hostname: hostname.into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            cpu_count: 16,
            ram_total_mb: 64_000,
            gpus: vec![LobbyGpu {
                name: "RTX 5060 Ti".into(),
                vram_total_mb: Some(16_000),
            }],
        }
    }

    #[test]
    fn a_card_from_the_wire_is_clamped() {
        let mut oversized = card(&"h".repeat(4_000));
        oversized.gpus = (0..200)
            .map(|index| LobbyGpu {
                name: format!("gpu {index} {}", "x".repeat(500)),
                vram_total_mb: None,
            })
            .collect();
        let clean = oversized.sanitized();
        assert_eq!(clean.hostname.len(), LobbyLimits::CARD_TEXT_BYTES);
        assert_eq!(clean.gpus.len(), LobbyLimits::CARD_GPUS);
        assert!(clean
            .gpus
            .iter()
            .all(|gpu| gpu.name.len() <= LobbyLimits::CARD_TEXT_BYTES));
        // A multi-byte boundary never splits a character.
        let accented = card(&"é".repeat(200)).sanitized();
        assert!(accented.hostname.len() <= LobbyLimits::CARD_TEXT_BYTES);
        assert!(accented.hostname.chars().all(|c| c == 'é'));
    }

    #[test]
    fn presence_announces_once_then_only_on_change() {
        let guests = LobbyGuests::default();
        assert_eq!(guests.admit("a", &card("alpha"), false), Ok(GuestSeen::Announce));
        assert_eq!(guests.admit("a", &card("alpha"), false), Ok(GuestSeen::Known));
        assert_eq!(guests.admit("a", &card("renamed"), false), Ok(GuestSeen::Announce));
        assert_eq!(guests.hostname("a").as_deref(), Some("renamed"));
        assert_eq!(guests.snapshot().len(), 1);
    }

    #[test]
    fn a_candidate_is_rate_limited_per_second_and_per_post_window() {
        let guests = LobbyGuests::default();
        let card = card("alpha");
        // The first request of a new candidate counts as one.
        for _ in 0..LobbyLimits::REQUESTS_PER_SECOND {
            assert!(guests.admit("burst", &card, false).is_ok());
        }
        assert_eq!(guests.admit("burst", &card, false), Err("rate_limited"));

        // Posts have their own, slower window; spread them past the per-second cap.
        let guests = LobbyGuests::default();
        for _ in 0..LobbyLimits::POSTS_PER_WINDOW {
            assert!(guests.admit("talker", &card, true).is_ok());
            guests
                .guests
                .lock()
                .unwrap()
                .get_mut("talker")
                .unwrap()
                .second_count = 0;
        }
        assert_eq!(guests.admit("talker", &card, true), Err("rate_limited"));
        // Reading is still allowed while the text window is exhausted.
        guests
            .guests
            .lock()
            .unwrap()
            .get_mut("talker")
            .unwrap()
            .second_count = 0;
        assert!(guests.admit("talker", &card, false).is_ok());
    }

    #[test]
    fn the_lobby_refuses_candidates_beyond_its_capacity() {
        let guests = LobbyGuests::default();
        for index in 0..LobbyLimits::GUESTS {
            assert!(guests.admit(&format!("n{index}"), &card("x"), false).is_ok());
        }
        assert_eq!(guests.admit("one-more", &card("x"), false), Err("lobby_full"));
    }

    /// The candidate's swarm against a swarm that only speaks the lobby
    /// protocol, over loopback TCP: the dial, the codec and the identity the
    /// answering side derives from the transport.
    #[tokio::test]
    async fn a_candidate_reaches_a_principal_and_is_named_by_its_transport_key() {
        let principal = crate::identity::Identity::generate();
        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(principal.libp2p_keypair())
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )
            .unwrap()
            .with_behaviour(|_| network_lobby_behaviour())
            .unwrap()
            .build();
        swarm
            .listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
            .unwrap();
        let address = loop {
            if let SwarmEvent::NewListenAddr { address, .. } = swarm.select_next_some().await {
                break address;
            }
        };
        let principal_peer = *swarm.local_peer_id();
        tokio::spawn(async move {
            loop {
                if let SwarmEvent::Behaviour(request_response::Event::Message {
                    peer,
                    message: request_response::Message::Request { request, channel, .. },
                    ..
                }) = swarm.select_next_some().await
                {
                    let text = match request {
                        LobbyRequest::Post { text, .. } => text,
                        LobbyRequest::Fetch { .. }
                        | LobbyRequest::Info
                        | LobbyRequest::Describe
                        | LobbyRequest::Directory => String::new(),
                    };
                    let node_id = node_id_of_peer(&peer).unwrap_or_default();
                    let _ = swarm.behaviour_mut().send_response(
                        channel,
                        LobbyResponse {
                            accepted: true,
                            error: None,
                            node_id: node_id.clone(),
                            info: None,
                            networks: Vec::new(),
                            pending: false,
                            messages: vec![LobbyMessage {
                                message_id: "m1".into(),
                                sender_node_id: node_id,
                                sender_name: String::new(),
                                member: false,
                                text,
                                sent_at: String::new(),
                            }],
                        },
                    );
                }
            }
        });

        let candidate = crate::identity::Identity::generate();
        let client = LobbyClient::start(&candidate).unwrap();
        let response = client
            .request_at(
                principal_peer,
                vec![address],
                LobbyRequest::Post {
                    card: card("candidate"),
                    message_id: "m1".into(),
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();
        assert!(response.accepted);
        // The frame carries no node id: the principal read it off the handshake.
        assert_eq!(response.node_id, candidate.node_id.0);
        assert_eq!(response.messages[0].text, "hello");
    }

    #[tokio::test]
    async fn published_stats_are_served_from_the_slot_and_ask_for_a_refresh_when_stale() {
        let stats = PublicStats::default();
        // Nothing yet: the request gets nothing and wakes the publisher.
        assert_eq!(stats.current(), None);
        tokio::time::timeout(Duration::from_millis(200), stats.wanted())
            .await
            .expect("an empty slot asks for a refresh");

        stats.publish(NetworkInfo {
            models: (0..200).map(|index| format!("model-{index:03}")).collect(),
            node_count: 3,
            vram_total_mb: Some(48_000),
        });
        let served = stats.current().unwrap();
        assert_eq!(served.node_count, 3);
        assert_eq!(served.models.len(), LobbyLimits::INFO_MODELS);
        // Fresh stats do not ask again.
        assert!(tokio::time::timeout(Duration::from_millis(50), stats.wanted())
            .await
            .is_err());

        stats.clear();
        assert_eq!(stats.current(), None);

        for _ in 0..LobbyLimits::INFO_PER_WINDOW {
            assert!(stats.allow("asker"));
        }
        assert!(!stats.allow("asker"));
        assert!(stats.allow("someone-else"));
    }

    fn listed(name: &str, description: String) -> crate::gateway::PublicNetworkView {
        serde_json::from_value(serde_json::json!({
            "cluster_id": name,
            "name": name,
            "fingerprint": format!("fp-{name}"),
            "trust_document": "{}",
            "organization": "",
            "description": description,
            "rules": "",
            "channels": "",
            "country_code": "BR",
            "country_name": "Brasil",
            "city": "Curitiba",
            "geo_label": "br.curitiba",
            "access_policy": "anyone_with_anchor",
            "location": null,
            "discovered_via": "pkarr",
        }))
        .unwrap()
    }

    #[test]
    fn a_directory_answer_is_cut_to_what_one_frame_carries() {
        let many: Vec<_> = (0..200).map(|i| listed(&format!("n{i}"), String::new())).collect();
        assert_eq!(
            fit_directory_answer(many).len(),
            LobbyLimits::DIRECTORY_NETWORKS
        );
        // Three descriptions of 900 KiB do not fit 2 MiB: the third is dropped.
        let heavy: Vec<_> = (0..3)
            .map(|i| listed(&format!("h{i}"), "x".repeat(900 * 1024)))
            .collect();
        let kept = fit_directory_answer(heavy);
        assert_eq!(kept.len(), 2);
        let frame = serde_json::to_vec(&LobbyResponse {
            accepted: true,
            networks: kept,
            ..LobbyResponse::default()
        })
        .unwrap();
        assert!(frame.len() <= LobbyLimits::RESPONSE_BYTES);
    }

    /// A directory answer is far larger than a request may be; it has to cross
    /// the real codec, not only fit the arithmetic.
    #[tokio::test]
    async fn a_directory_answer_larger_than_a_request_frame_reaches_the_asker() {
        let relay = crate::identity::Identity::generate();
        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(relay.libp2p_keypair())
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )
            .unwrap()
            .with_behaviour(|_| network_lobby_behaviour())
            .unwrap()
            .build();
        swarm
            .listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
            .unwrap();
        let address = loop {
            if let SwarmEvent::NewListenAddr { address, .. } = swarm.select_next_some().await {
                break address;
            }
        };
        let relay_peer = *swarm.local_peer_id();
        tokio::spawn(async move {
            loop {
                if let SwarmEvent::Behaviour(request_response::Event::Message {
                    message:
                        request_response::Message::Request {
                            request, channel, ..
                        },
                    ..
                }) = swarm.select_next_some().await
                {
                    assert_eq!(request, LobbyRequest::Directory);
                    let _ = swarm.behaviour_mut().send_response(
                        channel,
                        LobbyResponse {
                            accepted: true,
                            networks: vec![
                                listed("big", "d".repeat(300 * 1024)),
                                listed("small", String::new()),
                            ],
                            ..LobbyResponse::default()
                        },
                    );
                }
            }
        });

        let fresh = crate::identity::Identity::generate();
        let client = LobbyClient::start(&fresh).unwrap();
        let response = client
            .request_at(relay_peer, vec![address], LobbyRequest::Directory)
            .await
            .unwrap();
        assert!(response.accepted);
        assert_eq!(response.networks.len(), 2);
        assert_eq!(response.networks[0].description.len(), 300 * 1024);
    }

    #[tokio::test]
    async fn the_directory_board_tells_pending_from_disabled_and_asks_to_be_filled() {
        let board = DirectoryBoard::default();
        assert_eq!(board.known(), BoardAnswer::Pending);
        assert_eq!(board.own(), BoardAnswer::Pending);
        tokio::time::timeout(Duration::from_millis(200), board.wanted())
            .await
            .expect("an empty board asks to be filled");

        // A node that is neither a relay nor in a public network.
        board.publish(Vec::new(), None);
        assert_eq!(board.known(), BoardAnswer::Disabled);
        assert_eq!(board.own(), BoardAnswer::Ready(Vec::new()));

        board.publish(Vec::new(), Some(Vec::new()));
        assert_eq!(board.known(), BoardAnswer::Ready(Vec::new()));
        // A fresh board does not ask again.
        assert!(tokio::time::timeout(Duration::from_millis(50), board.wanted())
            .await
            .is_err());

        for _ in 0..LobbyLimits::INFO_PER_WINDOW {
            assert!(board.allow("asker"));
        }
        assert!(!board.allow("asker"));
    }

    /// A key that does not parse is never dialed, and nothing would say so.
    #[test]
    fn default_directory_relays_are_keys() {
        for key in DEFAULT_DIRECTORY_RELAYS {
            assert!(
                crate::pkarr_node::peer_id_from_uri(key).is_some(),
                "{key} is not a pkarr key"
            );
        }
    }

    #[test]
    fn node_id_comes_from_the_peer_id_key() {
        let identity = crate::identity::Identity::generate();
        let peer_id = identity.libp2p_keypair().public().to_peer_id();
        assert_eq!(node_id_of_peer(&peer_id).as_deref(), Some(identity.node_id.0.as_str()));
    }
}
