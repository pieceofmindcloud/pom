//! Route plan per edge: which transport this node uses to talk to each peer.
//!
//! The transport ladder (QUIC, HTTP/HTTPS, libp2p tunnel) rediscovered the route
//! on every frame: each attempt on a dead address consumed the frame's deadline
//! before reaching the path that works. Here the route is **measured once**,
//! in the background, and pinned. The frame uses the pinned route; if it fails, it tries
//! again on it, then moves on to the next already measured route in the list, and only with
//! the list exhausted goes back to the full ladder and asks for a new analysis.
//!
//! The measurement is **per directed edge** and made by the sender: reaching
//! peer B from A says nothing about reaching A from B (a node
//! behind NAT goes out through the tailnet but is only reached through the tunnel).
//! So each process measures its own outgoing edges and none of this goes to gossip.
//!
//! Two plans per peer, because they are different ports and servers:
//! `Inference` (the inference server, destination of prefill and decode) and
//! `Gateway` (the gateway, destination of the direct response's return and of
//! remote-stream).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::{debug, info, warn};

/// Validity of an analysis. After that the edge is measured again, even
/// without a failure: a better path may have appeared (new hole punch, an IPv6 that
/// came back).
pub const PLAN_TTL: Duration = Duration::from_secs(180);

/// How long after the last frame an edge stays pinned. It covers the gap
/// between legs of a generation and between turns of a conversation, without
/// holding an idle peer's route forever.
pub const PIN_GRACE: Duration = Duration::from_secs(60);

/// Wait before measuring again an edge whose last analysis found no route at
/// all (peer still without a model, port still closed). It is not a frame
/// wait: it is only the interval between two background analyses.
const EMPTY_RETRY: Duration = Duration::from_secs(5);

/// Cadence of the planner when nobody wakes it.
const PLANNER_TICK: Duration = Duration::from_secs(2);

/// Ceiling of a direct probe (HTTP or QUIC). The probe runs off the frame; the
/// ceiling only separates "did not answer" from "answered slowly".
#[cfg(not(test))]
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(test)]
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(100);

/// Ceiling of the probe through the tunnel: it may need to open a relay circuit.
const TUNNEL_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Consecutive failures of a pinned route before moving on to the next one. The
/// first failure repeats the same route ("try again"); the second one switches.
const FAILURES_BEFORE_FAILOVER: u32 = 2;

/// Connection establishment budget for an already measured peer: three RTTs,
/// with a floor and a ceiling. The ceiling is QUIC's fallback budget.
const ESTABLISH_FLOOR: Duration = Duration::from_millis(100);
/// Establishment budget before the peer's first measurement.
const ESTABLISH_UNMEASURED: Duration = Duration::from_millis(150);

/// Width of the band in which two routes count as tied in RTT. Within
/// it the preferred transport wins (QUIC, then HTTP, then tunnel):
/// millisecond probes vary more than that from one measurement to the next.
const RTT_TIE_BUCKET_MS: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Plane {
    Inference,
    Gateway,
}

impl Plane {
    pub fn as_str(self) -> &'static str {
        match self {
            Plane::Inference => "inference",
            Plane::Gateway => "gateway",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteKind {
    Quic,
    Http,
    Tunnel,
}

impl RouteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteKind::Quic => "quic",
            RouteKind::Http => "http",
            RouteKind::Tunnel => "tunnel",
        }
    }

    fn preference(self) -> u8 {
        match self {
            RouteKind::Quic => 0,
            RouteKind::Http => 1,
            RouteKind::Tunnel => 2,
        }
    }
}

/// A measured route to a peer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlannedRoute {
    pub kind: RouteKind,
    /// HTTP(S) base URL. On a QUIC route it is the URL of the same host, which the
    /// HTTP/3 client uses as authority; on a tunnel route it stays empty.
    pub base_url: String,
    /// `host:port` of the HTTP/3 endpoint, only on a QUIC route.
    pub quic_addr: Option<String>,
    #[serde(serialize_with = "serialize_ms")]
    pub rtt: Duration,
}

fn serialize_ms<S: serde::Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_f64(value.as_secs_f64() * 1000.0)
}

impl PlannedRoute {
    pub fn describe(&self) -> String {
        let endpoint = match self.kind {
            RouteKind::Quic => self.quic_addr.clone().unwrap_or_default(),
            RouteKind::Http => self.base_url.clone(),
            RouteKind::Tunnel => "libp2p".to_string(),
        };
        format!(
            "{} {} ({:.1} ms)",
            self.kind.as_str(),
            endpoint,
            self.rtt.as_secs_f64() * 1000.0
        )
    }
}

/// Orders the measured routes: lowest RTT first, and within the same
/// `RTT_TIE_BUCKET_MS` band the preferred transport.
pub fn rank(mut routes: Vec<PlannedRoute>) -> Vec<PlannedRoute> {
    routes.sort_by(|a, b| {
        let bucket = |r: &PlannedRoute| (r.rtt.as_secs_f64() * 1000.0 / RTT_TIE_BUCKET_MS) as u64;
        bucket(a)
            .cmp(&bucket(b))
            .then(a.kind.preference().cmp(&b.kind.preference()))
            .then(a.rtt.cmp(&b.rtt))
            .then_with(|| a.base_url.cmp(&b.base_url))
    });
    routes
}

/// What happened to the edge after a failure of the pinned route.
#[derive(Debug, Clone, PartialEq)]
pub enum FailureOutcome {
    /// The same route must be tried again.
    Retry,
    /// The pinned route is now the next one in the list.
    FailedOver(PlannedRoute),
    /// No measured route is left: the edge goes back to the full ladder and a new
    /// analysis was requested.
    Exhausted,
}

struct EdgePlan {
    routes: Vec<PlannedRoute>,
    active: usize,
    strikes: u32,
    measured_at: Instant,
    fingerprint: u64,
    /// The last analysis found no route and this one was kept. The edge stays
    /// due for analysis (`claim_measurement`), which repeats every
    /// `EMPTY_RETRY` until something answers.
    unanswered: bool,
}

#[derive(Clone, Copy)]
struct Attempt {
    at: Instant,
    fingerprint: u64,
}

type EdgeKey = (String, Plane);

/// Use of an edge by frames: how many are in flight and when the last one
/// finished. This is what pins the route while inference runs (see `pinned`).
#[derive(Debug, Default, Clone, Copy)]
struct EdgeUse {
    in_flight: usize,
    last: Option<Instant>,
}

/// Guard for a frame (or delivery) in flight over an edge. While it exists, and
/// for `PIN_GRACE` afterwards, the planner neither re-measures nor swaps the
/// edge's route; only the frame's own failure swaps it (`record_failure`).
pub struct RouteUse<'a> {
    book: &'a RouteBook,
    key: EdgeKey,
}

impl Drop for RouteUse<'_> {
    fn drop(&mut self) {
        if let Ok(mut usage) = self.book.usage.write() {
            if let Some(entry) = usage.get_mut(&self.key) {
                entry.in_flight = entry.in_flight.saturating_sub(1);
                entry.last = Some(Instant::now());
            }
        }
    }
}

/// This process's pinned routes.
#[derive(Default)]
pub struct RouteBook {
    edges: RwLock<HashMap<EdgeKey, EdgePlan>>,
    attempts: RwLock<HashMap<EdgeKey, Attempt>>,
    wake: tokio::sync::Notify,
    /// Peers this node only reaches through a relay circuit: every live libp2p
    /// connection with them is `/p2p-circuit`. A `tunnel` route to one of them goes
    /// through an intermediate server on every frame. Rebuilt on every pass of the
    /// planner, because the hole punch may close after the measurement.
    relayed: RwLock<std::collections::BTreeSet<String>>,
    /// Frames in flight per edge; see `RouteUse`.
    usage: RwLock<HashMap<EdgeKey, EdgeUse>>,
}

/// The process's route book. Global like the `tunnel_sender`: every transport
/// client of the process (gateway, inference server, relay) needs
/// to see the same decision, and each one has its own `PeerClients`.
pub fn book() -> &'static RouteBook {
    static BOOK: OnceLock<RouteBook> = OnceLock::new();
    BOOK.get_or_init(RouteBook::default)
}

impl RouteBook {
    fn key(node_id: &str, plane: Plane) -> EdgeKey {
        (node_id.to_string(), plane)
    }

    /// This edge's pinned route, if there is a valid analysis. An expired
    /// analysis still holds while the edge is in use (`pinned`): a long
    /// generation does not drop to the full ladder halfway just because it
    /// outlived `PLAN_TTL`.
    pub fn active(&self, node_id: &str, plane: Plane) -> Option<PlannedRoute> {
        let key = Self::key(node_id, plane);
        let edges = self.edges.read().ok()?;
        let edge = edges.get(&key)?;
        if edge.measured_at.elapsed() >= PLAN_TTL && !self.pinned(&key) {
            return None;
        }
        edge.routes.get(edge.active).cloned()
    }

    /// Marks a frame in flight over the edge. The route stays pinned while the
    /// guard lives and for `PIN_GRACE` after the last frame.
    pub fn begin_use(&self, node_id: &str, plane: Plane) -> RouteUse<'_> {
        let key = Self::key(node_id, plane);
        if let Ok(mut usage) = self.usage.write() {
            usage.entry(key.clone()).or_default().in_flight += 1;
        }
        RouteUse { book: self, key }
    }

    /// The edge has a frame in flight, or had one less than `PIN_GRACE` ago.
    fn pinned(&self, key: &EdgeKey) -> bool {
        self.usage.read().ok().is_some_and(|usage| {
            usage.get(key).is_some_and(|entry| {
                entry.in_flight > 0 || entry.last.is_some_and(|last| last.elapsed() < PIN_GRACE)
            })
        })
    }

    /// There is a pinned route to preserve: the edge is in use and has an
    /// installed analysis. Without an analysis there is nothing to preserve,
    /// and measurement proceeds as usual.
    fn holds_route(&self, key: &EdgeKey) -> bool {
        self.pinned(key)
            && self
                .edges
                .read()
                .ok()
                .is_some_and(|edges| edges.contains_key(key))
    }

    /// All the measured routes of the edge, in order of preference.
    pub fn routes(&self, node_id: &str, plane: Plane) -> Vec<PlannedRoute> {
        self.edges
            .read()
            .ok()
            .and_then(|edges| {
                edges
                    .get(&Self::key(node_id, plane))
                    .map(|edge| edge.routes[edge.active.min(edge.routes.len())..].to_vec())
            })
            .unwrap_or_default()
    }

    /// Records an analysis result. An empty list keeps the existing route
    /// (renewing its deadline, so `active` still returns it), which only a
    /// real frame's failure erases, and leaves the edge due for analysis, so
    /// it repeats after `EMPTY_RETRY` instead of `PLAN_TTL`.
    pub fn install(
        &self,
        node_id: &str,
        plane: Plane,
        routes: Vec<PlannedRoute>,
        fingerprint: u64,
    ) {
        let routes = rank(routes);
        let key = Self::key(node_id, plane);
        // An analysis that finished while the edge was in use does not swap the
        // route mid-inference: the result is dropped and the edge is measured
        // again once idle (the fingerprint or the deadline will still be
        // stale).
        if self.holds_route(&key) {
            debug!(
                "route {} to {}: analysis dropped, edge in use",
                plane.as_str(),
                short(node_id)
            );
            return;
        }
        let Ok(mut edges) = self.edges.write() else {
            return;
        };
        let previous = edges
            .get(&key)
            .and_then(|edge| edge.routes.get(edge.active).cloned());
        let label = short(node_id);
        if routes.is_empty() {
            // An empty analysis does not erase a route that worked: the probe
            // has a short deadline and a busy peer (engine lock held by
            // another session's prefill chunk) did not answer in time - the
            // tunnel route vanished and the session lifecycle went to
            // unreachable direct addresses. The route stays until a real frame
            // fails on it (`record_failure` exhausts and erases it).
            //
            // The renewed deadline keeps the route in use, but the edge stays
            // due for analysis (`unanswered`): an analysis nobody answered
            // says nothing about the edge, and treating it as fresh held a
            // route measured while the peer was reloading for a whole
            // `PLAN_TTL` after the peer came back.
            if let Some(edge) = edges.get_mut(&key) {
                edge.measured_at = Instant::now();
                edge.fingerprint = fingerprint;
                let kept = edge
                    .routes
                    .get(edge.active)
                    .map(PlannedRoute::describe)
                    .unwrap_or_default();
                if edge.unanswered {
                    debug!(
                        "route {} to {label}: still no answer to the analysis; keeping {kept}",
                        plane.as_str()
                    );
                } else {
                    warn!(
                        "route {} to {label}: no route answered the analysis; keeping {kept}",
                        plane.as_str()
                    );
                }
                edge.unanswered = true;
            }
            return;
        }
        let chosen = routes[0].clone();
        let changed = previous.as_ref().is_none_or(|old| {
            old.kind != chosen.kind
                || old.base_url != chosen.base_url
                || old.quic_addr != chosen.quic_addr
        });
        if changed {
            info!(
                "route {} to {label}: {} [{} alternative(s)]",
                plane.as_str(),
                chosen.describe(),
                routes.len() - 1
            );
        } else {
            debug!(
                "route {} to {label} confirmed: {}",
                plane.as_str(),
                chosen.describe()
            );
        }
        edges.insert(
            key,
            EdgePlan {
                routes,
                active: 0,
                strikes: 0,
                measured_at: Instant::now(),
                fingerprint,
                unanswered: false,
            },
        );
    }

    pub fn record_success(&self, node_id: &str, plane: Plane) {
        if let Ok(mut edges) = self.edges.write() {
            if let Some(edge) = edges.get_mut(&Self::key(node_id, plane)) {
                edge.strikes = 0;
            }
        }
    }

    /// Records a failure of the pinned route and decides the next step.
    pub fn record_failure(&self, node_id: &str, plane: Plane) -> FailureOutcome {
        let outcome = {
            let Ok(mut edges) = self.edges.write() else {
                return FailureOutcome::Exhausted;
            };
            let key = Self::key(node_id, plane);
            let Some(edge) = edges.get_mut(&key) else {
                return FailureOutcome::Exhausted;
            };
            edge.strikes += 1;
            if edge.strikes < FAILURES_BEFORE_FAILOVER {
                return FailureOutcome::Retry;
            }
            edge.strikes = 0;
            edge.active += 1;
            match edge.routes.get(edge.active).cloned() {
                Some(next) => FailureOutcome::FailedOver(next),
                None => {
                    edges.remove(&key);
                    FailureOutcome::Exhausted
                }
            }
        };
        let label = short(node_id);
        match &outcome {
            FailureOutcome::FailedOver(next) => warn!(
                "route {} to {label} failed twice; moving on to {}",
                plane.as_str(),
                next.describe()
            ),
            FailureOutcome::Exhausted => {
                warn!(
                    "route {} to {label}: all measured routes failed; new analysis requested",
                    plane.as_str()
                );
                self.forget_attempt(node_id, plane);
                self.wake.notify_one();
            }
            FailureOutcome::Retry => {}
        }
        outcome
    }

    /// Discards the edge's analysis and wakes the planner.
    pub fn request_reanalysis(&self, node_id: &str, plane: Plane) {
        if let Ok(mut edges) = self.edges.write() {
            edges.remove(&Self::key(node_id, plane));
        }
        self.forget_attempt(node_id, plane);
        self.wake.notify_one();
    }

    fn forget_attempt(&self, node_id: &str, plane: Plane) {
        if let Ok(mut attempts) = self.attempts.write() {
            attempts.remove(&Self::key(node_id, plane));
        }
    }

    /// Reserves the analysis of an edge when it is needed: no analysis,
    /// expired analysis, or the peer changed what it announces (`fingerprint`). An
    /// analysis that found no route only repeats after `EMPTY_RETRY`.
    pub fn claim_measurement(&self, node_id: &str, plane: Plane, fingerprint: u64) -> bool {
        let key = Self::key(node_id, plane);
        // An edge in use is not probed: the probe would compete for the link
        // with the frames and the result would be dropped in `install` anyway.
        if self.holds_route(&key) {
            return false;
        }
        let now = Instant::now();
        let fresh = self.edges.read().ok().is_some_and(|edges| {
            edges.get(&key).is_some_and(|edge| {
                edge.fingerprint == fingerprint
                    && !edge.unanswered
                    && now.duration_since(edge.measured_at) < PLAN_TTL
            })
        });
        if fresh {
            return false;
        }
        let Ok(mut attempts) = self.attempts.write() else {
            return false;
        };
        if let Some(previous) = attempts.get(&key) {
            if previous.fingerprint == fingerprint && now.duration_since(previous.at) < EMPTY_RETRY
            {
                return false;
            }
        }
        attempts.insert(
            key,
            Attempt {
                at: now,
                fingerprint,
            },
        );
        true
    }

    /// Peers reached only through a relay (see the `relayed` field), in order.
    pub fn relayed_nodes(&self) -> Vec<String> {
        self.relayed
            .read()
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn set_relayed(&self, nodes: std::collections::BTreeSet<String>) {
        if let Ok(mut relayed) = self.relayed.write() {
            *relayed = nodes;
        }
    }

    /// Forgets the edges of peers that left the table.
    pub fn retain_nodes(&self, alive: &[String]) {
        let keep = |key: &EdgeKey| alive.iter().any(|id| *id == key.0);
        if let Ok(mut edges) = self.edges.write() {
            edges.retain(|key, _| keep(key));
        }
        if let Ok(mut attempts) = self.attempts.write() {
            attempts.retain(|key, _| keep(key));
        }
        if let Ok(mut usage) = self.usage.write() {
            usage.retain(|key, entry| keep(key) || entry.in_flight > 0);
        }
    }

    /// RTT of the pinned route to each peer in a plan, in ms.
    pub fn rtt_map(&self, plane: Plane) -> HashMap<String, f64> {
        let Ok(edges) = self.edges.read() else {
            return HashMap::new();
        };
        edges
            .iter()
            .filter(|((_, edge_plane), _)| *edge_plane == plane)
            .filter(|(_, edge)| edge.measured_at.elapsed() < PLAN_TTL)
            .filter_map(|((node_id, _), edge)| {
                edge.routes
                    .get(edge.active)
                    .map(|route| (node_id.clone(), route.rtt.as_secs_f64() * 1000.0))
            })
            .collect()
    }

    /// Transport of the pinned route to each peer in a plan (`quic`, `http`,
    /// `tunnel`). Published in `/health` for the gateway to decide which way the
    /// response returns (`fastest::return_path_prefers_ring`).
    pub fn kind_map(&self, plane: Plane) -> HashMap<String, String> {
        let Ok(edges) = self.edges.read() else {
            return HashMap::new();
        };
        edges
            .iter()
            .filter(|((_, edge_plane), _)| *edge_plane == plane)
            .filter(|(_, edge)| edge.measured_at.elapsed() < PLAN_TTL)
            .filter_map(|((node_id, _), edge)| {
                edge.routes
                    .get(edge.active)
                    .map(|route| (node_id.clone(), route.kind.as_str().to_string()))
            })
            .collect()
    }

    /// Lowest RTT measured to the peer, in any plan.
    pub fn rtt(&self, node_id: &str) -> Option<Duration> {
        let edges = self.edges.read().ok()?;
        [Plane::Inference, Plane::Gateway]
            .into_iter()
            .filter_map(|plane| edges.get(&Self::key(node_id, plane)))
            .filter_map(|edge| edge.routes.get(edge.active).map(|route| route.rtt))
            .min()
    }

    /// How long to wait to establish a connection with this peer: three measured
    /// RTTs, between `ESTABLISH_FLOOR` and `ceiling`; without a measurement,
    /// `ESTABLISH_UNMEASURED`. A connection to a dead address cannot
    /// consume the frame's deadline.
    pub fn establish_budget(&self, node_id: &str, ceiling: Duration) -> Duration {
        match self.rtt(node_id) {
            Some(rtt) => (rtt * 3).clamp(ESTABLISH_FLOOR, ceiling.max(ESTABLISH_FLOOR)),
            None => ESTABLISH_UNMEASURED.min(ceiling),
        }
    }

    /// Identity of the pinned routes, without times: it changes when a route is
    /// switched, and not on every read. It serves as a cache key.
    pub fn signature(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .snapshot()
            .into_iter()
            .map(|edge| {
                let active = edge
                    .active
                    .map(|route| {
                        format!(
                            "{}:{}:{:?}",
                            route.kind.as_str(),
                            route.base_url,
                            route.quic_addr
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "{}/{}={active}+{}",
                    edge.node_id,
                    edge.plane.as_str(),
                    edge.alternatives.len()
                )
            })
            .collect();
        out.sort();
        out
    }

    /// State of all the edges, for diagnostics.
    pub fn snapshot(&self) -> Vec<EdgeSnapshot> {
        let Ok(edges) = self.edges.read() else {
            return Vec::new();
        };
        let mut out: Vec<EdgeSnapshot> = edges
            .iter()
            .map(|((node_id, plane), edge)| EdgeSnapshot {
                node_id: node_id.clone(),
                plane: *plane,
                active: edge.routes.get(edge.active).cloned(),
                alternatives: edge.routes[(edge.active + 1).min(edge.routes.len())..].to_vec(),
                measured_ms_ago: edge.measured_at.elapsed().as_millis() as u64,
            })
            .collect();
        out.sort_by(|a, b| {
            a.node_id
                .cmp(&b.node_id)
                .then(a.plane.as_str().cmp(b.plane.as_str()))
        });
        out
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeSnapshot {
    pub node_id: String,
    pub plane: Plane,
    pub active: Option<PlannedRoute>,
    pub alternatives: Vec<PlannedRoute>,
    pub measured_ms_ago: u64,
}

fn short(node_id: &str) -> &str {
    node_id.get(..12).unwrap_or(node_id)
}

// ── Measurement ──────────────────────────────────────────────────────────────

/// What is known about an edge before measuring it.
#[derive(Debug, Clone)]
pub struct EdgeTarget {
    pub node_id: String,
    pub plane: Plane,
    pub http_bases: Vec<String>,
    pub quic_addrs: Vec<String>,
    pub(crate) tunnel: Option<crate::peer_client::TunnelPeer>,
    /// Changes when the peer announces other addresses or another deployment
    /// state: either one invalidates the analysis.
    pub fingerprint: u64,
}

impl EdgeTarget {
    pub fn from_peer(peer: &crate::network::PeerInfo, plane: Plane) -> Self {
        let http_bases = match plane {
            Plane::Inference => peer.inference_candidates(),
            Plane::Gateway => peer.gateway_candidates(),
        };
        let quic_port = match plane {
            Plane::Inference => peer.inference_quic_port,
            Plane::Gateway => peer.gateway_quic_port,
        };
        let quic_addrs: Vec<String> = quic_port
            .map(|port| {
                peer.dialable_addrs()
                    .into_iter()
                    .map(|address| format!("{}:{port}", crate::network::ip_host(&address)))
                    .collect()
            })
            .unwrap_or_default();
        let tunnel = crate::peer_client::TunnelPeer::from_peer(peer);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        http_bases.hash(&mut hasher);
        quic_addrs.hash(&mut hasher);
        peer.libp2p_peer_id.hash(&mut hasher);
        peer.libp2p_dialable_addrs().hash(&mut hasher);
        peer.deploy_id.hash(&mut hasher);
        peer.start_layer.hash(&mut hasher);
        peer.end_layer.hash(&mut hasher);
        crate::peer_client::tls_enabled().hash(&mut hasher);
        crate::peer_client::quic_enabled().hash(&mut hasher);
        Self {
            node_id: peer.node_id.clone(),
            plane,
            http_bases,
            quic_addrs,
            tunnel,
            fingerprint: hasher.finish(),
        }
    }
}

/// Measures all the routes of an edge in parallel, each one with the real
/// transport: `GET /health` over HTTP(S) with the client pinned to the peer's
/// certificate, one HTTP/3 request per QUIC endpoint and `/health` through the
/// tunnel. Two samples per direct route, and the lowest counts: the first pays for
/// the handshake, which the frame does not pay on an already open connection.
pub async fn measure_edge(
    clients: &crate::peer_client::PeerClients,
    target: &EdgeTarget,
) -> Vec<PlannedRoute> {
    use futures::future::join_all;

    let http = join_all(
        target
            .http_bases
            .iter()
            .map(|base| probe_http(clients, &target.node_id, base)),
    );
    let quic_enabled = clients.effective_quic_enabled();
    let quic = join_all(
        target
            .quic_addrs
            .iter()
            .filter(|_| quic_enabled)
            .map(|addr| {
                let base = base_for_quic(&target.http_bases, addr);
                async move {
                    probe_quic(clients, &target.node_id, &base, addr)
                        .await
                        .map(|rtt| PlannedRoute {
                            kind: RouteKind::Quic,
                            base_url: base.clone(),
                            quic_addr: Some(addr.clone()),
                            rtt,
                        })
                }
            }),
    );
    let tunnel = probe_tunnel(target);
    let (http, quic, tunnel) = tokio::join!(http, quic, tunnel);

    let mut routes: Vec<PlannedRoute> = http.into_iter().flatten().collect();
    routes.extend(quic.into_iter().flatten());
    routes.extend(tunnel);
    routes
}

/// The HTTP URL of the same host as a QUIC endpoint, to serve as the authority of
/// the HTTP/3 request. Without one from the same host, the first.
fn base_for_quic(http_bases: &[String], quic_addr: &str) -> String {
    let host = quic_addr
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(quic_addr);
    http_bases
        .iter()
        .find(|base| {
            base.split("://")
                .nth(1)
                .and_then(|rest| rest.rsplit_once(':'))
                .is_some_and(|(base_host, _)| base_host == host)
        })
        .or_else(|| http_bases.first())
        .cloned()
        .unwrap_or_default()
}

async fn probe_http(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base: &str,
) -> Option<PlannedRoute> {
    let client = clients.for_peer(node_id).ok()?;
    let url = format!("{}/health", base.trim_end_matches('/'));
    let mut best: Option<Duration> = None;
    for _ in 0..2 {
        let started = Instant::now();
        // Any HTTP response proves the path: a server still
        // loading the model answers, and the route to it is the same.
        match tokio::time::timeout(PROBE_TIMEOUT, client.get(&url).send()).await {
            // A response from another node is not a path to this one.
            Ok(Ok(response))
                if crate::peer_client::answered_by_other_node(response.headers(), node_id)
                    .is_some() =>
            {
                break
            }
            Ok(Ok(_)) => {
                let rtt = started.elapsed();
                best = Some(best.map_or(rtt, |b| b.min(rtt)));
            }
            _ => break,
        }
    }
    best.map(|rtt| PlannedRoute {
        kind: RouteKind::Http,
        base_url: base.to_string(),
        quic_addr: None,
        rtt,
    })
}

async fn probe_quic(
    clients: &crate::peer_client::PeerClients,
    node_id: &str,
    base: &str,
    addr: &str,
) -> Option<Duration> {
    let addresses = [addr.to_string()];
    let mut best: Option<Duration> = None;
    for _ in 0..2 {
        let started = Instant::now();
        // The HTTP/3 client only speaks POST. Any status (405 included)
        // proves that the handshake and the stream work through this endpoint.
        match clients
            .quic_post(
                node_id,
                base,
                &addresses,
                "/health",
                Vec::new(),
                None,
                PROBE_TIMEOUT,
            )
            .await
        {
            Ok(_) => {
                let rtt = started.elapsed();
                best = Some(best.map_or(rtt, |b| b.min(rtt)));
            }
            Err(_) => break,
        }
    }
    best
}

/// Whether a tunnel reply proves a path to the peer's server. Any status the
/// server itself gave does, as in the direct probes. The tunnel's own "nothing
/// is listening" does not: the direct probes fail against that same closed
/// port, so counting it made the tunnel the only route of every peer measured
/// while its server was down - a deployment being loaded - and that route was
/// then kept for `PLAN_TTL`.
fn tunnel_reply_proves_route(status: u16) -> bool {
    status != crate::infer_tunnel::NOTHING_LISTENING_STATUS
}

async fn probe_tunnel(target: &EdgeTarget) -> Option<PlannedRoute> {
    let tunnel = target.tunnel.as_ref()?;
    let sender = crate::infer_tunnel::tunnel_sender()?;
    let request = match target.plane {
        Plane::Inference => crate::infer_tunnel::TunnelRequest::get("/health"),
        Plane::Gateway => {
            crate::infer_tunnel::TunnelRequest::gateway("GET", "/health", Vec::new(), Vec::new())
        }
    };
    let mut best: Option<Duration> = None;
    for _ in 0..2 {
        let (reply, response) = tokio::sync::oneshot::channel();
        let started = Instant::now();
        sender
            .send(crate::infer_tunnel::TunnelOutbound {
                peer_id: tunnel.peer_id,
                addr_candidates: tunnel.addrs.clone(),
                request: request.clone(),
                reply,
            })
            .await
            .ok()?;
        match tokio::time::timeout(TUNNEL_PROBE_TIMEOUT, response).await {
            Ok(Ok(Ok(response))) if tunnel_reply_proves_route(response.status) => {
                let rtt = started.elapsed();
                best = Some(best.map_or(rtt, |b| b.min(rtt)));
            }
            _ => break,
        }
    }
    best.map(|rtt| PlannedRoute {
        kind: RouteKind::Tunnel,
        base_url: String::new(),
        quic_addr: None,
        rtt,
    })
}

async fn measure_and_install(clients: &crate::peer_client::PeerClients, target: EdgeTarget) {
    let routes = measure_edge(clients, &target).await;
    book().install(&target.node_id, target.plane, routes, target.fingerprint);
}

/// Background planner: keeps this node's edges to all the live peers
/// measured. It runs before any request - the deployment that becomes
/// `serving` changes the peer's `fingerprint` and fires the analysis right away -
/// and is woken when an edge exhausts the measured routes.
pub fn spawn_planner(network: std::sync::Arc<crate::network::Network>) {
    tokio::spawn(async move {
        let clients = crate::peer_client::PeerClients::for_route_probes();
        loop {
            tokio::select! {
                _ = book().wake.notified() => {}
                _ = tokio::time::sleep(PLANNER_TICK) => {}
            }
            if network.is_detached() {
                continue;
            }
            clients.set_tls(network.tls_enabled());
            let peers = network.alive_peers().await;
            let mut jobs = Vec::new();
            for peer in &peers {
                if crate::infer_tunnel::is_local_node(&peer.node_id)
                    || peer.node_id == network.node_id
                {
                    continue;
                }
                for plane in [Plane::Inference, Plane::Gateway] {
                    let target = EdgeTarget::from_peer(peer, plane);
                    if book().claim_measurement(&target.node_id, plane, target.fingerprint) {
                        jobs.push(measure_and_install(&clients, target));
                    }
                }
            }
            futures::future::join_all(jobs).await;
            let alive: Vec<String> = peers.iter().map(|peer| peer.node_id.clone()).collect();
            book().retain_nodes(&alive);
            book().set_relayed(
                peers
                    .iter()
                    .filter(|peer| {
                        let links = peer
                            .libp2p_peer_id
                            .as_deref()
                            .map(|id| network.libp2p_links(id))
                            .unwrap_or_default();
                        !links.is_empty() && links.iter().all(|link| link.relayed)
                    })
                    .map(|peer| peer.node_id.clone())
                    .collect(),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(kind: RouteKind, base: &str, rtt_ms: u64) -> PlannedRoute {
        PlannedRoute {
            kind,
            base_url: base.to_string(),
            quic_addr: (kind == RouteKind::Quic).then(|| "10.0.0.1:1".to_string()),
            rtt: Duration::from_millis(rtt_ms),
        }
    }

    #[test]
    fn the_lowest_rtt_wins_and_a_tie_prefers_quic_then_http() {
        let ranked = rank(vec![
            route(RouteKind::Tunnel, "", 40),
            route(RouteKind::Http, "http://b", 27),
            route(RouteKind::Quic, "http://b", 26),
            route(RouteKind::Http, "http://a", 90),
        ]);
        let kinds: Vec<_> = ranked
            .iter()
            .map(|r| (r.kind, r.base_url.as_str()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (RouteKind::Quic, "http://b"),
                (RouteKind::Http, "http://b"),
                (RouteKind::Tunnel, ""),
                (RouteKind::Http, "http://a"),
            ]
        );
        // Outside the tie band, the RTT rules even against the preferred
        // transport.
        let ranked = rank(vec![
            route(RouteKind::Quic, "q", 30),
            route(RouteKind::Tunnel, "", 10),
        ]);
        assert_eq!(ranked[0].kind, RouteKind::Tunnel);
    }

    /// Moves the end of the edge's last use into the past, as if the grace
    /// period had already elapsed.
    fn age_use(book: &RouteBook, node_id: &str, plane: Plane, by: Duration) {
        let mut usage = book.usage.write().unwrap();
        let entry = usage.get_mut(&RouteBook::key(node_id, plane)).unwrap();
        entry.last = Some(Instant::now() - by);
    }

    fn age_analysis(book: &RouteBook, node_id: &str, plane: Plane, by: Duration) {
        let mut edges = book.edges.write().unwrap();
        let edge = edges.get_mut(&RouteBook::key(node_id, plane)).unwrap();
        edge.measured_at = Instant::now() - by;
    }

    /// With a frame in flight, the planner neither probes nor swaps the route;
    /// it only does so again after the last frame's grace period.
    #[test]
    fn an_edge_in_use_is_neither_remeasured_nor_swapped() {
        let book = RouteBook::default();
        book.install(
            "peer",
            Plane::Inference,
            vec![route(RouteKind::Http, "http://a", 5)],
            1,
        );

        let frame = book.begin_use("peer", Plane::Inference);
        assert!(
            !book.claim_measurement("peer", Plane::Inference, 2),
            "no probe while in use"
        );
        book.install(
            "peer",
            Plane::Inference,
            vec![route(RouteKind::Tunnel, "", 1)],
            2,
        );
        assert_eq!(
            book.active("peer", Plane::Inference).unwrap().kind,
            RouteKind::Http
        );
        // An empty analysis does not erase the route in use either.
        book.install("peer", Plane::Inference, Vec::new(), 2);
        assert!(book.active("peer", Plane::Inference).is_some());
        drop(frame);

        // Right after the last frame the route stays pinned.
        assert!(!book.claim_measurement("peer", Plane::Inference, 2));
        age_use(&book, "peer", Plane::Inference, PIN_GRACE);
        assert!(book.claim_measurement("peer", Plane::Inference, 2));
        book.install(
            "peer",
            Plane::Inference,
            vec![route(RouteKind::Tunnel, "", 1)],
            2,
        );
        assert_eq!(
            book.active("peer", Plane::Inference).unwrap().kind,
            RouteKind::Tunnel
        );
    }

    /// A generation longer than `PLAN_TTL` does not drop to the full ladder
    /// halfway: the expired analysis still holds while the edge is in use.
    #[test]
    fn an_expired_analysis_holds_while_the_edge_is_in_use() {
        let book = RouteBook::default();
        book.install(
            "peer",
            Plane::Gateway,
            vec![route(RouteKind::Http, "http://a", 5)],
            1,
        );
        age_analysis(&book, "peer", Plane::Gateway, PLAN_TTL);
        assert!(
            book.active("peer", Plane::Gateway).is_none(),
            "idle, expired"
        );

        let delivery = book.begin_use("peer", Plane::Gateway);
        assert_eq!(
            book.active("peer", Plane::Gateway).unwrap().base_url,
            "http://a"
        );
        drop(delivery);
        age_use(&book, "peer", Plane::Gateway, PIN_GRACE);
        assert!(book.active("peer", Plane::Gateway).is_none());
    }

    /// The frame's own failure still swaps the route on a pinned edge: the pin
    /// only holds the planner back.
    #[test]
    fn a_frame_failure_switches_route_even_when_pinned() {
        let book = RouteBook::default();
        book.install(
            "peer",
            Plane::Inference,
            vec![
                route(RouteKind::Http, "http://a", 5),
                route(RouteKind::Tunnel, "", 30),
            ],
            1,
        );
        let _frame = book.begin_use("peer", Plane::Inference);
        assert_eq!(
            book.record_failure("peer", Plane::Inference),
            FailureOutcome::Retry
        );
        assert!(matches!(
            book.record_failure("peer", Plane::Inference),
            FailureOutcome::FailedOver(_)
        ));
        assert_eq!(
            book.active("peer", Plane::Inference).unwrap().kind,
            RouteKind::Tunnel
        );
    }

    #[test]
    fn the_tunnel_answering_for_a_closed_port_is_not_a_route() {
        assert!(!tunnel_reply_proves_route(
            crate::infer_tunnel::NOTHING_LISTENING_STATUS
        ));
        // The server answered, even while loading: the path exists.
        assert!(tunnel_reply_proves_route(200));
        assert!(tunnel_reply_proves_route(503));
    }

    /// An empty analysis (a probe that did not answer in time on a busy peer)
    /// does not erase a route that worked; only a real frame's failure does.
    #[test]
    fn an_empty_analysis_keeps_the_working_route() {
        let book = RouteBook::default();
        book.install(
            "peer",
            Plane::Inference,
            vec![route(RouteKind::Tunnel, "", 12)],
            1,
        );
        book.install("peer", Plane::Inference, Vec::new(), 2);
        assert_eq!(
            book.active("peer", Plane::Inference).unwrap().kind,
            RouteKind::Tunnel
        );
        assert!(
            book.claim_measurement("peer", Plane::Inference, 2),
            "an unanswered analysis leaves the edge due, not held for PLAN_TTL"
        );
        assert!(
            !book.claim_measurement("peer", Plane::Inference, 2),
            "and repeats only after EMPTY_RETRY"
        );
        assert_eq!(
            book.record_failure("peer", Plane::Inference),
            FailureOutcome::Retry
        );
        assert_eq!(
            book.record_failure("peer", Plane::Inference),
            FailureOutcome::Exhausted
        );
        assert!(book.active("peer", Plane::Inference).is_none());
    }

    #[test]
    fn a_failure_retries_once_switches_route_and_runs_out_asking_for_a_new_analysis() {
        let book = RouteBook::default();
        book.install(
            "peer",
            Plane::Inference,
            vec![
                route(RouteKind::Http, "http://a", 5),
                route(RouteKind::Tunnel, "", 30),
            ],
            1,
        );
        assert_eq!(
            book.active("peer", Plane::Inference).unwrap().base_url,
            "http://a"
        );

        assert_eq!(
            book.record_failure("peer", Plane::Inference),
            FailureOutcome::Retry
        );
        assert_eq!(
            book.active("peer", Plane::Inference).unwrap().kind,
            RouteKind::Http
        );
        match book.record_failure("peer", Plane::Inference) {
            FailureOutcome::FailedOver(next) => assert_eq!(next.kind, RouteKind::Tunnel),
            other => panic!("expected a route switch, got {other:?}"),
        }
        assert_eq!(
            book.active("peer", Plane::Inference).unwrap().kind,
            RouteKind::Tunnel
        );

        // A success resets the count: an isolated failure after it repeats.
        book.record_success("peer", Plane::Inference);
        assert_eq!(
            book.record_failure("peer", Plane::Inference),
            FailureOutcome::Retry
        );
        assert_eq!(
            book.record_failure("peer", Plane::Inference),
            FailureOutcome::Exhausted
        );
        assert!(book.active("peer", Plane::Inference).is_none());
        // Exhausted, the edge can be measured again right away.
        assert!(book.claim_measurement("peer", Plane::Inference, 1));
    }

    #[test]
    fn analysis_only_repeats_when_expired_changed_or_no_route_was_found() {
        let book = RouteBook::default();
        assert!(book.claim_measurement("peer", Plane::Gateway, 7));
        // An analysis in progress or just found empty does not repeat before EMPTY_RETRY.
        assert!(!book.claim_measurement("peer", Plane::Gateway, 7));
        book.install(
            "peer",
            Plane::Gateway,
            vec![route(RouteKind::Http, "http://g", 3)],
            7,
        );
        assert!(!book.claim_measurement("peer", Plane::Gateway, 7));
        // The peer announced other addresses: the analysis is valid again.
        assert!(book.claim_measurement("peer", Plane::Gateway, 8));
        // An explicit request for reanalysis too.
        book.request_reanalysis("peer", Plane::Gateway);
        assert!(book.active("peer", Plane::Gateway).is_none());
        assert!(book.claim_measurement("peer", Plane::Gateway, 8));
    }

    #[test]
    fn the_connection_budget_follows_the_measured_rtt() {
        let book = RouteBook::default();
        let ceiling = Duration::from_millis(300);
        assert_eq!(book.establish_budget("x", ceiling), ESTABLISH_UNMEASURED);
        book.install(
            "x",
            Plane::Inference,
            vec![route(RouteKind::Http, "http://x", 50)],
            1,
        );
        assert_eq!(
            book.establish_budget("x", ceiling),
            Duration::from_millis(150)
        );
        book.install(
            "y",
            Plane::Inference,
            vec![route(RouteKind::Http, "http://y", 5)],
            1,
        );
        assert_eq!(book.establish_budget("y", ceiling), ESTABLISH_FLOOR);
        book.install(
            "z",
            Plane::Inference,
            vec![route(RouteKind::Http, "http://z", 200)],
            1,
        );
        assert_eq!(book.establish_budget("z", ceiling), ceiling);
    }

    #[test]
    fn the_quic_authority_comes_from_the_same_host() {
        let bases = vec![
            "http://[2804::1]:8181".to_string(),
            "http://100.85.81.27:8181".to_string(),
        ];
        assert_eq!(
            base_for_quic(&bases, "100.85.81.27:43778"),
            "http://100.85.81.27:8181"
        );
        assert_eq!(
            base_for_quic(&bases, "[2804::1]:43778"),
            "http://[2804::1]:8181"
        );
        assert_eq!(base_for_quic(&bases, "10.9.9.9:1"), "http://[2804::1]:8181");
    }
}
