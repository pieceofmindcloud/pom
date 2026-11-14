//! Lobby: the room a public network opens to machines that are not members.
//!
//! A candidate talks to the network's principal before joining, so the admin
//! can decide on admission. The candidate is not in the cluster: it has no API
//! key, no gossip announce and no admission. What it has is its node key. The
//! connection runs under a transport key derived from it (see
//! [`transport_key`]), and each chat request carries the node key's
//! signature over that transport identity: the principal names the candidate
//! by the key that signed, never by a field the frame merely states.
//!
//! The protocol is pull only. A candidate posts a text or fetches the recent
//! history, and every request carries its hardware card; the principal never
//! opens a stream towards a candidate and keeps no session beyond a presence
//! entry that expires. Inside the network the lobby is the `lobby` room of the
//! network Chat: the principal turns a candidate's text into an ordinary
//! relayed `ChatAction`, and members answer through the usual authenticated
//! star.
//!
//! [`LobbyClient`] is the candidate's side. It is a small endpoint of its own
//! (dial only, over n0's relays when no direct path answers) so it also runs
//! in setup mode, where the node has no cluster endpoint yet.

use crate::p2p::Codec;
use crate::p2p::{EndpointAddr, EndpointId};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const NETWORK_LOBBY_PROTOCOL: &str = "/pom/network-lobby/1.0.0";
/// Room id of the lobby inside the network Chat.
pub const LOBBY_ROOM: &str = "lobby";

/// Bounds of the lobby. They are much tighter than the network Chat's because
/// the sender is not a member: the frame is accepted before any credential.
pub struct LobbyLimits;

impl LobbyLimits {
    /// Bound of a request. Sized for `Publish`, which carries a network's
    /// trust document; a chat text is bounded separately by `TEXT_BYTES`.
    pub const FRAME_BYTES: usize = 768 * 1024;
    /// Bound of an answer. Larger than a request's because a directory answer
    /// carries whole network descriptions, trust document included; it is
    /// read by the side that asked, from a peer it chose to dial.
    pub const RESPONSE_BYTES: usize = 2 * 1024 * 1024;
    /// Networks in one directory answer.
    pub const DIRECTORY_NETWORKS: usize = 64;
    /// Networks a relay keeps from their principals' publications.
    pub const PUBLICATIONS: usize = 256;
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

/// Each candidate talks in a thread of its own, `lobby/<candidate node_id>`:
/// it reads only its own, and only the admins the chain proves receive any.
/// A thread is erased when the candidate is approved, or once nobody wrote in
/// it for [`THREAD_TTL`], so nothing said there reaches a later candidate.
pub const LOBBY_THREAD_PREFIX: &str = "lobby/";
/// How long a thread nobody writes in is kept.
pub const THREAD_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The room of `candidate`'s lobby thread.
pub fn thread_room(candidate: &str) -> String {
    format!("{LOBBY_THREAD_PREFIX}{candidate}")
}

/// The candidate whose thread `room` is, if it is one.
pub fn thread_candidate(room: &str) -> Option<&str> {
    room.strip_prefix(LOBBY_THREAD_PREFIX)
        .filter(|candidate| !candidate.is_empty())
}

/// Whether `room` belongs to the lobby: the presence room or a thread. Only
/// admins receive it.
pub fn is_lobby_room(room: &str) -> bool {
    room == LOBBY_ROOM || thread_candidate(room).is_some()
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
    /// Round trip of the candidate's last request to the principal, as the
    /// candidate measured it. Self-declared, like the hardware.
    #[serde(default)]
    pub rtt_ms: Option<u32>,
    /// Whether the candidate reaches the principal through a relay circuit.
    /// Set by the principal from the connection it sees; whatever the
    /// candidate sent here is discarded.
    #[serde(default)]
    pub relayed: Option<bool>,
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
            rtt_ms: None,
            relayed: None,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LobbyRequest {
    /// Say something in the lobby.
    Post {
        identity: LobbyIdentity,
        card: LobbyCard,
        message_id: String,
        text: String,
    },
    /// Read the recent history. Also what keeps the candidate listed.
    Fetch {
        identity: LobbyIdentity,
        card: LobbyCard,
    },
    /// Ask to be admitted with `role`. The request waits for an admin; the
    /// answer to it, and to every later `Fetch`, carries its state.
    Admission {
        identity: LobbyIdentity,
        card: LobbyCard,
        role: crate::config::EnrollmentRole,
    },
    /// A member asks the principal for the network's current connection key,
    /// because the one it holds stopped being accepted - it missed a
    /// rotation. Answered with the grant only to a node on the principal's
    /// member list (an approved admission record). It does not depend on the
    /// lobby being open, and works for unlisted networks too.
    ClaimKey { identity: LobbyIdentity },
    /// A node says it is leaving the network. It is the same notice a member
    /// sends its principal over the cluster's control channel, for when that
    /// channel is not there: the principal never came to admit this node, or
    /// cannot be reached that way. The principal forgets the node's
    /// admission and rotates the connection key.
    Leave { identity: LobbyIdentity },
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
    /// A network's principal tells a relay about its network, so the relay
    /// can list it. Sent over a direct dial to the relay, which is what makes
    /// listing work for a network whose every node is behind NAT: the relay
    /// has no need to reach back. `identity` must be the principal's.
    Publish {
        identity: LobbyIdentity,
        network: crate::gateway::PublicNetworkView,
    },
}

/// How long a relay lists a published network without hearing again.
pub const PUBLICATION_TTL: Duration = Duration::from_secs(5 * 60);
/// How often a principal publishes its network to its relays.
pub const PUBLICATION_INTERVAL: Duration = Duration::from_secs(90);

/// Relay keys (`pk:...`) every installation asks for the public networks, in
/// addition to `POM_DIRECTORY_RELAYS` and to the relays already configured.
/// A key here must parse as a pkarr key: `default_directory_relays_are_keys`.
pub const DEFAULT_DIRECTORY_RELAYS: &[&str] =
    &["pk:kyqrhnypyqfu7ic43ej1i7a5kjmyh8nxxrpumzmxjqk6dty4xq6o"];

/// Keys from [`DEFAULT_DIRECTORY_RELAYS`] and the `DIRECTORY_RELAYS`
/// environment variable (comma separated), without repeats.
pub fn default_directory_relays() -> Vec<String> {
    // Never in tests: a route test that lists networks would dial the real
    // relay and assert on whatever is published there that day.
    let built_in: &[&str] = if cfg!(test) {
        &[]
    } else {
        DEFAULT_DIRECTORY_RELAYS
    };
    let mut keys: Vec<String> = built_in.iter().map(|key| key.to_string()).collect();
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStatus {
    Pending,
    Approved,
    Rejected,
}

/// What an approved candidate needs to join: the network's trust document and
/// its connection key. It only ever travels in an answer to the candidate
/// whose signed identity an admin approved, over the QUIC connection that
/// identity was proven on.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct AdmissionGrant {
    pub trust_document: String,
    pub connection_key: String,
    #[serde(default)]
    pub relay_peers: Vec<String>,
    /// The network's settings as they are now. The copy inside the trust
    /// document is the one signed when the network was created - a network
    /// that started local would be joined with discovery off.
    #[serde(default)]
    pub network: Option<crate::config::NetworkSettings>,
}

impl std::fmt::Debug for AdmissionGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdmissionGrant(..)")
    }
}

/// The state of the caller's own admission request, in an answer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdmissionView {
    pub status: AdmissionStatus,
    pub role: crate::config::EnrollmentRole,
    /// Present only when approved.
    #[serde(default)]
    pub grant: Option<AdmissionGrant>,
    /// Only when rejected: how many seconds until this machine may ask
    /// again. Zero or absent means it may ask now.
    #[serde(default)]
    pub retry_after_secs: Option<u64>,
}

/// One admission request as the principal keeps it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionRecord {
    pub node_id: String,
    pub card: LobbyCard,
    pub role: crate::config::EnrollmentRole,
    pub status: AdmissionStatus,
    pub requested_at: String,
    #[serde(default)]
    pub decided_at: Option<String>,
}

/// Admission records kept at once; the oldest decided ones go first. The
/// approved ones are the network's member list, so this also bounds how many
/// members can reclaim a rotated key.
pub const ADMISSION_RECORDS: usize = 512;
/// How long a node that just left is kept off the member list, so the last
/// seconds it is still seen as a peer do not put it back.
pub const DEPARTED_HOLD: Duration = Duration::from_secs(10 * 60);
/// How long a rejected machine waits before it may ask again.
pub const ADMISSION_RETRY_AFTER_SECS: i64 = 60 * 60;

/// Seconds a rejected record still has to wait before its node may ask
/// again; `None` when it is not rejected or the wait is over.
pub fn admission_retry_after(
    record: &AdmissionRecord,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<u64> {
    if record.status != AdmissionStatus::Rejected {
        return None;
    }
    let decided = record
        .decided_at
        .as_deref()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())?
        .with_timezone(&chrono::Utc);
    let remaining = ADMISSION_RETRY_AFTER_SECS - (now - decided).num_seconds();
    (remaining > 0).then_some(remaining as u64)
}

/// Applies one request to the list. A pending or approved request is only
/// refreshed (card, role while pending); a rejected one may be asked again
/// after [`ADMISSION_RETRY_AFTER_SECS`]. Returns the resulting status.
pub fn apply_admission_request(
    records: &mut Vec<AdmissionRecord>,
    node_id: &str,
    card: LobbyCard,
    role: crate::config::EnrollmentRole,
    now: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<AdmissionStatus, &'static str> {
    if let Some(record) = records.iter_mut().find(|record| record.node_id == node_id) {
        match record.status {
            AdmissionStatus::Approved => {}
            AdmissionStatus::Pending => {
                record.card = card;
                record.role = role;
            }
            AdmissionStatus::Rejected => {
                if admission_retry_after(record, now).is_some() {
                    return Err("admission_rejected");
                }
                record.card = card;
                record.role = role;
                record.status = AdmissionStatus::Pending;
                record.requested_at = now.to_rfc3339();
                record.decided_at = None;
            }
        }
        return Ok(record.status);
    }
    if records.len() >= ADMISSION_RECORDS {
        // Make room by dropping the oldest request that was already decided.
        let Some(oldest) = records
            .iter()
            .position(|record| record.status != AdmissionStatus::Pending)
        else {
            return Err("admissions_full");
        };
        records.remove(oldest);
    }
    records.push(AdmissionRecord {
        node_id: node_id.to_string(),
        card,
        role,
        status: AdmissionStatus::Pending,
        requested_at: now.to_rfc3339(),
        decided_at: None,
    });
    Ok(AdmissionStatus::Pending)
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
    /// The caller's own admission request, when it has one. Only in answers
    /// to `Post`, `Fetch` and `Admission`.
    #[serde(default)]
    pub admission: Option<AdmissionView>,
    /// The lobby is open but its conversation is not: the network Chat is
    /// off there, so only an admission request goes through. Only in answers
    /// to `Post`, `Fetch` and `Admission`.
    #[serde(default)]
    pub messages_closed: bool,
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

// ── Identity: a transport key of its own, bound to the node key ──────────────

const TRANSPORT_KEY_DOMAIN: &[u8] = b"pom/network-lobby/transport-key/1";
const IDENTITY_DOMAIN: &[u8] = b"pom/network-lobby/identity/1:";

/// The endpoint key the lobby client connects with.
///
/// It is **not** the node key, on purpose. The node's own endpoint already
/// runs under the node key, and two endpoints under one key confuse the
/// relays, which hold one connection per key: the second would knock the
/// first off its home relay. A separate key keeps the two apart; the
/// derivation is deterministic so the candidate keeps one identity across
/// restarts.
pub fn transport_key(identity: &crate::identity::Identity) -> crate::p2p::SecretKey {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(TRANSPORT_KEY_DOMAIN);
    hasher.update(identity.signing_key.to_bytes());
    let seed: [u8; 32] = hasher.finalize().into();
    crate::p2p::SecretKey::from_bytes(&seed)
}

fn identity_message(transport: &EndpointId) -> Vec<u8> {
    let mut message = IDENTITY_DOMAIN.to_vec();
    message.extend_from_slice(transport.as_bytes());
    message
}

/// Proof that the transport identity of a lobby connection belongs to a
/// node: the node key's signature over that transport endpoint id. It is only
/// worth something together with the transport private key, which never
/// leaves the node, so replaying it from another connection proves nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LobbyIdentity {
    /// The node's Ed25519 public key, hex.
    pub pubkey: String,
    /// Signature over the transport endpoint id, hex.
    pub signature: String,
}

impl LobbyIdentity {
    pub fn sign(identity: &crate::identity::Identity, transport: &EndpointId) -> Self {
        use ed25519_dalek::Signer;
        Self {
            pubkey: identity.public_key_hex(),
            signature: hex::encode(
                identity
                    .signing_key
                    .sign(&identity_message(transport))
                    .to_bytes(),
            ),
        }
    }

    /// The `node_id` this proof names, when it was signed for `transport`.
    pub fn verified_node_id(&self, transport: &EndpointId) -> Option<String> {
        use ed25519_dalek::Verifier;
        let pubkey: [u8; 32] = hex::decode(&self.pubkey).ok()?.try_into().ok()?;
        let key = ed25519_dalek::VerifyingKey::from_bytes(&pubkey).ok()?;
        let signature: [u8; 64] = hex::decode(&self.signature).ok()?.try_into().ok()?;
        key.verify(
            &identity_message(transport),
            &ed25519_dalek::Signature::from_bytes(&signature),
        )
        .ok()?;
        Some(crate::identity::NodeId::from_pubkey(&key).0)
    }
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
/// The protocol handler answers from this slot and never computes: the numbers come
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
            callers
                .retain(|_, (started, _)| now.duration_since(*started) < LobbyLimits::INFO_WINDOW);
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
/// Like [`PublicStats`], the protocol handler only reads it: the listing resolves
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
    /// Networks their principals published here, by fingerprint.
    published: Mutex<HashMap<String, (Instant, crate::gateway::PublicNetworkView)>>,
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
            callers
                .retain(|_, (started, _)| now.duration_since(*started) < LobbyLimits::INFO_WINDOW);
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

    /// Keeps a network its principal published (already validated). Refused
    /// by a node that does not answer directory queries, or when full.
    pub fn accept_publication(&self, network: crate::gateway::PublicNetworkView) -> bool {
        let gathers = matches!(
            self.known
                .lock()
                .expect("directory board known lock")
                .as_ref(),
            Some((_, Some(_)))
        );
        if !gathers {
            // Not known yet, or disabled: have the board filled so the next
            // publication (90 s later) has an answer.
            self.wanted.notify_one();
            return false;
        }
        let now = Instant::now();
        let mut published = self
            .published
            .lock()
            .expect("directory board published lock");
        published.retain(|_, (at, _)| now.duration_since(*at) < PUBLICATION_TTL);
        if !published.contains_key(&network.fingerprint)
            && published.len() >= LobbyLimits::PUBLICATIONS
        {
            return false;
        }
        published.insert(network.fingerprint.clone(), (now, network));
        true
    }

    fn publications(&self) -> Listed {
        let now = Instant::now();
        let mut published = self
            .published
            .lock()
            .expect("directory board published lock");
        published.retain(|_, (at, _)| now.duration_since(*at) < PUBLICATION_TTL);
        let mut listed: Listed = published.values().map(|(_, view)| view.clone()).collect();
        listed.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
        listed
    }

    pub fn known(&self) -> BoardAnswer {
        let slot = self
            .known
            .lock()
            .expect("directory board known lock")
            .clone();
        if Self::stale(slot.as_ref().map(|(at, _)| *at)) {
            self.wanted.notify_one();
        }
        match slot {
            Some((at, Some(mut listed))) => {
                // What principals published here comes first: it is the
                // principal's own word, and newer than anything gathered. It
                // is served whatever the age of the gathered part, which
                // comes from a slow scan and is dropped once too old; an
                // answer never waits for that scan.
                let mut merged = self.publications();
                if at.elapsed() >= INFO_MAX_AGE {
                    listed.clear();
                }
                listed.retain(|gathered| {
                    !merged
                        .iter()
                        .any(|published| published.fingerprint == gathered.fingerprint)
                });
                merged.append(&mut listed);
                BoardAnswer::Ready(fit_directory_answer(merged))
            }
            Some((_, None)) => BoardAnswer::Disabled,
            None => BoardAnswer::Pending,
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
        used += serde_json::to_vec(network)
            .map(|bytes| bytes.len())
            .unwrap_or(budget);
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
impl Codec for LobbyCodec {
    const PROTOCOL: &'static str = NETWORK_LOBBY_PROTOCOL;
    type Request = LobbyRequest;
    type Response = LobbyResponse;

    async fn read_request<T>(&mut self, io: &mut T) -> std::io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io, LobbyLimits::FRAME_BYTES).await
    }

    async fn read_response<T>(&mut self, io: &mut T) -> std::io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        read_msg(io, LobbyLimits::RESPONSE_BYTES).await
    }

    async fn write_request<T>(&mut self, io: &mut T, request: Self::Request) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &request, LobbyLimits::FRAME_BYTES).await
    }

    async fn write_response<T>(
        &mut self,
        io: &mut T,
        response: Self::Response,
    ) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        write_msg(io, &response, LobbyLimits::RESPONSE_BYTES).await
    }
}

// ── Candidate side ───────────────────────────────────────────────────────────

/// How long a resolved publisher address is reused before asking pkarr again.
const DIAL_CACHE_TTL: Duration = Duration::from_secs(60);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Dial-only endpoint that reaches a network's principal by its key.
pub struct LobbyClient {
    p2p: Arc<crate::p2p::P2p>,
    pkarr: crate::pkarr_node::PkarrNode,
    dial_cache: Mutex<HashMap<String, (Instant, EndpointAddr)>>,
    node_id: String,
    proof: LobbyIdentity,
}

impl LobbyClient {
    /// Binds the endpoint. It runs under [`transport_key`], not the node key,
    /// on an ephemeral port, and reaches the principal over n0's relays when
    /// no direct path answers: a candidate only ever dials.
    pub async fn start(identity: &crate::identity::Identity) -> Result<Arc<Self>> {
        Self::start_with(identity, iroh::RelayMode::Default).await
    }

    async fn start_with(
        identity: &crate::identity::Identity,
        relay_mode: iroh::RelayMode,
    ) -> Result<Arc<Self>> {
        let pkarr = crate::pkarr_node::PkarrNode::from_signing_key(&identity.signing_key)?;
        let transport = transport_key(identity);
        let proof = LobbyIdentity::sign(identity, &transport.public());
        let book = iroh::address_lookup::MemoryLookup::new();
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .secret_key(transport)
            .relay_mode(relay_mode)
            .address_lookup(book.clone())
            .bind()
            .await
            .context("binding the lobby endpoint")?;
        Ok(Arc::new(Self {
            p2p: crate::p2p::P2p::new(endpoint, book),
            pkarr,
            dial_cache: Mutex::new(HashMap::new()),
            node_id: identity.node_id.0.clone(),
            proof,
        }))
    }

    /// The proof a chat request carries so the principal can name this node.
    pub fn proof(&self) -> LobbyIdentity {
        self.proof.clone()
    }

    /// This machine's `node_id`, so the screen can tell its own messages.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    async fn dial_address(&self, key: &str) -> Option<EndpointAddr> {
        if let Some((at, address)) = self
            .dial_cache
            .lock()
            .expect("lobby dial cache lock")
            .get(key)
            .cloned()
        {
            if at.elapsed() < DIAL_CACHE_TTL {
                return Some(address);
            }
        }
        let address = tokio::time::timeout(RESOLVE_TIMEOUT, self.pkarr.resolve_dial_addr(key))
            .await
            .ok()
            .flatten()?;
        self.dial_cache
            .lock()
            .expect("lobby dial cache lock")
            .insert(key.to_string(), (Instant::now(), address.clone()));
        Some(address)
    }

    /// Sends one request to the principal whose key is `key` (`pk:...`). The
    /// address is resolved through pkarr at dial time and never stored.
    pub async fn request(&self, key: &str, request: LobbyRequest) -> Result<LobbyResponse> {
        crate::pkarr_node::endpoint_id_from_uri(key).context("the publisher key is invalid")?;
        let Some(address) = self.dial_address(key).await else {
            anyhow::bail!("publisher_unresolved");
        };
        let result = self.request_at(address, request).await;
        if matches!(&result, Err(error) if error.to_string().starts_with("publisher_unreachable")) {
            // A stale address is the usual cause; resolve again next time.
            self.dial_cache
                .lock()
                .expect("lobby dial cache lock")
                .remove(key);
        }
        result
    }

    /// Sends one request to an already resolved endpoint address.
    async fn request_at(
        &self,
        address: EndpointAddr,
        request: LobbyRequest,
    ) -> Result<LobbyResponse> {
        let texts = crate::p2p::addr_texts(&address);
        let call = self.p2p.call(LobbyCodec, address.id, &texts, request);
        match tokio::time::timeout(REQUEST_TIMEOUT, call).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(anyhow::anyhow!("publisher_unreachable: {error}")),
            Err(_) => Err(anyhow::anyhow!("publisher_timeout")),
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
            LobbyClient::start(&identity).await
        })
        .await
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A principal's endpoint on loopback, answering the lobby protocol with
    /// `answer`. No relay: the candidate dials the direct address.
    async fn serve_lobby<F>(
        identity: &crate::identity::Identity,
        answer: F,
    ) -> (EndpointAddr, iroh::protocol::Router)
    where
        F: Fn(EndpointId, LobbyRequest) -> LobbyResponse + Clone + Send + Sync + 'static,
    {
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .secret_key(identity.secret_key())
            .relay_mode(iroh::RelayMode::Disabled)
            .bind_addr(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let address = EndpointAddr::from_parts(
            endpoint.id(),
            endpoint
                .bound_sockets()
                .into_iter()
                .map(iroh::TransportAddr::Ip),
        );
        let serve = crate::p2p::Serve::new(
            LobbyCodec,
            move |connection: iroh::endpoint::Connection, request| {
                let answer = answer.clone();
                async move { answer(connection.remote_id(), request) }
            },
        );
        let router = iroh::protocol::Router::builder(endpoint)
            .accept(NETWORK_LOBBY_PROTOCOL.as_bytes(), serve)
            .spawn();
        (address, router)
    }

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
            rtt_ms: None,
            relayed: None,
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
        assert_eq!(
            guests.admit("a", &card("alpha"), false),
            Ok(GuestSeen::Announce)
        );
        assert_eq!(
            guests.admit("a", &card("alpha"), false),
            Ok(GuestSeen::Known)
        );
        assert_eq!(
            guests.admit("a", &card("renamed"), false),
            Ok(GuestSeen::Announce)
        );
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
            assert!(guests
                .admit(&format!("n{index}"), &card("x"), false)
                .is_ok());
        }
        assert_eq!(
            guests.admit("one-more", &card("x"), false),
            Err("lobby_full")
        );
    }

    /// The candidate's endpoint against an endpoint that only speaks the lobby
    /// protocol, over loopback: the dial, the codec and the identity the
    /// answering side derives from the transport.
    #[tokio::test]
    async fn a_candidate_reaches_a_principal_and_is_named_by_its_transport_key() {
        let principal = crate::identity::Identity::generate();
        let (address, _router) = serve_lobby(&principal, |peer, request| {
            let text = match &request {
                LobbyRequest::Post { text, .. } => text.clone(),
                LobbyRequest::Fetch { .. }
                | LobbyRequest::Admission { .. }
                | LobbyRequest::ClaimKey { .. }
                | LobbyRequest::Leave { .. }
                | LobbyRequest::Publish { .. }
                | LobbyRequest::Info
                | LobbyRequest::Describe
                | LobbyRequest::Directory => String::new(),
            };
            let node_id = match &request {
                LobbyRequest::Post { identity, .. }
                | LobbyRequest::Fetch { identity, .. }
                | LobbyRequest::Admission { identity, .. } => {
                    identity.verified_node_id(&peer).unwrap_or_default()
                }
                _ => String::new(),
            };
            LobbyResponse {
                accepted: true,
                error: None,
                node_id: node_id.clone(),
                info: None,
                networks: Vec::new(),
                pending: false,
                admission: None,
                messages_closed: false,
                messages: vec![LobbyMessage {
                    message_id: "m1".into(),
                    sender_node_id: node_id,
                    sender_name: String::new(),
                    member: false,
                    text,
                    sent_at: String::new(),
                }],
            }
        })
        .await;

        let candidate = crate::identity::Identity::generate();
        let client = LobbyClient::start_with(&candidate, iroh::RelayMode::Disabled)
            .await
            .unwrap();
        let response = client
            .request_at(
                address,
                LobbyRequest::Post {
                    identity: client.proof(),
                    card: card("candidate"),
                    message_id: "m1".into(),
                    text: "hello".into(),
                },
            )
            .await
            .unwrap();
        assert!(response.accepted);
        // The principal named the candidate by the key that signed for the
        // transport it actually saw, not by anything the frame states.
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
        assert!(
            tokio::time::timeout(Duration::from_millis(50), stats.wanted())
                .await
                .is_err()
        );

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
        let many: Vec<_> = (0..200)
            .map(|i| listed(&format!("n{i}"), String::new()))
            .collect();
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
        let (address, _router) = serve_lobby(&relay, |_, request| {
            assert_eq!(request, LobbyRequest::Directory);
            LobbyResponse {
                accepted: true,
                networks: vec![
                    listed("big", "d".repeat(300 * 1024)),
                    listed("small", String::new()),
                ],
                ..LobbyResponse::default()
            }
        })
        .await;

        let fresh = crate::identity::Identity::generate();
        let client = LobbyClient::start_with(&fresh, iroh::RelayMode::Disabled)
            .await
            .unwrap();
        let response = client
            .request_at(address, LobbyRequest::Directory)
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
        assert!(
            tokio::time::timeout(Duration::from_millis(50), board.wanted())
                .await
                .is_err()
        );

        for _ in 0..LobbyLimits::INFO_PER_WINDOW {
            assert!(board.allow("asker"));
        }
        assert!(!board.allow("asker"));
    }

    #[test]
    fn a_relay_lists_what_principals_published_ahead_of_what_it_gathered() {
        let board = DirectoryBoard::default();
        // Before the board is filled, and on a node that does not answer
        // directory queries, a publication is not kept.
        assert!(!board.accept_publication(listed("early", String::new())));
        board.publish(Vec::new(), None);
        assert!(!board.accept_publication(listed("nobody", String::new())));

        let mut gathered = listed("both", "gathered".into());
        gathered.fingerprint = "fp-both".into();
        board.publish(
            Vec::new(),
            Some(vec![gathered, listed("only-gathered", String::new())]),
        );
        assert!(board.accept_publication(listed("both", "published".into())));
        assert!(board.accept_publication(listed("only-published", String::new())));
        // Publishing again replaces, it does not duplicate.
        assert!(board.accept_publication(listed("both", "published again".into())));

        let BoardAnswer::Ready(answer) = board.known() else {
            panic!("the board has an answer");
        };
        let names: Vec<_> = answer.iter().map(|network| network.name.as_str()).collect();
        assert_eq!(names, ["both", "only-published", "only-gathered"]);
        assert_eq!(answer[0].description, "published again");

        // When the gathered part is too old to serve, what principals
        // published is still answered: the relay never goes back to
        // "pending" while a new scan runs.
        board.known.lock().unwrap().as_mut().unwrap().0 =
            Instant::now() - INFO_MAX_AGE - Duration::from_secs(1);
        let BoardAnswer::Ready(answer) = board.known() else {
            panic!("publications are served without the gathered part");
        };
        let names: Vec<_> = answer.iter().map(|network| network.name.as_str()).collect();
        assert_eq!(names, ["both", "only-published"]);
    }

    /// A key that does not parse is never dialed, and nothing would say so.
    #[test]
    fn default_directory_relays_are_keys() {
        for key in DEFAULT_DIRECTORY_RELAYS {
            assert!(
                crate::pkarr_node::endpoint_id_from_uri(key).is_some(),
                "{key} is not a pkarr key"
            );
        }
    }

    #[test]
    fn an_admission_request_waits_is_refreshed_and_respects_a_rejection() {
        use crate::config::EnrollmentRole;
        let now = chrono::Utc::now();
        let mut records = Vec::new();
        assert_eq!(
            apply_admission_request(
                &mut records,
                "a",
                card("alpha"),
                EnrollmentRole::Worker,
                now
            ),
            Ok(AdmissionStatus::Pending)
        );
        // Asking again while pending only refreshes what the admin will read.
        assert_eq!(
            apply_admission_request(
                &mut records,
                "a",
                card("renamed"),
                EnrollmentRole::Visitor,
                now
            ),
            Ok(AdmissionStatus::Pending)
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].card.hostname, "renamed");
        assert_eq!(records[0].role, EnrollmentRole::Visitor);

        // A rejection holds for a while, then the machine may ask again.
        records[0].status = AdmissionStatus::Rejected;
        records[0].decided_at = Some(now.to_rfc3339());
        // The candidate is told how long, counted down from the decision.
        assert_eq!(
            admission_retry_after(&records[0], now),
            Some(ADMISSION_RETRY_AFTER_SECS as u64)
        );
        assert_eq!(
            admission_retry_after(&records[0], now + chrono::Duration::seconds(600)),
            Some(ADMISSION_RETRY_AFTER_SECS as u64 - 600)
        );
        assert_eq!(
            admission_retry_after(
                &records[0],
                now + chrono::Duration::seconds(ADMISSION_RETRY_AFTER_SECS)
            ),
            None
        );
        assert_eq!(
            apply_admission_request(
                &mut records,
                "a",
                card("alpha"),
                EnrollmentRole::Worker,
                now
            ),
            Err("admission_rejected")
        );
        let later = now + chrono::Duration::seconds(ADMISSION_RETRY_AFTER_SECS + 1);
        assert_eq!(
            apply_admission_request(
                &mut records,
                "a",
                card("alpha"),
                EnrollmentRole::Worker,
                later
            ),
            Ok(AdmissionStatus::Pending)
        );

        // An approval is never undone by the candidate asking again.
        records[0].status = AdmissionStatus::Approved;
        assert_eq!(
            apply_admission_request(&mut records, "a", card("x"), EnrollmentRole::Visitor, later),
            Ok(AdmissionStatus::Approved)
        );
        assert_eq!(records[0].role, EnrollmentRole::Worker);

        // Full of pending requests, a new one is refused rather than evicting.
        let mut full: Vec<AdmissionRecord> = Vec::new();
        for index in 0..ADMISSION_RECORDS {
            apply_admission_request(
                &mut full,
                &format!("n{index}"),
                card("x"),
                EnrollmentRole::Worker,
                now,
            )
            .unwrap();
        }
        assert_eq!(
            apply_admission_request(&mut full, "late", card("x"), EnrollmentRole::Worker, now),
            Err("admissions_full")
        );
        full[3].status = AdmissionStatus::Rejected;
        assert!(
            apply_admission_request(&mut full, "late", card("x"), EnrollmentRole::Worker, now)
                .is_ok()
        );
        assert!(full.iter().all(|record| record.node_id != "n3"));
    }

    #[test]
    fn the_lobby_connects_under_a_key_that_is_not_the_node_key() {
        let identity = crate::identity::Identity::generate();
        let node_key = identity.secret_key().public();
        let transport = transport_key(&identity).public();
        // Two endpoints under one key would knock each other off the relays.
        assert_ne!(transport, node_key);
        // Stable across restarts, and different for another node.
        assert_eq!(transport, transport_key(&identity).public());
        let other = crate::identity::Identity::generate();
        assert_ne!(transport, transport_key(&other).public());
    }

    #[test]
    fn a_lobby_identity_names_the_node_only_on_the_transport_it_was_signed_for() {
        let identity = crate::identity::Identity::generate();
        let transport = transport_key(&identity).public();
        let proof = LobbyIdentity::sign(&identity, &transport);
        assert_eq!(
            proof.verified_node_id(&transport).as_deref(),
            Some(identity.node_id.0.as_str())
        );

        // Replayed from someone else's connection, it proves nothing.
        let thief = crate::identity::Identity::generate();
        let thief_transport = transport_key(&thief).public();
        assert_eq!(proof.verified_node_id(&thief_transport), None);

        // Claiming another node's key without its signature fails too.
        let forged = LobbyIdentity {
            pubkey: identity.public_key_hex(),
            signature: LobbyIdentity::sign(&thief, &thief_transport).signature,
        };
        assert_eq!(forged.verified_node_id(&thief_transport), None);
        assert_eq!(LobbyIdentity::default().verified_node_id(&transport), None);
    }
}
