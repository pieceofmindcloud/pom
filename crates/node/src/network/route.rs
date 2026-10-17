//! Choice of the address through which to talk to each peer.
//!
//! A node has more than one valid address, and which of them works depends on who
//! is asking. This module stores each candidate's measurement and decides which to use.
//! The decision lives here, alone and pure, because getting it wrong raises no error:
//! the node starts, shows up in the cluster, and the request dies silently on hop one.

use serde::{Deserialize, Serialize};

/// How much better the new candidate has to be for the switch to be worth it.
///
/// Both conditions hold together. The relative one alone would switch routes between
/// 0.8 and 1.2 ms; the absolute one alone would ignore a proportionally huge
/// degradation on an already slow link. The 25% comes from the 2026-08-13 measurement
/// between MacBook and ksia (9.6 against 7.2 ms of p50), which must NOT switch.
const RELATIVE_MARGIN: f64 = 0.25;
const ABSOLUTE_MARGIN_MS: f64 = 2.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    pub addr: String,
    /// `None` is "did not answer", which is different from "answered slowly".
    pub rtt_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteChoice {
    pub addr: String,
    pub rtt_ms: f64,
}

/// Chooses the path to talk through, given what was measured and what is used today.
///
/// Returns `None` when no candidate answered - the caller treats that
/// as an unreachable peer, not as "use the first in the list".
pub fn choose_route(measurements: &[Measurement], current: Option<&str>) -> Option<RouteChoice> {
    let mut alive: Vec<(&str, f64)> = measurements
        .iter()
        .filter_map(|m| m.rtt_ms.map(|rtt| (m.addr.as_str(), rtt)))
        .collect();
    if alive.is_empty() {
        return None;
    }
    // Stable order before comparing: without this, two tied candidates
    // come out in iteration order and two nodes may choose different paths.
    alive.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(b.0))
    });
    let (best_addr, best_rtt) = alive[0];

    // The current route is only kept if it still answers.
    if let Some(current_addr) = current {
        if let Some(&(_, current_rtt)) = alive.iter().find(|(a, _)| *a == current_addr) {
            let relative_gain = (current_rtt - best_rtt) / current_rtt;
            let absolute_gain = current_rtt - best_rtt;
            let worth_switching =
                relative_gain >= RELATIVE_MARGIN && absolute_gain >= ABSOLUTE_MARGIN_MS;
            if !worth_switching {
                return Some(RouteChoice {
                    addr: current_addr.to_string(),
                    rtt_ms: current_rtt,
                });
            }
        }
    }
    Some(RouteChoice {
        addr: best_addr.to_string(),
        rtt_ms: best_rtt,
    })
}

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// How often the candidates are measured again.
pub const MEASURE_INTERVAL: Duration = Duration::from_secs(300);
/// A probe that goes past this counts as a failure.
#[cfg(not(test))]
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(test)]
const PROBE_TIMEOUT: Duration = Duration::from_millis(50);

struct Entry {
    choice: RouteChoice,
    measured_at: Instant,
}

/// Recent transport failure. The entry remains until a new successful
/// probe or until it expires, so that a following session does not pay the
/// timeout of the same address again.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(300);

/// The path through which this node talks to each peer.
///
/// It lives only in this process's memory and is not published in the gossip:
/// publishing a measurement table would turn addressing into distributed state, and
/// the `PeerList` is already at the MTU limit.
#[derive(Default)]
pub struct RouteTable {
    by_node: RwLock<HashMap<String, Entry>>,
    /// Failure memory per `(node_id, URL)`.
    ///
    /// The key is the whole URL, so the inference port and the gateway port
    /// of the same host coexist without being confused - and it is on purpose that
    /// both live here. The control plane (the Dashboard reading
    /// `/api/ui/node`) suffers from the same defect the inference path
    /// suffered from: without memory, a dead address is redialed on every read.
    /// A second table would be a parallel mechanism with the same decision
    /// to make in two places.
    failures: RwLock<HashMap<(String, String), Instant>>,
    /// Last HTTP address that actually answered, per peer: it is what the participants
    /// screen shows as the direct route in use (see `last_success`).
    successes: RwLock<HashMap<String, (String, Instant)>>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Address chosen for this peer, if already measured and still valid.
    pub fn resolve(&self, node_id: &str) -> Option<String> {
        self.by_node
            .read()
            .ok()?
            .get(node_id)
            .filter(|entry| entry.measured_at.elapsed() < MEASURE_INTERVAL)
            .map(|e| e.choice.addr.clone())
    }

    /// Route and latency of each peer, for the interface to show.
    pub fn snapshot(&self) -> Vec<(String, RouteChoice)> {
        self.by_node
            .read()
            .map(|t| {
                t.iter()
                    .map(|(k, v)| (k.clone(), v.choice.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Takes a route out of use right away, without waiting for the cycle.
    ///
    /// Called when a real request failed on that address: waiting 5
    /// minutes would mean failing everything that arrived in the meantime.
    pub fn mark_failure(&self, node_id: &str, addr: &str) {
        if let Ok(mut t) = self.by_node.write() {
            if t.get(node_id)
                .map(|e| e.choice.addr == addr)
                .unwrap_or(false)
            {
                t.remove(node_id);
            }
        }
        if let Ok(mut failures) = self.failures.write() {
            failures.insert((node_id.to_string(), addr.to_string()), Instant::now());
        }
        if let Ok(mut successes) = self.successes.write() {
            if successes.get(node_id).is_some_and(|(ok, _)| ok == addr) {
                successes.remove(node_id);
            }
        }
    }

    /// Tells whether a real request has recently failed on this address.
    pub fn is_failed(&self, node_id: &str, addr: &str) -> bool {
        let Ok(mut failures) = self.failures.write() else {
            return false;
        };
        let key = (node_id.to_string(), addr.to_string());
        let Some(failed_at) = failures.get(&key).copied() else {
            return false;
        };
        if failed_at.elapsed() >= FAILURE_COOLDOWN {
            failures.remove(&key);
            false
        } else {
            true
        }
    }

    /// Discards the choice when the peer announces another address topology.
    ///
    /// A route measured for an old address cannot survive an
    /// update of `addr_candidates`: during the 5 minute window of the
    /// measurement it would make the pipeline keep dialing a container IP that
    /// is no longer announced.
    ///
    /// **The failure memory is not erased here.** It is keyed by the
    /// address, so a new address simply is not in the map, and an
    /// address that answers again is cleared by `mark_success` or by the
    /// probe. Erasing by `node_id` was worse than useless: a rotating temporary
    /// IPv6 changes `addr_candidates`, and the rotation - which is exactly the
    /// event that kills the old address - was also the event that erased the
    /// proof that it died. The cluster went back to dialing the corpse.
    pub fn invalidate(&self, node_id: &str) {
        if let Ok(mut t) = self.by_node.write() {
            t.remove(node_id);
        }
    }

    /// Forgets everything known about this peer. Only for when the peer disappears.
    pub fn forget(&self, node_id: &str) {
        self.invalidate(node_id);
        if let Ok(mut failures) = self.failures.write() {
            failures.retain(|(id, _), _| id != node_id);
        }
        if let Ok(mut successes) = self.successes.write() {
            successes.remove(node_id);
        }
    }

    /// A real request that answered erases the address's failure memory.
    ///
    /// It is the opposite of `mark_failure`, and what lets an address return to
    /// use before the end of `FAILURE_COOLDOWN` when it serves again.
    pub fn mark_success(&self, node_id: &str, addr: &str) {
        if let Ok(mut failures) = self.failures.write() {
            failures.remove(&(node_id.to_string(), addr.to_string()));
        }
        if let Ok(mut successes) = self.successes.write() {
            successes.insert(node_id.to_string(), (addr.to_string(), Instant::now()));
        }
    }

    /// The HTTP address that answered last for this peer, if that was
    /// less than `FAILURE_COOLDOWN` ago and it has not failed since. It is an
    /// observed fact, not what the peer announces: an announced IPv6 that nobody
    /// reaches never shows up here.
    pub fn last_success(&self, node_id: &str) -> Option<String> {
        let (addr, when) = self.successes.read().ok()?.get(node_id).cloned()?;
        (when.elapsed() < FAILURE_COOLDOWN && !self.is_failed(node_id, &addr)).then_some(addr)
    }

    /// Candidates that have not been discredited yet, in the order received.
    ///
    /// Returning empty is information, not an error: it means "every address
    /// this node knows for this peer failed recently". The caller must not
    /// insist on any of them - it must ask for the peer's current address (see
    /// `Network::request_peer_addresses`) and say on the screen that it is
    /// finding the node again.
    pub fn usable(&self, node_id: &str, candidates: &[String]) -> Vec<String> {
        candidates
            .iter()
            .filter(|url| !self.is_failed(node_id, url))
            .cloned()
            .collect()
    }

    /// Whether the route to this exact endpoint failed recently.
    ///
    /// An HTTP failure on the gateway or inference port does not prove that the
    /// P2P/UDP endpoint of the same host is dead. Comparing only the host made
    /// the gossip and the rediscovery erase transport addresses that were still
    /// serving the prefill.
    pub fn endpoint_is_discredited(&self, node_id: &str, addr: &str) -> bool {
        let Some(target) = host_and_port_of(addr) else {
            return false;
        };
        let Ok(failures) = self.failures.read() else {
            return false;
        };
        failures.iter().any(|((id, url), failed_at)| {
            id == node_id
                && host_and_port_of(url).is_some_and(|endpoint| endpoint == target)
                && failed_at.elapsed() < FAILURE_COOLDOWN
        })
    }

    /// Writes a choice directly, without probing. For tests only.
    #[cfg(test)]
    pub fn remember_for_test(&self, node_id: &str, addr: &str, rtt_ms: f64) {
        if let Ok(mut t) = self.by_node.write() {
            t.insert(
                node_id.to_string(),
                Entry {
                    choice: RouteChoice {
                        addr: addr.to_string(),
                        rtt_ms,
                    },
                    measured_at: Instant::now(),
                },
            );
        }
        self.mark_success(node_id, addr);
    }

    #[cfg(test)]
    pub fn remember_for_test_with_age(
        &self,
        node_id: &str,
        addr: &str,
        rtt_ms: f64,
        age: Duration,
    ) {
        if let Ok(mut table) = self.by_node.write() {
            table.insert(
                node_id.to_string(),
                Entry {
                    choice: RouteChoice {
                        addr: addr.to_string(),
                        rtt_ms,
                    },
                    measured_at: Instant::now() - age,
                },
            );
        }
        self.mark_success(node_id, addr);
    }

    /// Whether enough time has passed since this peer's last measurement.
    pub fn needs_measuring(&self, node_id: &str) -> bool {
        self.by_node
            .read()
            .map(|t| match t.get(node_id) {
                Some(e) => e.measured_at.elapsed() >= MEASURE_INTERVAL,
                None => true,
            })
            .unwrap_or(true)
    }

    /// Measures all the candidates and writes the choice.
    ///
    /// The probe is `GET /health` over HTTP(S), not ICMP: it measures exactly the
    /// inference port the pipeline will use, with firewall and proxy in between.
    /// Probing the gateway would not be enough: it may answer while the
    /// inference port swallows SYN.
    ///
    /// The probe's client accepts the node's self-signed certificate. That only holds
    /// for measuring latency: the clients that carry inference keep
    /// using pinning by `node_id`.
    pub async fn measure_peer(
        &self,
        clients: &crate::peer_client::PeerClients,
        node_id: &str,
        candidates: &[String],
    ) {
        let Ok(http) = clients.probe() else {
            return;
        };
        let mut measurements = Vec::with_capacity(candidates.len());
        for route in candidates {
            let start = Instant::now();
            let response =
                tokio::time::timeout(PROBE_TIMEOUT, http.get(format!("{route}/health")).send())
                    .await;
            let rtt = match response {
                Ok(Ok(r)) if r.status().is_success() => {
                    self.mark_success(node_id, route);
                    Some(start.elapsed().as_secs_f64() * 1000.0)
                }
                // A slow probe does **not** discredit the address: its
                // timeout is 3 s, much tighter than that of a real
                // request, and `resolve_step_url` has no route left when all the
                // candidates are discredited. Only a failure of a real request
                // writes to the memory - that is how 84e7141 designed
                // it, and that is how it stays.
                _ => None,
            };
            measurements.push(Measurement {
                addr: route.clone(),
                rtt_ms: rtt,
            });
        }
        let current = self.resolve(node_id);
        if let Some(choice) = choose_route(&measurements, current.as_deref()) {
            if let Ok(mut t) = self.by_node.write() {
                t.insert(
                    node_id.to_string(),
                    Entry {
                        choice,
                        measured_at: Instant::now(),
                    },
                );
            }
        } else if let Ok(mut t) = self.by_node.write() {
            t.remove(node_id);
        }
    }

    /// Measures the currently chosen route again without reordering the candidates.
    ///
    /// The addressing measurement is deliberately slower than the RTT
    /// measurement used by the dashboard. Reusing `RouteChoice.rtt_ms` would keep the RTT
    /// frozen until the next route switch; probing the chosen URL
    /// keeps the two cadences independent.
    pub async fn measure_current_peer(
        &self,
        clients: &crate::peer_client::PeerClients,
        node_id: &str,
    ) -> Option<f64> {
        let route = self.resolve(node_id)?;
        let http = clients.probe().ok()?;
        let start = Instant::now();
        let response =
            tokio::time::timeout(PROBE_TIMEOUT, http.get(format!("{route}/health")).send()).await;
        match response {
            Ok(Ok(response)) if response.status().is_success() => {
                self.mark_success(node_id, &route);
                Some(start.elapsed().as_secs_f64() * 1000.0)
            }
            _ => None,
        }
    }
}

fn host_and_port_of(addr: &str) -> Option<(&str, u16)> {
    let without_scheme = addr.split_once("://").map_or(addr, |(_, rest)| rest);
    let authority = without_scheme.split('/').next().unwrap_or(without_scheme);
    let (host, port) = if authority.starts_with('[') {
        let host_end = authority.find(']')?;
        let host = &authority[1..host_end];
        let port = authority.get(host_end + 1..)?.strip_prefix(':')?;
        (host, port)
    } else {
        authority.rsplit_once(':')?
    };
    Some((host, port.parse().ok()?))
}

/// A round trip latency measurement from this node to a destination.
/// The opposite direction is another measurement, kept by the remote process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RttMeasurement {
    pub target_node_id: String,
    pub rtt_ms: f64,
    /// `p2p` is the normal transport. `direct-health` identifies the fallback
    /// used when the peer does not offer the new tunnel yet.
    pub transport: String,
}

#[derive(Debug, Clone)]
pub struct RttSnapshot {
    pub measurement: RttMeasurement,
    pub age: Duration,
}

struct RttEntry {
    measurement: RttMeasurement,
    measured_at: Instant,
}

/// Directional RTTs measured by this process.
///
/// It is not a shortest path table. Each entry is a direct measurement
/// `origin -> destination`; an edge is never inferred from the sum of others.
#[derive(Default)]
pub struct RttTable {
    by_destination: RwLock<HashMap<String, RttEntry>>,
}

impl RttTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> Vec<RttMeasurement> {
        self.snapshot_with_age()
            .into_iter()
            .map(|entry| entry.measurement)
            .collect()
    }

    pub fn snapshot_with_age(&self) -> Vec<RttSnapshot> {
        let mut snapshots = self
            .by_destination
            .read()
            .map(|table| {
                table
                    .values()
                    .map(|entry| RttSnapshot {
                        measurement: entry.measurement.clone(),
                        age: entry.measured_at.elapsed(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        snapshots.sort_by(|a, b| {
            a.measurement
                .target_node_id
                .cmp(&b.measurement.target_node_id)
        });
        snapshots
    }

    pub fn fresh_snapshot(&self) -> Vec<RttMeasurement> {
        self.snapshot_with_age()
            .into_iter()
            .filter(|entry| entry.age < RTT_MEASURE_INTERVAL)
            .map(|entry| entry.measurement)
            .collect()
    }

    pub fn needs_measuring(&self, node_id: &str) -> bool {
        self.by_destination
            .read()
            .map(|t| match t.get(node_id) {
                Some(entry) => entry.measured_at.elapsed() >= RTT_MEASURE_INTERVAL,
                None => true,
            })
            .unwrap_or(true)
    }

    pub fn remember(&self, node_id: &str, rtt_ms: f64, transport: &str) {
        if !rtt_ms.is_finite() || rtt_ms < 0.0 {
            return;
        }
        if let Ok(mut t) = self.by_destination.write() {
            t.insert(
                node_id.to_string(),
                RttEntry {
                    measurement: RttMeasurement {
                        target_node_id: node_id.to_string(),
                        rtt_ms,
                        transport: transport.to_string(),
                    },
                    measured_at: Instant::now(),
                },
            );
        }
    }

    pub fn mark_failure(&self, node_id: &str) {
        if let Ok(mut t) = self.by_destination.write() {
            t.remove(node_id);
        }
    }

    #[cfg(test)]
    pub fn remember_for_test(&self, node_id: &str, rtt_ms: f64, transport: &str) {
        self.remember(node_id, rtt_ms, transport);
    }

    #[cfg(test)]
    pub fn remember_for_test_with_age(
        &self,
        node_id: &str,
        rtt_ms: f64,
        transport: &str,
        age: Duration,
    ) {
        if let Ok(mut table) = self.by_destination.write() {
            table.insert(
                node_id.to_string(),
                RttEntry {
                    measurement: RttMeasurement {
                        target_node_id: node_id.to_string(),
                        rtt_ms,
                        transport: transport.to_string(),
                    },
                    measured_at: Instant::now() - age,
                },
            );
        }
    }
}

/// The directional measurement is renewed together with the addressing route.
pub const RTT_MEASURE_INTERVAL: Duration = Duration::from_secs(120);

/// Prefers the tunnel when it answered, but lets a peer that announced
/// the capability and is momentarily without the endpoint remain measurable through
/// the direct `/health`. Before, this fallback only existed for peers that never
/// announced a tunnel, so the RTT entry was erased on every renewal.
pub(crate) fn choose_rtt(
    tunnel_rtt_ms: Option<f64>,
    direct_health_rtt_ms: Option<f64>,
) -> Option<(f64, &'static str)> {
    tunnel_rtt_ms
        .filter(|rtt| rtt.is_finite() && *rtt >= 0.0)
        .map(|rtt| (rtt, "p2p"))
        .or_else(|| {
            direct_health_rtt_ms
                .filter(|rtt| rtt.is_finite() && *rtt >= 0.0)
                .map(|rtt| (rtt, "direct-health"))
        })
}

#[cfg(test)]
pub mod tests_support {
    use axum::{routing::get, Router};
    use std::time::Duration;

    /// Starts a local `/health` that answers after `delay_ms`, and returns the
    /// base URL. It is the same endpoint the probe uses in production.
    pub async fn health_server(delay_ms: u64) -> String {
        let app = Router::new().route(
            "/health",
            get(move || async move {
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                "ok"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(addr: &str, rtt: f64) -> Measurement {
        Measurement {
            addr: addr.into(),
            rtt_ms: Some(rtt),
        }
    }

    /// The numbers are the ones measured on 2026-08-13 between the MacBook and ksia:
    /// LAN IPv4 9.6 ms of p50 against 7.2 ms for the global IPv6, a tie within
    /// the Wi-Fi noise. Without a margin, this pair would switch routes every cycle.
    #[test]
    fn a_tie_within_the_margin_keeps_the_current_route() {
        let measurements = vec![m("192.168.100.138:8092", 9.6), m("[2804:d55::1]:8092", 7.2)];
        let choice = choose_route(&measurements, Some("192.168.100.138:8092")).unwrap();
        assert_eq!(
            choice.addr, "192.168.100.138:8092",
            "7.2 vs 9.6 does not exceed 25%"
        );
    }

    #[test]
    fn a_clear_win_switches_the_route() {
        let measurements = vec![
            m("192.168.100.138:8092", 40.0),
            m("[2804:d55::1]:8092", 7.0),
        ];
        let choice = choose_route(&measurements, Some("192.168.100.138:8092")).unwrap();
        assert_eq!(choice.addr, "[2804:d55::1]:8092");
    }

    /// Large percentage difference but tiny absolute one: 0.8 against 1.2 ms
    /// goes past 25% and does not reach 2 ms. Switching here is noise.
    #[test]
    fn a_percentage_win_under_two_milliseconds_does_not_switch() {
        let measurements = vec![m("a:1", 1.2), m("b:1", 0.8)];
        let choice = choose_route(&measurements, Some("a:1")).unwrap();
        assert_eq!(choice.addr, "a:1");
    }

    #[test]
    fn without_a_current_route_the_fastest_wins() {
        let measurements = vec![m("a:1", 9.6), m("b:1", 7.2)];
        assert_eq!(choose_route(&measurements, None).unwrap().addr, "b:1");
    }

    /// A candidate that did not answer is not choosable, even if it is the current one.
    #[test]
    fn an_unreachable_current_route_is_replaced_at_once() {
        let measurements = vec![
            Measurement {
                addr: "a:1".into(),
                rtt_ms: None,
            },
            m("b:1", 50.0),
        ];
        assert_eq!(
            choose_route(&measurements, Some("a:1")).unwrap().addr,
            "b:1"
        );
    }

    #[test]
    fn no_reachable_candidate_yields_none() {
        let measurements = vec![Measurement {
            addr: "a:1".into(),
            rtt_ms: None,
        }];
        assert!(choose_route(&measurements, Some("a:1")).is_none());
    }

    #[test]
    fn rtt_table_keeps_directional_measurements_and_transport() {
        let table = RttTable::new();
        table.remember_for_test("peer-b", 12.5, "p2p");
        table.remember_for_test("peer-c", 30.0, "direct-health");

        assert_eq!(
            table.snapshot(),
            vec![
                RttMeasurement {
                    target_node_id: "peer-b".into(),
                    rtt_ms: 12.5,
                    transport: "p2p".into(),
                },
                RttMeasurement {
                    target_node_id: "peer-c".into(),
                    rtt_ms: 30.0,
                    transport: "direct-health".into(),
                },
            ]
        );
        assert!(table.needs_measuring("peer-d"));
    }

    #[test]
    fn direct_health_is_a_fallback_when_the_tunnel_measurement_is_unavailable() {
        assert_eq!(choose_rtt(None, Some(3.5)), Some((3.5, "direct-health")));
        assert_eq!(choose_rtt(Some(1.5), Some(3.5)), Some((1.5, "p2p")));
    }

    #[test]
    fn a_stale_rtt_is_visible_but_not_fresh_for_selection() {
        let table = RttTable::new();
        table.remember_for_test("fresh", 12.5, "p2p");
        table.remember_for_test_with_age(
            "stale",
            1.0,
            "direct-health",
            RTT_MEASURE_INTERVAL + Duration::from_secs(1),
        );

        let with_age = table.snapshot_with_age();
        assert_eq!(with_age.len(), 2);
        assert!(with_age.iter().any(|entry| {
            entry.measurement.target_node_id == "stale" && entry.age >= RTT_MEASURE_INTERVAL
        }));
        assert_eq!(
            table
                .fresh_snapshot()
                .into_iter()
                .map(|measurement| measurement.target_node_id)
                .collect::<Vec<_>>(),
            vec!["fresh"]
        );
    }

    /// Exact tie between two candidates with no current route: lexicographic
    /// order wins, so two nodes reach the same conclusion instead of
    /// diverging by HashMap order.
    #[test]
    fn an_exact_tie_is_broken_deterministically() {
        let measurements = vec![m("b:1", 5.0), m("a:1", 5.0)];
        assert_eq!(choose_route(&measurements, None).unwrap().addr, "a:1");
    }

    /// Two local servers with different delays: the faster one wins, and the
    /// table starts resolving that node_id to it.
    #[tokio::test]
    async fn measuring_two_candidates_picks_the_faster_one() {
        let fast = tests_support::health_server(0).await;
        let slow = tests_support::health_server(80).await;
        let table = RouteTable::new();
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(10));
        table
            .measure_peer(&http, "n1", &[slow.clone(), fast.clone()])
            .await;
        assert_eq!(table.resolve("n1"), Some(fast));
    }

    /// A marked failure takes the address out of use immediately, without waiting for
    /// the 5 minute cycle: the next request already goes through another path.
    #[tokio::test]
    async fn a_failure_drops_the_route_right_away() {
        let alive = tests_support::health_server(0).await;
        let table = RouteTable::new();
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(10));
        table.measure_peer(&http, "n1", &[alive.clone()]).await;
        assert_eq!(table.resolve("n1"), Some(alive.clone()));
        table.mark_failure("n1", &alive);
        assert_eq!(table.resolve("n1"), None);
    }

    #[test]
    fn an_address_update_invalidates_the_stale_route() {
        let table = RouteTable::new();
        table.remember_for_test("n1", "https://172.17.0.1:8092", 1.0);

        table.invalidate("n1");

        assert_eq!(table.resolve("n1"), None);
    }

    #[test]
    fn a_stale_route_is_not_used_for_inference_address_selection() {
        let table = RouteTable::new();
        table.remember_for_test_with_age(
            "n1",
            "https://172.17.0.1:8092",
            1.0,
            MEASURE_INTERVAL + Duration::from_secs(1),
        );

        assert_eq!(table.resolve("n1"), None);
    }

    /// A dead candidate is not chosen just for being in the list.
    #[tokio::test]
    async fn a_dead_candidate_is_never_chosen() {
        let alive = tests_support::health_server(0).await;
        let table = RouteTable::new();
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(10));
        // Port 1 closed: the probe fails fast instead of waiting for the timeout.
        table
            .measure_peer(
                &http,
                "n1",
                &["http://127.0.0.1:1".to_string(), alive.clone()],
            )
            .await;
        assert_eq!(table.resolve("n1"), Some(alive));
    }

    /// The route with a scheme and the raw address with the same port compare equal,
    /// but different services on the same host stay independent.
    #[test]
    fn endpoint_includes_its_port_with_or_without_a_url_scheme() {
        assert_eq!(
            host_and_port_of("http://[2804:d55::9]:8080"),
            Some(("2804:d55::9", 8080))
        );
        assert_eq!(
            host_and_port_of("[2804:d55::9]:42777"),
            Some(("2804:d55::9", 42777))
        );
    }

    /// A failure on the gateway does not erase the P2P address used by the tunnel or
    /// prefill, even when both resolve to the same host.
    #[test]
    fn a_dead_gateway_does_not_discredit_the_peer_transport_port() {
        let table = RouteTable::new();
        table.mark_failure("n1", "http://[2804:d55::9]:8080");
        assert!(!table.endpoint_is_discredited("n1", "[2804:d55::9]:42777"));
        assert!(table.endpoint_is_discredited("n1", "http://[2804:d55::9]:8080"));
        assert!(!table.endpoint_is_discredited("n1", "[2804:d55::a]:42777"));
        assert!(!table.endpoint_is_discredited("n2", "[2804:d55::9]:42777"));
    }

    /// `usable` returns empty when everything failed, and that is information: the
    /// caller has to ask for the current address instead of choosing a corpse.
    #[test]
    fn usable_is_empty_when_every_candidate_is_discredited() {
        let table = RouteTable::new();
        let candidates = vec![
            "http://10.0.0.5:8080".to_string(),
            "http://10.0.0.6:8080".to_string(),
        ];
        assert_eq!(table.usable("n1", &candidates), candidates);
        table.mark_failure("n1", &candidates[0]);
        assert_eq!(table.usable("n1", &candidates), vec![candidates[1].clone()]);
        table.mark_failure("n1", &candidates[1]);
        assert!(table.usable("n1", &candidates).is_empty());
        table.mark_success("n1", &candidates[0]);
        assert_eq!(table.usable("n1", &candidates), vec![candidates[0].clone()]);
    }

    #[tokio::test]
    async fn a_route_probe_must_test_the_inference_endpoint() {
        let blackhole = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blackhole_addr = blackhole.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((stream, _)) = blackhole.accept().await else {
                return;
            };
            let _stream = stream;
            std::future::pending::<()>().await;
        });

        let inference = format!("http://{blackhole_addr}");
        let healthy_inference = tests_support::health_server(0).await;
        let table = RouteTable::new();
        table
            .measure_peer(
                &crate::peer_client::PeerClients::new(Duration::from_secs(10)),
                "n1",
                &[inference, healthy_inference.clone()],
            )
            .await;

        assert_eq!(
            table.resolve("n1"),
            Some(healthy_inference),
            "the probe must measure the inference port, not a different gateway"
        );
    }

    #[tokio::test]
    async fn current_route_rtt_is_measured_without_replacing_the_route() {
        let healthy = tests_support::health_server(0).await;
        let table = RouteTable::new();
        let http = crate::peer_client::PeerClients::new(Duration::from_secs(10));
        table
            .measure_peer(&http, "n1", std::slice::from_ref(&healthy))
            .await;
        let before = table.resolve("n1");

        let rtt = table.measure_current_peer(&http, "n1").await;

        assert!(rtt.is_some());
        assert_eq!(table.resolve("n1"), before);
    }

    /// A never measured peer resolves to nothing - the caller falls back to the
    /// plan's `inference_url`, which is today's behavior.
    #[test]
    fn an_unmeasured_peer_resolves_to_nothing() {
        assert_eq!(RouteTable::new().resolve("unknown"), None);
    }

    /// Just measured is not measured again: it keeps the probe cheap in the normal regime.
    #[tokio::test]
    async fn a_freshly_measured_peer_is_not_measured_again() {
        let alive = tests_support::health_server(0).await;
        let table = RouteTable::new();
        assert!(
            table.needs_measuring("n1"),
            "never measured needs measuring"
        );
        table
            .measure_peer(
                &crate::peer_client::PeerClients::new(Duration::from_secs(10)),
                "n1",
                &[alive],
            )
            .await;
        assert!(!table.needs_measuring("n1"));
    }
}
