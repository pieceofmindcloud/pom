//! Planner by delivery time: the chosen pipeline is the one that finishes the
//! request earliest, not the one with the lowest network latency.
//!
//! The cost of a path adds up, for this request:
//!
//! ```text
//! prefill: new_tokens × ms/token of each stage × (1 + stage queue)
//! decode:  expected_tokens × (ms/step of each stage × (1 + queue))
//! network: (1 + expected_tokens) × (RTT of each edge, including
//!          requester → head and tail → requester)
//! ```
//!
//! - **Network** comes from the pinned routes (`route_plan`): the requester measures
//!   its own edges; the edges between two other peers come from the `/health` of the
//!   origin peer (`route_rtt_ms`, `gateway_rtt_ms`).
//! - **Speed** comes from each peer's `/health` (`perf`): a test when serving and
//!   the average of the real traffic.
//! - **Balancing**: the queue (the peer's `active_generations`, or this gateway's
//!   in-flight load, whichever is larger) multiplies the stage's cost. A fast
//!   peer with a queue becomes slower than an idle average one, and the load spreads
//!   without artificial round-robin.
//! - **Prefix**: a path that already has this prompt's prefix pays only for the
//!   new tokens (`estimate_chain` with `cached_tokens`), so reuse
//!   wins when it pays off and loses when the cache's peer is saturated.
//!
//! Since the terms are sums per stage and per edge, the best path comes out of
//! a dynamic program over the layer ranges.

use std::collections::HashMap;

use serde::Serialize;

use crate::network::PeerInfo;
use crate::pipeline::{model_pipeline_candidates, plan_from_steps, step_for, PipelinePlan};

/// RTT assumed for an edge nobody has measured yet.
const UNKNOWN_RTT_MS: f64 = 100.0;

/// Cost per layer when no peer of the model has a measured speed. The
/// absolute value does not matter - it is the same for everyone -, it only needs to
/// be greater than zero for the queue to still weigh in the choice.
const FALLBACK_PREFILL_MS_PER_LAYER: f64 = 0.1;
const FALLBACK_DECODE_MS_PER_LAYER: f64 = 1.0;

/// What a peer's `/health` says about cost and load.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PeerPerf {
    pub prefill_ms_per_token: Option<f64>,
    pub decode_ms_per_step: Option<f64>,
    pub active_generations: u32,
    /// RTT of this peer's pinned route to another's inference server.
    pub route_rtt_ms: HashMap<String, f64>,
    /// RTT of this peer's pinned route to another's gateway.
    pub gateway_rtt_ms: HashMap<String, f64>,
    /// Transport (`quic`, `http`, `tunnel`) of this peer's pinned routes.
    pub route_kinds: HashMap<String, String>,
    pub gateway_route_kinds: HashMap<String, String>,
    /// Peers this one only reaches through a relay circuit (`relayed_peers`): a
    /// `tunnel` route to them goes through an intermediate server.
    pub relayed_peers: std::collections::HashSet<String>,
}

impl PeerPerf {
    pub fn from_health(body: &serde_json::Value) -> Self {
        let number =
            |value: &serde_json::Value| value.as_f64().filter(|v| v.is_finite() && *v > 0.0);
        let map = |value: Option<&serde_json::Value>| -> HashMap<String, f64> {
            value
                .and_then(serde_json::Value::as_object)
                .map(|object| {
                    object
                        .iter()
                        .filter_map(|(id, rtt)| number(rtt).map(|rtt| (id.clone(), rtt)))
                        .collect()
                })
                .unwrap_or_default()
        };
        let kinds = |value: Option<&serde_json::Value>| -> HashMap<String, String> {
            value
                .and_then(serde_json::Value::as_object)
                .map(|object| {
                    object
                        .iter()
                        .filter_map(|(id, kind)| {
                            kind.as_str().map(|kind| (id.clone(), kind.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let perf = body.get("perf");
        Self {
            prefill_ms_per_token: perf
                .and_then(|p| p.get("prefill_ms_per_token"))
                .and_then(number),
            decode_ms_per_step: perf
                .and_then(|p| p.get("decode_ms_per_step"))
                .and_then(number),
            active_generations: body
                .get("active_generations")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32,
            route_rtt_ms: map(body.get("route_rtt_ms")),
            gateway_rtt_ms: map(body.get("gateway_rtt_ms")),
            route_kinds: kinds(body.get("route_kinds")),
            gateway_route_kinds: kinds(body.get("gateway_route_kinds")),
            relayed_peers: body
                .get("relayed_peers")
                .and_then(serde_json::Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| id.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// Everything the estimate needs, already gathered by the gateway.
pub struct CostInputs<'a> {
    /// `node_id` of whoever originates the request.
    pub requester: &'a str,
    /// RTT from the requester to each peer's inference server.
    pub requester_rtt_ms: &'a HashMap<String, f64>,
    pub perf: &'a HashMap<String, PeerPerf>,
    /// In-flight requests this gateway has already sent to each peer.
    pub local_load: &'a HashMap<String, u32>,
    pub prompt_tokens: f64,
    pub decode_tokens: f64,
    /// Imposed return mode (`POM_RETURN_MODE`, for testing); `None` lets the
    /// planner choose.
    pub forced_return: Option<ReturnMode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageEstimate {
    pub node_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub prefill_ms_per_token: f64,
    pub decode_ms_per_step: f64,
    /// `false` when the cost is estimated from the others' per-layer average.
    pub measured: bool,
    pub queue: u32,
    /// RTT of the edge that arrives at this stage.
    pub rtt_in_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanEstimate {
    pub stages: Vec<StageEstimate>,
    /// RTT from the tail back to the requester; infinite when there is no return.
    pub return_rtt_ms: f64,
    /// Which way the response returns, chosen together with the path.
    pub return_mode: ReturnMode,
    pub cached_tokens: u32,
    pub prefill_ms: f64,
    /// Time of one decode token with the current queues: stages plus network.
    pub decode_ms_per_token: f64,
    pub total_ms: f64,
}

struct Speeds {
    prefill_per_layer: f64,
    decode_per_layer: f64,
}

fn layers(peer: &PeerInfo) -> f64 {
    (peer.end_layer - peer.start_layer + 1) as f64
}

/// Average cost per layer of the measured peers, to estimate those not yet measured.
fn fallback_speeds(candidates: &[&PeerInfo], perf: &HashMap<String, PeerPerf>) -> Speeds {
    let mut prefill = (0.0, 0.0);
    let mut decode = (0.0, 0.0);
    for peer in candidates {
        let Some(p) = perf.get(&peer.node_id) else {
            continue;
        };
        if let Some(ms) = p.prefill_ms_per_token {
            prefill.0 += ms;
            prefill.1 += layers(peer);
        }
        if let Some(ms) = p.decode_ms_per_step {
            decode.0 += ms;
            decode.1 += layers(peer);
        }
    }
    Speeds {
        prefill_per_layer: if prefill.1 > 0.0 {
            prefill.0 / prefill.1
        } else {
            FALLBACK_PREFILL_MS_PER_LAYER
        },
        decode_per_layer: if decode.1 > 0.0 {
            decode.0 / decode.1
        } else {
            FALLBACK_DECODE_MS_PER_LAYER
        },
    }
}

fn stage(peer: &PeerInfo, inputs: &CostInputs, speeds: &Speeds, rtt_in_ms: f64) -> StageEstimate {
    let perf = inputs.perf.get(&peer.node_id);
    let prefill = perf.and_then(|p| p.prefill_ms_per_token);
    let decode = perf.and_then(|p| p.decode_ms_per_step);
    let queue = perf
        .map(|p| p.active_generations)
        .unwrap_or(0)
        .max(inputs.local_load.get(&peer.node_id).copied().unwrap_or(0));
    StageEstimate {
        node_id: peer.node_id.clone(),
        start_layer: peer.start_layer,
        end_layer: peer.end_layer,
        prefill_ms_per_token: prefill.unwrap_or(speeds.prefill_per_layer * layers(peer)),
        decode_ms_per_step: decode.unwrap_or(speeds.decode_per_layer * layers(peer)),
        measured: prefill.is_some() && decode.is_some(),
        queue,
        rtt_in_ms,
    }
}

/// RTT from `from` to the inference server of `to`.
fn edge_rtt(from: &str, to: &str, inputs: &CostInputs) -> f64 {
    measured_edge_rtt(from, to, inputs).unwrap_or(UNKNOWN_RTT_MS)
}

/// `edge_rtt` without a guess: `None` when nobody measured the edge.
fn measured_edge_rtt(from: &str, to: &str, inputs: &CostInputs) -> Option<f64> {
    if from == to {
        return Some(0.0);
    }
    if from == inputs.requester {
        return inputs.requester_rtt_ms.get(to).copied();
    }
    inputs
        .perf
        .get(from)
        .and_then(|p| p.route_rtt_ms.get(to).copied())
        // Without a measurement from the origin peer, the gateway's one to the
        // destination is the best guess available.
        .or_else(|| inputs.requester_rtt_ms.get(to).copied())
}

/// Which way each token's response returns to the requester.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReturnMode {
    /// The tail delivers directly to the gateway (`reply_to`).
    Direct,
    /// The response returns through the same connections as the way out.
    Ring,
}

impl ReturnMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReturnMode::Direct => "direct",
            ReturnMode::Ring => "ring",
        }
    }

    /// Fraction of the RTT of each outbound edge that the mode pays per token: the
    /// way out is only the arrival path in the direct response, and round trip in the ring.
    fn forward_weight(self) -> f64 {
        match self {
            ReturnMode::Direct => 0.5,
            ReturnMode::Ring => 1.0,
        }
    }
}

/// `POM_RETURN_MODE=direct|ring` imposes the return mode, to test a path
/// the planner would not choose - for example, direct delivery to a
/// gateway behind NAT, which the ring would hide. Absent or `auto`, the
/// planner decides. With `direct`, a return that does not exist becomes an inference
/// failure instead of being worked around.
pub fn forced_return_mode() -> Option<ReturnMode> {
    match crate::brand::env("RETURN_MODE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "direct" | "direct" => Some(ReturnMode::Direct),
        "ring" | "ring" => Some(ReturnMode::Ring),
        _ => None,
    }
}

/// Measured RTT from the tail back to the requester. `None` when the tail has already
/// measured routes to other gateways and has none to this one: with no return, the
/// direct response is not viable. With no measurement at all from the tail, the guess
/// is this gateway's RTT to it.
fn return_rtt(tail: &str, inputs: &CostInputs) -> Option<f64> {
    match measured_return_rtt(tail, inputs) {
        ReturnLeg::Measured(rtt) => Some(rtt),
        ReturnLeg::Missing => None,
        ReturnLeg::Unmeasured => Some(UNKNOWN_RTT_MS),
    }
}

enum ReturnLeg {
    Measured(f64),
    /// The tail measured routes, but none to this gateway.
    Missing,
    /// Nobody has measured anything yet.
    Unmeasured,
}

fn measured_return_rtt(tail: &str, inputs: &CostInputs) -> ReturnLeg {
    if tail == inputs.requester {
        return ReturnLeg::Measured(0.0);
    }
    let from_gateway = || match inputs.requester_rtt_ms.get(tail) {
        Some(rtt) => ReturnLeg::Measured(*rtt),
        None => ReturnLeg::Unmeasured,
    };
    match inputs.perf.get(tail) {
        Some(perf) => match perf.gateway_rtt_ms.get(inputs.requester) {
            Some(rtt) => ReturnLeg::Measured(*rtt),
            None if !perf.gateway_rtt_ms.is_empty() => ReturnLeg::Missing,
            None => from_gateway(),
        },
        None => from_gateway(),
    }
}

/// Cost of a stage in this request, with the queue applied.
fn stage_cost(stage: &StageEstimate, prompt_tokens: f64, decode_tokens: f64) -> f64 {
    let load = 1.0 + stage.queue as f64;
    (prompt_tokens * stage.prefill_ms_per_token + decode_tokens * stage.decode_ms_per_step) * load
}

fn edge_cost(rtt_ms: f64, decode_tokens: f64) -> f64 {
    (1.0 + decode_tokens) * rtt_ms
}

/// Network of one token on the path, by return mode: half an RTT per outbound edge
/// plus half an RTT from the tail to the gateway in the direct response; the whole
/// RTT of each edge in the ring. `None` when the mode has no return.
fn network_ms(
    stages: &[StageEstimate],
    return_rtt_ms: Option<f64>,
    mode: ReturnMode,
) -> Option<f64> {
    let forward: f64 = stages.iter().map(|s| s.rtt_in_ms).sum::<f64>() * mode.forward_weight();
    match mode {
        ReturnMode::Direct => return_rtt_ms.map(|back| forward + back / 2.0),
        ReturnMode::Ring => Some(forward),
    }
}

/// Estimates an already chosen path, by the best return mode. `cached_tokens`
/// of the prompt do not go through prefill (prefix already retained on that path).
pub fn estimate_chain(
    chain: &[&PeerInfo],
    inputs: &CostInputs,
    candidates: &[&PeerInfo],
    cached_tokens: u32,
) -> PlanEstimate {
    let speeds = fallback_speeds(candidates, inputs.perf);
    let prompt = (inputs.prompt_tokens - cached_tokens as f64).max(1.0);
    let mut stages = Vec::with_capacity(chain.len());
    let mut from = inputs.requester.to_string();
    for peer in chain {
        let rtt = edge_rtt(&from, &peer.node_id, inputs);
        stages.push(stage(peer, inputs, &speeds, rtt));
        from = peer.node_id.clone();
    }
    let return_rtt_ms = chain
        .last()
        .and_then(|tail| return_rtt(&tail.node_id, inputs));
    let modes = match inputs.forced_return {
        Some(mode) => vec![mode],
        None => vec![ReturnMode::Direct, ReturnMode::Ring],
    };
    let (return_mode, network) = modes
        .into_iter()
        .filter_map(|mode| network_ms(&stages, return_rtt_ms, mode).map(|ms| (mode, ms)))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or((ReturnMode::Ring, 0.0));
    let prefill_ms = stages
        .iter()
        .map(|s| prompt * s.prefill_ms_per_token * (1.0 + s.queue as f64))
        .sum::<f64>()
        + network;
    let decode_ms_per_token = stages
        .iter()
        .map(|s| s.decode_ms_per_step * (1.0 + s.queue as f64))
        .sum::<f64>()
        + network;
    let total_ms = stages
        .iter()
        .map(|s| stage_cost(s, prompt, inputs.decode_tokens))
        .sum::<f64>()
        + edge_cost(network, inputs.decode_tokens);
    PlanEstimate {
        stages,
        return_rtt_ms: return_rtt_ms.unwrap_or(f64::INFINITY),
        return_mode,
        cached_tokens,
        prefill_ms,
        decode_ms_per_token,
        total_ms,
    }
}

/// A path as the dashboard shows it: the same sources and the same computation as
/// the planner, but without guessing - an unmeasured leg stays `None` and no total
/// is closed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChainView {
    /// Round trip RTT of each leg: requester -> head, each handoff
    /// between stages and, if the tail is not the requester, tail -> requester.
    pub legs_ms: Vec<Option<f64>>,
    /// Return mode the planner would choose for this path.
    pub return_mode: Option<ReturnMode>,
    /// Network paid per token in the chosen mode (not the sum of the legs: in the
    /// direct response each leg costs half an RTT; in the ring the tail's return
    /// does not exist).
    pub network_ms: Option<f64>,
    /// Compute of one decode step summed over the stages, with the queue.
    pub compute_ms: Option<f64>,
    /// One token along the route: network in the chosen mode plus compute.
    pub decode_ms_per_token: Option<f64>,
}

pub fn describe_chain(chain: &[&PeerInfo], inputs: &CostInputs) -> ChainView {
    let mut legs_ms = Vec::with_capacity(chain.len() + 1);
    let mut from = inputs.requester;
    for peer in chain {
        legs_ms.push(measured_edge_rtt(from, &peer.node_id, inputs));
        from = &peer.node_id;
    }
    let back = match chain.last() {
        Some(tail) if tail.node_id != inputs.requester => {
            let leg = measured_return_rtt(&tail.node_id, inputs);
            legs_ms.push(match leg {
                ReturnLeg::Measured(rtt) => Some(rtt),
                _ => None,
            });
            leg
        }
        _ => ReturnLeg::Measured(0.0),
    };
    let forward = legs_ms[..chain.len()]
        .iter()
        .copied()
        .collect::<Option<Vec<_>>>()
        .map(|legs| legs.into_iter().sum::<f64>());
    let modes = match inputs.forced_return {
        Some(mode) => vec![mode],
        None => vec![ReturnMode::Direct, ReturnMode::Ring],
    };
    // Without the return measured, the planner would compare against a guess; the
    // dashboard does not choose the mode for it.
    let undecided = matches!(back, ReturnLeg::Unmeasured) && modes.contains(&ReturnMode::Direct);
    let chosen = forward.filter(|_| !undecided).and_then(|forward| {
        modes
            .into_iter()
            .filter_map(|mode| {
                let network = match (mode, &back) {
                    (ReturnMode::Ring, _) => forward * mode.forward_weight(),
                    (ReturnMode::Direct, ReturnLeg::Measured(back)) => {
                        forward * mode.forward_weight() + back / 2.0
                    }
                    (ReturnMode::Direct, _) => return None,
                };
                Some((mode, network))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
    });
    let compute_ms = chain
        .iter()
        .map(|peer| {
            let perf = inputs.perf.get(&peer.node_id)?;
            let queue = perf
                .active_generations
                .max(inputs.local_load.get(&peer.node_id).copied().unwrap_or(0));
            perf.decode_ms_per_step.map(|ms| ms * (1.0 + queue as f64))
        })
        .collect::<Option<Vec<_>>>()
        .map(|steps| steps.into_iter().sum::<f64>());
    ChainView {
        legs_ms,
        return_mode: chosen.map(|(mode, _)| mode),
        network_ms: chosen.map(|(_, ms)| ms),
        compute_ms,
        decode_ms_per_token: chosen
            .zip(compute_ms)
            .map(|((_, network), compute)| network + compute),
    }
}

/// The path and the return mode with the lowest delivery time, and the estimate.
///
/// The return mode is part of the planning, not after it: the same path
/// costs differently with direct response and with ring, and the best path for one
/// mode may not be the other's. So the dynamic program runs once
/// per mode and the lowest total wins.
///
/// Only a peer whose range starts exactly where the previous one ends gets in (the
/// same rule as the greedy planner). `preferred_head`, when chosen in the
/// interface, is the only head accepted: an invalid choice fails closed.
pub fn plan_fastest(
    peers: &[PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
    total_layers: u32,
    inputs: &CostInputs,
    preferred_head: Option<&str>,
) -> Option<(PipelinePlan, PlanEstimate)> {
    let mut candidates = model_pipeline_candidates(peers, model_name, deploy_id);
    candidates.sort_by(|a, b| {
        a.start_layer
            .cmp(&b.start_layer)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    let speeds = fallback_speeds(&candidates, inputs.perf);

    let modes = match inputs.forced_return {
        Some(mode) => vec![mode],
        None => vec![ReturnMode::Direct, ReturnMode::Ring],
    };
    let mut winner: Option<(f64, Vec<&PeerInfo>)> = None;
    for mode in modes {
        let weight = mode.forward_weight();
        // Best accumulated cost up to the end of each candidate, and where it came from.
        let mut best: HashMap<&str, (f64, Option<usize>)> = HashMap::new();
        for (index, peer) in candidates.iter().enumerate() {
            let own = |rtt: f64| {
                stage_cost(
                    &stage(peer, inputs, &speeds, rtt),
                    inputs.prompt_tokens,
                    inputs.decode_tokens,
                ) + edge_cost(rtt * weight, inputs.decode_tokens)
            };
            let entry = if peer.start_layer == 0 {
                if preferred_head.is_some_and(|head| head != peer.node_id) {
                    continue;
                }
                Some((own(edge_rtt(inputs.requester, &peer.node_id, inputs)), None))
            } else {
                candidates[..index]
                    .iter()
                    .enumerate()
                    .filter(|(_, prev)| prev.end_layer + 1 == peer.start_layer)
                    .filter_map(|(prev_index, prev)| {
                        best.get(prev.node_id.as_str()).map(|(cost, _)| {
                            (
                                cost + own(edge_rtt(&prev.node_id, &peer.node_id, inputs)),
                                Some(prev_index),
                            )
                        })
                    })
                    .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
            };
            if let Some(entry) = entry {
                best.insert(peer.node_id.as_str(), entry);
            }
        }

        let tail = candidates
            .iter()
            .enumerate()
            .filter(|(_, peer)| peer.end_layer + 1 == total_layers)
            .filter_map(|(index, peer)| {
                let (cost, _) = best.get(peer.node_id.as_str())?;
                let back = match mode {
                    ReturnMode::Direct => edge_cost(
                        return_rtt(&peer.node_id, inputs)? / 2.0,
                        inputs.decode_tokens,
                    ),
                    ReturnMode::Ring => 0.0,
                };
                Some((index, cost + back))
            })
            .min_by(|a, b| {
                a.1.partial_cmp(&b.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| candidates[a.0].node_id.cmp(&candidates[b.0].node_id))
            });
        let Some((tail_index, total)) = tail else {
            continue;
        };
        let mut chain = Vec::new();
        let mut cursor = Some(tail_index);
        while let Some(index) = cursor {
            let peer = candidates[index];
            chain.push(peer);
            cursor = best.get(peer.node_id.as_str()).and_then(|(_, prev)| *prev);
        }
        chain.reverse();
        if winner.as_ref().is_none_or(|(cost, _)| total < *cost) {
            winner = Some((total, chain));
        }
    }

    let (_, chain) = winner?;
    let plan = plan_for_chain(&chain)?;
    // The estimate redoes the mode choice for the winning path, with the
    // same computation; it is from it that the gateway reads the mode.
    let estimate = estimate_chain(&chain, inputs, &candidates, 0);
    Some((plan, estimate))
}

/// Why the response must return through the ring instead of being delivered directly
/// to the gateway, or `None` when direct delivery works.
///
/// Whoever delivers directly is the tail (direct response) or the head (remote-stream).
/// Each one publishes in `/health` the pinned route it has to each gateway
/// (`gateway_route_kinds`). The ring returns the response through the same connection
/// as the way out, so it is preferable when that node:
///
/// - has no measured route to this gateway, although it has already measured routes
///   to others (no return: a gateway behind NAT that only the way out reaches); or
/// - only returns through the p2p tunnel while the way out to it is all direct
///   (QUIC or HTTP) - the tunnel is usually the slowest and most fragile path.
///
/// Without data from the node (not probed yet, plan not measured yet) nothing changes.
pub fn return_path_prefers_ring(
    chain: &[String],
    requester: &str,
    perf: &HashMap<String, PeerPerf>,
    requester_route_kinds: &HashMap<String, String>,
) -> Option<String> {
    let forward_kind = |index: usize| -> Option<&str> {
        if index == 0 {
            if chain[0] == requester {
                return Some("local");
            }
            return requester_route_kinds.get(&chain[0]).map(String::as_str);
        }
        perf.get(&chain[index - 1])
            .and_then(|p| p.route_kinds.get(&chain[index]))
            .map(String::as_str)
    };
    let deliverers = [chain.first(), chain.last()];
    for (position, deliverer) in deliverers.into_iter().enumerate() {
        let Some(deliverer) = deliverer else {
            continue;
        };
        if deliverer == requester {
            continue;
        }
        let Some(deliverer_perf) = perf.get(deliverer) else {
            continue;
        };
        let back = deliverer_perf
            .gateway_route_kinds
            .get(requester)
            .map(String::as_str);
        let short = deliverer.get(..12).unwrap_or(deliverer);
        match back {
            None if !deliverer_perf.gateway_route_kinds.is_empty() => {
                return Some(format!("{short} has no route back to this gateway"));
            }
            Some("tunnel") => {
                let upto = if position == 0 { 0 } else { chain.len() - 1 };
                let forward_direct = (0..=upto)
                    .all(|index| matches!(forward_kind(index), Some("quic" | "http" | "local")));
                if forward_direct {
                    return Some(format!(
                        "{short} only comes back through the p2p tunnel and the outbound path is direct"
                    ));
                }
            }
            _ => {}
        }
    }
    None
}

/// The model's candidates, in the order the planner considers them.
pub fn candidates<'a>(
    peers: &'a [PeerInfo],
    model_name: &str,
    deploy_id: Option<&str>,
) -> Vec<&'a PeerInfo> {
    model_pipeline_candidates(peers, model_name, deploy_id)
}

/// The first stage of an unpinned request: round robin over the layer-0 `heads` (in the order given), starting at `turn`,
/// so requests that arrive together land on different heads. A head with generations in flight (`queue`) is passed
/// over for the next one; when every head is busy, the least loaded wins, the earliest in the rotation on a tie.
pub fn rotate_head(heads: &[String], turn: usize, queue: impl Fn(&str) -> u32) -> Option<String> {
    if heads.is_empty() {
        return None;
    }
    let n = heads.len();
    let order: Vec<&String> = (0..n).map(|i| &heads[(turn + i) % n]).collect();
    if let Some(free) = order.iter().find(|head| queue(head) == 0) {
        return Some((*free).clone());
    }
    order.into_iter().min_by_key(|head| queue(head)).cloned()
}

/// Resolves a chain of `node_id`s (for example, that of a cached prefix)
/// against the live candidates. `None` if any is gone or if the chain does not cover
/// `0..total_layers` in contiguous ranges.
pub fn resolve_chain<'a>(
    candidates: &[&'a PeerInfo],
    node_ids: &[String],
    total_layers: u32,
    preferred_head: Option<&str>,
) -> Option<Vec<&'a PeerInfo>> {
    let mut chain = Vec::with_capacity(node_ids.len());
    let mut pos = 0;
    for node_id in node_ids {
        let peer = *candidates.iter().find(|peer| peer.node_id == *node_id)?;
        if peer.start_layer != pos {
            return None;
        }
        pos = peer.end_layer + 1;
        chain.push(peer);
    }
    if pos != total_layers {
        return None;
    }
    if let Some(head) = preferred_head {
        if chain.first().map(|peer| peer.node_id.as_str()) != Some(head) {
            return None;
        }
    }
    Some(chain)
}

pub fn plan_for_chain(chain: &[&PeerInfo]) -> Option<PipelinePlan> {
    let steps = chain
        .iter()
        .map(|peer| step_for(peer, peer.start_layer))
        .collect::<Option<Vec<_>>>()?;
    Some(plan_from_steps(steps))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_spreads_requests_that_arrive_together() {
        let heads: Vec<String> = ["a", "b", "c"].iter().map(|h| h.to_string()).collect();
        let idle = |_: &str| 0;
        assert_eq!(rotate_head(&heads, 0, idle).as_deref(), Some("a"));
        assert_eq!(rotate_head(&heads, 1, idle).as_deref(), Some("b"));
        assert_eq!(rotate_head(&heads, 2, idle).as_deref(), Some("c"));
        assert_eq!(rotate_head(&heads, 3, idle).as_deref(), Some("a"));
        assert_eq!(rotate_head(&[], 0, idle), None);
    }

    #[test]
    fn a_busy_head_is_passed_over_for_the_next_free_one() {
        let heads: Vec<String> = ["a", "b", "c"].iter().map(|h| h.to_string()).collect();
        let load = |h: &str| if h == "b" { 2 } else { 0 };
        assert_eq!(rotate_head(&heads, 1, load).as_deref(), Some("c"), "b is busy: the next one in the rotation");
        let all_busy = |h: &str| match h { "a" => 3, "b" => 1, _ => 2 };
        assert_eq!(rotate_head(&heads, 0, all_busy).as_deref(), Some("b"), "all busy: the least loaded");
    }

    fn peer(node_id: &str, start: u32, end: u32) -> PeerInfo {
        PeerInfo {
            deploy_id: String::new(),
            tls: false,
            node_id: node_id.to_string(),
            pubkey: "p".into(),
            cluster_id: None,
            trust_version: None,
            membership: None,
            relay: None,
            addr: "10.0.0.1:42777".into(),
            addr_candidates: Vec::new(),
            model: "m".into(),
            start_layer: start,
            end_layer: end,
            total_layers: 48,
            memory_mb: 0.0,
            last_seen: chrono::Utc::now(),
            inference_port: Some(8181),
            inference_quic_port: None,
            gateway_port: Some(8090),
            gateway_quic_port: None,
            profile: crate::profile::NodeProfile::Worker,
            hostname: String::new(),
            capabilities: Vec::new(),
            connection_verified: false,
            disk_total_mb: 0,
            disk_free_mb: 0,
            api_keys_active: 0,
            inference_frame_capabilities: Vec::new(),
            endpoint_id: None,
            endpoint_addrs: Vec::new(),
            tp: None,
            runtime_capacity: None,
            idle: false,
        }
    }

    fn perf(prefill: f64, decode: f64, active: u32) -> PeerPerf {
        PeerPerf {
            prefill_ms_per_token: Some(prefill),
            decode_ms_per_step: Some(decode),
            active_generations: active,
            ..PeerPerf::default()
        }
    }

    fn chain_of(plan: &PipelinePlan) -> Vec<String> {
        plan.steps.iter().map(|step| step.node_id.clone()).collect()
    }

    /// A distant but fast head beats the near and slow one when it finishes earlier.
    #[test]
    fn a_distant_fast_head_beats_a_near_slow_one() {
        let peers = vec![
            peer("near", 0, 27),
            peer("far", 0, 27),
            peer("tail", 28, 47),
        ];
        let rtt = HashMap::from([
            ("near".to_string(), 1.0),
            ("far".to_string(), 30.0),
            ("tail".to_string(), 20.0),
        ]);
        let perfs = HashMap::from([
            ("near".to_string(), perf(2.0, 80.0, 0)),
            ("far".to_string(), perf(0.5, 15.0, 0)),
            ("tail".to_string(), perf(1.0, 20.0, 0)),
        ]);
        let load = HashMap::new();
        let inputs = CostInputs {
            requester: "gateway",
            requester_rtt_ms: &rtt,
            perf: &perfs,
            local_load: &load,
            prompt_tokens: 200.0,
            decode_tokens: 200.0,
            forced_return: None,
        };
        let (plan, estimate) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["far", "tail"]);
        assert!(estimate.total_ms > 0.0);
        // Little output: the network advantage of the near head weighs in again.
        let short = CostInputs {
            decode_tokens: 0.0,
            prompt_tokens: 1.0,
            ..inputs
        };
        let (plan, _) = plan_fastest(&peers, "m", None, 48, &short, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["near", "tail"]);
    }

    /// The queue multiplies the cost: the busy fastest peer gives way to the
    /// idle average one, and the load spreads.
    #[test]
    fn a_busy_fast_peer_yields_to_an_idle_one() {
        let peers = vec![peer("fast", 0, 47), peer("slow", 0, 47)];
        let rtt = HashMap::from([("fast".to_string(), 5.0), ("slow".to_string(), 5.0)]);
        let idle = HashMap::from([
            ("fast".to_string(), perf(1.0, 20.0, 0)),
            ("slow".to_string(), perf(1.5, 30.0, 0)),
        ]);
        let load = HashMap::new();
        let inputs = CostInputs {
            requester: "gateway",
            requester_rtt_ms: &rtt,
            perf: &idle,
            local_load: &load,
            prompt_tokens: 100.0,
            decode_tokens: 100.0,
            forced_return: None,
        };
        let (plan, _) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["fast"]);

        let busy = HashMap::from([
            ("fast".to_string(), perf(1.0, 20.0, 2)),
            ("slow".to_string(), perf(1.5, 30.0, 0)),
        ]);
        let inputs = CostInputs {
            perf: &busy,
            ..inputs
        };
        let (plan, _) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["slow"]);
    }

    /// The edges between peers enter the computation: the middle stage that talks
    /// fast to the chosen head beats the one that is only fast for the gateway.
    #[test]
    fn inter_hop_latency_decides_between_equal_replicas() {
        let peers = vec![
            peer("head", 0, 23),
            peer("mid-a", 24, 47),
            peer("mid-b", 24, 47),
        ];
        let rtt = HashMap::from([
            ("head".to_string(), 5.0),
            ("mid-a".to_string(), 5.0),
            ("mid-b".to_string(), 40.0),
        ]);
        let mut head = perf(1.0, 10.0, 0);
        head.route_rtt_ms =
            HashMap::from([("mid-a".to_string(), 60.0), ("mid-b".to_string(), 2.0)]);
        let perfs = HashMap::from([
            ("head".to_string(), head),
            ("mid-a".to_string(), perf(1.0, 10.0, 0)),
            ("mid-b".to_string(), perf(1.0, 10.0, 0)),
        ]);
        let load = HashMap::new();
        let inputs = CostInputs {
            requester: "gateway",
            requester_rtt_ms: &rtt,
            perf: &perfs,
            local_load: &load,
            prompt_tokens: 50.0,
            decode_tokens: 50.0,
            forced_return: None,
        };
        let (plan, estimate) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(chain_of(&plan), vec!["head", "mid-b"]);
        assert_eq!(estimate.stages[1].rtt_in_ms, 2.0);
    }

    #[test]
    fn the_chosen_head_is_the_only_head_and_a_cached_chain_is_resolved() {
        let peers = vec![peer("a", 0, 47), peer("b", 0, 47)];
        let rtt = HashMap::new();
        let perfs = HashMap::from([
            ("a".to_string(), perf(1.0, 10.0, 0)),
            ("b".to_string(), perf(5.0, 50.0, 0)),
        ]);
        let load = HashMap::new();
        let inputs = CostInputs {
            requester: "gateway",
            requester_rtt_ms: &rtt,
            perf: &perfs,
            local_load: &load,
            prompt_tokens: 1000.0,
            decode_tokens: 10.0,
            forced_return: None,
        };
        let (plan, _) = plan_fastest(&peers, "m", None, 48, &inputs, Some("b")).unwrap();
        assert_eq!(chain_of(&plan), vec!["b"]);
        assert!(plan_fastest(&peers, "m", None, 48, &inputs, Some("ghost")).is_none());

        // The cached prefix discounts the prefill: the slow peer with 990 tokens
        // already retained now delivers earlier.
        let all = candidates(&peers, "m", None);
        let chain = resolve_chain(&all, &["b".to_string()], 48, None).unwrap();
        let cached = estimate_chain(&chain, &inputs, &all, 990);
        let a_chain = resolve_chain(&all, &["a".to_string()], 48, None).unwrap();
        let cold = estimate_chain(&a_chain, &inputs, &all, 0);
        assert!(cached.total_ms < cold.total_ms);
        assert!(resolve_chain(&all, &["x".to_string()], 48, None).is_none());
    }

    fn perf_with_back(kind: Option<&str>, others: bool) -> PeerPerf {
        let mut perf = PeerPerf::default();
        if let Some(kind) = kind {
            perf.gateway_route_kinds
                .insert("gateway".into(), kind.into());
        }
        if others {
            perf.gateway_route_kinds
                .insert("other".into(), "http".into());
        }
        perf
    }

    #[test]
    fn the_answer_returns_by_the_ring_when_the_tail_cannot_reach_the_gateway() {
        let chain = vec!["head".to_string(), "tail".to_string()];
        let own = HashMap::from([("head".to_string(), "quic".to_string())]);
        let mut head = perf_with_back(Some("quic"), false);
        head.route_kinds.insert("tail".into(), "quic".into());

        // No return route, having measured others: ring.
        let perf = HashMap::from([
            ("head".to_string(), head.clone()),
            ("tail".to_string(), perf_with_back(None, true)),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_some());

        // Return only through the tunnel with the way out all on QUIC: ring.
        let perf = HashMap::from([
            ("head".to_string(), head.clone()),
            ("tail".to_string(), perf_with_back(Some("tunnel"), false)),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_some());

        // Direct return: direct delivery.
        let perf = HashMap::from([
            ("head".to_string(), head.clone()),
            ("tail".to_string(), perf_with_back(Some("http"), false)),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_none());

        // The way out is also through the tunnel: the ring gains nothing.
        let mut tunneled = head.clone();
        tunneled.route_kinds.insert("tail".into(), "tunnel".into());
        let perf = HashMap::from([
            ("head".to_string(), tunneled),
            ("tail".to_string(), perf_with_back(Some("tunnel"), false)),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_none());

        // Node still with no measurement at all: nothing changes.
        let perf = HashMap::from([
            ("head".to_string(), head),
            ("tail".to_string(), PeerPerf::default()),
        ]);
        assert!(return_path_prefers_ring(&chain, "gateway", &perf, &own).is_none());
    }

    /// The return mode comes out of the planning: the gateway is the head (L0-15) and the
    /// tail is on another machine. Way out on QUIC with 26 ms.
    #[test]
    fn the_return_mode_is_chosen_with_the_path() {
        let peers = vec![peer("gw", 0, 15), peer("tail", 16, 47)];
        let rtt = HashMap::from([("tail".to_string(), 26.0)]);
        let load = HashMap::new();
        let plan_with = |tail_back: HashMap<String, f64>| {
            let mut tail = perf(1.0, 10.0, 0);
            tail.gateway_rtt_ms = tail_back;
            let perfs = HashMap::from([
                ("gw".to_string(), perf(1.0, 10.0, 0)),
                ("tail".to_string(), tail),
            ]);
            let inputs = CostInputs {
                requester: "gw",
                requester_rtt_ms: &rtt,
                perf: &perfs,
                local_load: &load,
                prompt_tokens: 50.0,
                decode_tokens: 50.0,
                forced_return: None,
            };
            plan_fastest(&peers, "m", None, 48, &inputs, None)
                .unwrap()
                .1
                .return_mode
        };
        // Return only through the tunnel, 48 ms: direct would cost 13 + 24 = 37 ms per
        // token, the ring 26 ms.
        assert_eq!(
            plan_with(HashMap::from([("gw".to_string(), 48.0)])),
            ReturnMode::Ring
        );
        // Tail with no return to this gateway (it measured only another one): ring.
        assert_eq!(
            plan_with(HashMap::from([("other".to_string(), 5.0)])),
            ReturnMode::Ring
        );
        // Fast return, 20 ms: direct costs 13 + 10 = 23 ms, below the ring.
        assert_eq!(
            plan_with(HashMap::from([("gw".to_string(), 20.0)])),
            ReturnMode::Direct
        );
    }

    #[test]
    fn a_forced_return_mode_is_the_only_one_planned() {
        let peers = vec![peer("gw", 0, 15), peer("tail", 16, 47)];
        let rtt = HashMap::from([("tail".to_string(), 26.0)]);
        let load = HashMap::new();
        let mut tail = perf(1.0, 10.0, 0);
        tail.gateway_rtt_ms = HashMap::from([("gw".to_string(), 48.0)]);
        let perfs = HashMap::from([
            ("gw".to_string(), perf(1.0, 10.0, 0)),
            ("tail".to_string(), tail),
        ]);
        let inputs = CostInputs {
            requester: "gw",
            requester_rtt_ms: &rtt,
            perf: &perfs,
            local_load: &load,
            prompt_tokens: 50.0,
            decode_tokens: 50.0,
            forced_return: Some(ReturnMode::Direct),
        };
        // On its own the planner would choose the ring; forced, it is direct.
        let (_, estimate) = plan_fastest(&peers, "m", None, 48, &inputs, None).unwrap();
        assert_eq!(estimate.return_mode, ReturnMode::Direct);
    }

    #[test]
    fn health_body_becomes_peer_perf() {
        let body = serde_json::json!({
            "active_generations": 2,
            "perf": {"prefill_ms_per_token": 0.8, "decode_ms_per_step": 24.5},
            "route_rtt_ms": {"peer-b": 26.0},
            "gateway_rtt_ms": {"peer-g": 3.5, "bad": -1},
            "relayed_peers": ["peer-g"],
        });
        let perf = PeerPerf::from_health(&body);
        assert_eq!(perf.active_generations, 2);
        assert_eq!(perf.decode_ms_per_step, Some(24.5));
        assert_eq!(perf.route_rtt_ms["peer-b"], 26.0);
        assert_eq!(perf.gateway_rtt_ms.len(), 1);
        assert!(perf.relayed_peers.contains("peer-g"));
    }
}
