//! Putting a model that is already sliced into service across the cluster.
//!
//! The pieces all existed and none of them was the path: the catalog knows who
//! holds which slice, `transfer` moves a file between two machines, and `launch`
//! binds a range to a peer. What was missing was the decision in the middle —
//! *which* machine should serve *which* slice, and what has to be copied first
//! for that to be possible. This module is only that decision, plus the
//! sequencing to carry it out; it starts no transfer mechanism and no launch
//! mechanism of its own.
//!
//! Two routes, deliberately split the way the slicing screen already splits:
//! `plan` is read-only and cheap enough for the screen to call while the
//! operator is still choosing, and `deploy` re-surveys the cluster rather than
//! trusting the preview it was handed.
//!
//! ## The operator's choice is part of the request
//!
//! The planner is a *suggestion*, not the decision. `selection` carries the
//! operator's own answer to the two questions the planner guesses at — which
//! ranges to put on the wire, and which machine takes each one — and everything
//! below validates that answer instead of quietly recomputing its own. Omitting
//! `selection` keeps the old behaviour exactly: the node plans it, and the
//! screen shows what it planned.
//!
//! Three things fall out of the architecture rather than out of taste, and the
//! validation leans on all three:
//!
//! * **The pipeline order is the layer order.** `pipeline.rs` sweeps from layer
//!   0 upwards, so the machine holding layer 0 is the one that receives every
//!   prompt. "Choosing the order" is therefore the same act as choosing who
//!   takes which range — there is no second knob, and pretending otherwise
//!   would be a lie on screen.
//! * **A node serves one range.** Two selected ranges on the same machine is a
//!   refusal, not a warning.
//! * **Overlap is not a preference.** Two selected ranges covering the same
//!   layer would run those layers twice and corrupt the answer, so a set that
//!   overlaps is refused. A *gap* is different: it is a legitimate half-deploy
//!   and only stops the pipeline from planning, so it warns.

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tracing::{info, warn};

use crate::auth::ConnectionAuth;
use crate::gateway::GatewayState;
use crate::network::PeerInfo;
use crate::plan::DEFAULT_RESERVE_PCT;
use crate::webui::{
    jobs, launch, new_job, set_job, set_job_progress, LaunchRequest, LaunchTarget, RuntimeParams,
    PEER_TIMEOUT,
};

const MIB: u64 = 1024 * 1024;

pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/deploy/plan", post(plan_endpoint))
        .route("/api/ui/deploy", post(deploy_endpoint))
}

/// One line of the operator's own answer: this range, on that machine.
#[derive(Debug, Clone, Deserialize)]
pub struct SelectedSlice {
    pub start_layer: u32,
    pub end_layer: u32,
    /// Empty means the operator has not chosen a machine for it yet, which is a
    /// refusal rather than an invitation for the node to pick one.
    #[serde(default)]
    pub target_node_id: String,
    /// Layers this machine puts on its GPU, when it should not use the
    /// cluster-wide number. See `LaunchTarget::n_gpu_layers` for why this is the
    /// one setting that varies per machine.
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct DeployRequest {
    /// The shard set, named the way the catalog groups it.
    pub group: String,
    #[serde(default)]
    pub params: RuntimeParams,
    /// The operator's explicit choice. `None` is the default path: the node
    /// decides, and the screen shows the decision.
    #[serde(default)]
    pub selection: Option<Vec<SelectedSlice>>,
}

// ── What a machine can tell us about itself ──────────────────────────────────

/// One slice of the chosen set, as it exists on one machine.
#[derive(Debug, Clone)]
struct HeldShard {
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    name: String,
    path: String,
    size_bytes: u64,
}

/// A machine in the cluster, with the slices of this set it already holds.
struct Machine {
    node_id: String,
    hostname: String,
    /// Memory the weights would have to fit in: free VRAM on a machine with a
    /// discrete GPU, available RAM anywhere else. `0` means the machine did not
    /// answer, and nothing is inferred from that.
    mem_free_mb: u64,
    /// Room for the file itself, which is what a copy needs and what
    /// `transfer.rs` checks before it writes a byte.
    disk_free_mb: u64,
    held: Vec<HeldShard>,
}

impl Machine {
    fn holds(&self, start: u32, end: u32) -> Option<&HeldShard> {
        self.held
            .iter()
            .find(|s| s.start_layer == start && s.end_layer == end)
    }
    fn label(&self) -> String {
        if self.hostname.is_empty() {
            self.node_id[..12.min(self.node_id.len())].to_string()
        } else {
            self.hostname.clone()
        }
    }
}

/// Ask every peer what it has, in parallel.
///
/// Reuses each node's own `/api/ui/models`, which is the same answer the
/// cluster catalog screen shows — a machine is the only honest source for where
/// its own files are, and for a path we would otherwise have to guess — and its
/// `/api/ui/node`, which is the only honest source for how much room it has.
async fn survey(state: &GatewayState, group: &str) -> Vec<Machine> {
    let peers: Vec<PeerInfo> = state.all_peers().await;
    let self_id = state.network.node_id.clone();

    let futures = peers.into_iter().map(|peer| {
        let http = state.http.clone();
        let is_self = peer.node_id == self_id;
        let group = group.to_string();
        async move {
            let base = peer.gateway_url()?;

            let (models, node) = futures::future::join(
                http.get(format!("{base}/api/ui/models"))
                    .timeout(PEER_TIMEOUT * 4)
                    .send(),
                http.get(format!("{base}/api/ui/node"))
                    .timeout(PEER_TIMEOUT)
                    .send(),
            )
            .await;

            let body: serde_json::Value = models.ok()?.json().await.ok()?;
            let node: serde_json::Value = match node {
                Ok(r) => r.json().await.unwrap_or(json!({})),
                Err(_) => json!({}),
            };

            let mut held: Vec<HeldShard> = body
                .get("shard_sets")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter(|set| set.get("group").and_then(|g| g.as_str()) == Some(group.as_str()))
                .flat_map(|set| set.get("shards").and_then(|v| v.as_array()).into_iter().flatten())
                .filter_map(parse_shard)
                .collect();
            // Directory order is whatever the filesystem felt like; the screen
            // lists these as "L0–4 · L5–31" and reads as noise unsorted.
            held.sort_by_key(|s| (s.start_layer, s.end_layer));

            Some(Machine {
                node_id: peer.node_id.clone(),
                hostname: if peer.hostname.is_empty() && is_self {
                    body.get("hostname")
                        .and_then(|h| h.as_str())
                        .unwrap_or_default()
                        .to_string()
                } else {
                    peer.hostname.clone()
                },
                mem_free_mb: free_memory_mb(node.get("system")),
                // The peer's own reading beats the one it gossiped minutes ago,
                // but the gossip is still there when it did not answer.
                disk_free_mb: node
                    .pointer("/system/disk/free_mb")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as u64)
                    .unwrap_or(peer.disk_free_mb),
                held,
            })
        }
    });

    futures::future::join_all(futures)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Where the weights of a slice would actually have to fit.
///
/// A machine with a discrete GPU loads them into VRAM (`--n-gpu-layers 99`), so
/// its RAM figure says nothing useful. Unified memory has no separate pool to
/// report, so RAM *is* the answer there — see the note `sysmon` puts on screen.
fn free_memory_mb(system: Option<&serde_json::Value>) -> u64 {
    let Some(system) = system else { return 0 };
    if system.get("gpu_kind").and_then(|k| k.as_str()) == Some("nvidia") {
        let free: f64 = system
            .get("gpus")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|g| {
                let total = g.get("vram_total_mb")?.as_f64()?;
                let used = g.get("vram_used_mb").and_then(|v| v.as_f64()).unwrap_or(0.0);
                Some((total - used).max(0.0))
            })
            .sum();
        if free > 0.0 {
            return free as u64;
        }
    }
    system
        .pointer("/ram/available_mb")
        .and_then(|v| v.as_f64())
        .map(|v| v as u64)
        .unwrap_or(0)
}

fn parse_shard(v: &serde_json::Value) -> Option<HeldShard> {
    let range = v.get("shard_range")?.as_array()?;
    Some(HeldShard {
        start_layer: range.first()?.as_u64()? as u32,
        end_layer: range.get(1)?.as_u64()? as u32,
        total_layers: v.get("total_layers").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
        name: v.get("name")?.as_str()?.to_string(),
        path: v.get("path")?.as_str()?.to_string(),
        size_bytes: v.get("size_bytes").and_then(|s| s.as_u64()).unwrap_or(0),
    })
}

// ── The decision ─────────────────────────────────────────────────────────────

/// A range that exists somewhere in the cluster, selected or not.
///
/// The same model is often sliced more than once — 0-4 / 5-31 on one machine and
/// 0-10 / 11-31 on another — and every one of those cuts is a legitimate thing
/// to deploy. The screen needs all of them to offer the choice, so the plan
/// carries the whole menu and not only the part it picked.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    name: String,
    size_bytes: u64,
    /// Every machine that already has this exact range.
    holders: Vec<String>,
    /// Part of the plan being shown.
    selected: bool,
}

/// A machine that could take a range, as the screen has to describe it.
#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    node_id: String,
    label: String,
    mem_free_mb: u64,
    disk_free_mb: u64,
    /// Ranges of this set it already holds.
    holds: Vec<[u32; 2]>,
    /// Range it takes in the plan being shown, if any.
    taking: Option<[u32; 2]>,
}

/// One machine offered as the target of one slice, with the reason it works.
#[derive(Debug, Clone, Serialize)]
pub struct Alternative {
    node_id: String,
    label: String,
    /// Already has the file: nothing to copy.
    has: bool,
    /// Nothing about this machine stops the assignment.
    ok: bool,
    /// What it costs, or why it does not work.
    note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    /// 1-based place in the pipeline. Position 1 receives the prompt.
    position: u32,
    start_layer: u32,
    end_layer: u32,
    total_layers: u32,
    name: String,
    size_bytes: u64,
    target_node_id: String,
    target_host: String,
    /// Already on the target: nothing to copy, this one starts immediately.
    ready: bool,
    /// Who sends it when it is not. `None` together with `ready: false` means
    /// nobody in the cluster has this slice at all.
    source_node_id: Option<String>,
    source_host: Option<String>,
    source_path: Option<String>,
    /// Where it will be on the target once it lands, when known up front.
    target_path: Option<String>,
    /// Entry point of the pipeline — whoever holds layer 0.
    primary: bool,
    /// Per-machine GPU offload, when the operator set one. `None` means this
    /// peer takes `params.n_gpu_layers`.
    n_gpu_layers: Option<u32>,
    /// Every machine the operator could move this range to.
    options: Vec<Alternative>,
    /// Set when this assignment cannot be carried out, and why.
    problem: Option<String>,
    /// Set when it can, but the operator should read it first.
    note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    group: String,
    total_layers: u32,
    steps: Vec<Step>,
    /// Every layer from 0 to `total_layers` is covered exactly once.
    complete: bool,
    /// Bytes that have to cross the network before this can serve.
    transfer_bytes: u64,
    /// Set when the plan cannot be carried out at all, and why.
    blocked: Option<String>,
    /// True when the node chose this, false when it is validating the
    /// operator's own selection.
    auto: bool,
    /// Every range anyone holds, so the screen can offer the ones left out.
    available: Vec<Candidate>,
    /// Every machine, so the screen can offer them as targets.
    machines: Vec<MachineView>,
    /// Layer ranges the selection does not cover.
    missing: Vec<[u32; 2]>,
    /// Layer ranges the selection covers more than once. Never empty together
    /// with `blocked: None`.
    overlap: Vec<[u32; 2]>,
    /// Things worth reading that do not stop the deploy.
    warnings: Vec<String>,
    /// The execution settings this deploy would launch with, defaults filled
    /// in. Echoed rather than left to the screen so the form starts from the
    /// node's own numbers instead of a second copy of them in TypeScript.
    params: RuntimeParams,
    /// Prompt tokens one request will be allowed, under `params`. This is the
    /// number that decides whether a harness fits — see `gateway::prompt_budget`.
    prompt_tokens: u32,
    /// Prompt tokens per round trip through the pipeline, under `params`.
    chunk_tokens: u32,
}

/// Every distinct range anyone holds, in layer order.
fn candidates(machines: &[Machine]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for m in machines {
        for s in &m.held {
            match out
                .iter_mut()
                .find(|c| c.start_layer == s.start_layer && c.end_layer == s.end_layer)
            {
                Some(c) => c.holders.push(m.node_id.clone()),
                None => out.push(Candidate {
                    start_layer: s.start_layer,
                    end_layer: s.end_layer,
                    total_layers: s.total_layers,
                    name: s.name.clone(),
                    size_bytes: s.size_bytes,
                    holders: vec![m.node_id.clone()],
                    selected: false,
                }),
            }
        }
    }
    out.sort_by_key(|c| (c.start_layer, c.end_layer));
    out
}

/// The chain the node would pick on its own: start at layer 0 and always take
/// the longest range that begins where the last one ended.
///
/// Longest-first is not an aesthetic choice. A node serves one range, so the
/// chain with the fewest slices is the one most likely to fit the cluster, and
/// taking ranges only where the previous one ended makes overlap impossible by
/// construction — which matters the moment a model has been sliced twice with
/// different cuts. When nothing continues the chain the sweep jumps to the next
/// range that starts later, which leaves a hole and says so.
fn auto_chain(candidates: &[Candidate]) -> Vec<usize> {
    let total = candidates.iter().map(|c| c.total_layers).max().unwrap_or(0);
    let mut chain = Vec::new();
    let mut next = 0u32;
    loop {
        let pick = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.start_layer == next)
            .max_by_key(|(_, c)| (c.end_layer, c.holders.len()))
            .map(|(i, _)| i)
            .or_else(|| {
                candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.start_layer > next)
                    .min_by_key(|(_, c)| (c.start_layer, std::cmp::Reverse(c.end_layer)))
                    .map(|(i, _)| i)
            });
        let Some(i) = pick else { break };
        chain.push(i);
        next = candidates[i].end_layer.saturating_add(1);
        if total > 0 && next >= total {
            break;
        }
    }
    chain
}

/// Hand each range in the chain to a machine.
///
/// A machine that already holds a slice is preferred for it: a 10 GB slice takes
/// ~17 minutes on the real link, so every avoided copy is the difference between
/// a deploy that finishes now and one that finishes after lunch. Within that,
/// roomiest first — and because the chain starts at layer 0, the roomiest
/// machine gets the range that runs the whole prompt of every request.
fn auto_assign(chain: &[usize], candidates: &[Candidate], machines: &[Machine]) -> Vec<String> {
    let mut taken: Vec<String> = Vec::new();
    let mut targets = vec![String::new(); chain.len()];

    for (slot, &ci) in chain.iter().enumerate() {
        let c = &candidates[ci];
        let mut holders: Vec<&Machine> = machines
            .iter()
            .filter(|m| !taken.contains(&m.node_id) && c.holders.contains(&m.node_id))
            .collect();
        holders.sort_by_key(|m| std::cmp::Reverse(m.mem_free_mb));
        if let Some(m) = holders.first() {
            taken.push(m.node_id.clone());
            targets[slot] = m.node_id.clone();
        }
    }

    for target in targets.iter_mut() {
        if !target.is_empty() {
            continue;
        }
        let mut free: Vec<&Machine> = machines
            .iter()
            .filter(|m| !taken.contains(&m.node_id))
            .collect();
        free.sort_by_key(|m| std::cmp::Reverse(m.mem_free_mb));
        if let Some(m) = free.first() {
            taken.push(m.node_id.clone());
            *target = m.node_id.clone();
        }
    }

    targets
}

/// Roughly what the weights need once loaded, in MB, with the same headroom the
/// slicing planner reserves.
fn needed_mb(size_bytes: u64) -> u64 {
    size_bytes / MIB
}

fn usable_mb(free_mb: u64) -> u64 {
    (free_mb as f64 * (1.0 - DEFAULT_RESERVE_PCT / 100.0)) as u64
}

/// Decide who serves what — or check what the operator decided.
fn plan(
    group: &str,
    machines: &[Machine],
    selection: Option<&[SelectedSlice]>,
    params: &RuntimeParams,
) -> Plan {
    let mut available = candidates(machines);
    let total_layers = available.iter().map(|c| c.total_layers).max().unwrap_or(0);

    // What we are planning: (index into `available`, target node id).
    let auto = selection.is_none();
    // (index into `available`, target node id, per-machine GPU offload)
    let mut chosen: Vec<(usize, String, Option<u32>)> = Vec::new();
    let mut blocked: Option<String> = None;
    let mut warnings: Vec<String> = Vec::new();

    match selection {
        None => {
            let chain = auto_chain(&available);
            let targets = auto_assign(&chain, &available, machines);
            chosen = chain
                .into_iter()
                .zip(targets)
                .map(|(i, t)| (i, t, None))
                .collect();
        }
        Some(picked) => {
            for s in picked {
                let Some(i) = available
                    .iter()
                    .position(|c| c.start_layer == s.start_layer && c.end_layer == s.end_layer)
                else {
                    blocked.get_or_insert(format!(
                        "a fatia L{}-{} não existe em máquina nenhuma do cluster",
                        s.start_layer, s.end_layer
                    ));
                    continue;
                };
                if chosen.iter().any(|(j, ..)| *j == i) {
                    blocked.get_or_insert(format!(
                        "a fatia L{}-{} foi escolhida duas vezes",
                        s.start_layer, s.end_layer
                    ));
                    continue;
                }
                chosen.push((i, s.target_node_id.clone(), s.n_gpu_layers));
            }
            chosen.sort_by_key(|(i, ..)| (available[*i].start_layer, available[*i].end_layer));
        }
    }

    for (i, ..) in &chosen {
        available[*i].selected = true;
    }

    if available.is_empty() {
        blocked.get_or_insert(format!("nenhuma máquina do cluster tem fatias de {group}"));
    } else if chosen.is_empty() {
        blocked.get_or_insert("nenhuma fatia selecionada".to_string());
    }

    // ── Coverage: gaps warn, overlap refuses ────────────────────────────────
    let mut missing: Vec<[u32; 2]> = Vec::new();
    let mut overlap: Vec<[u32; 2]> = Vec::new();
    let mut next = 0u32;
    for (i, ..) in &chosen {
        let c = &available[*i];
        if c.start_layer > next {
            missing.push([next, c.start_layer - 1]);
        } else if c.start_layer < next {
            overlap.push([c.start_layer, next.min(c.end_layer + 1) - 1]);
        }
        next = next.max(c.end_layer.saturating_add(1));
    }
    if total_layers > 0 && next < total_layers {
        missing.push([next, total_layers - 1]);
    }
    let complete = missing.is_empty() && overlap.is_empty() && !chosen.is_empty();

    if let Some(o) = overlap.first() {
        // Two peers running the same layers would run them twice and the answer
        // would be wrong without anything failing, so this is a stop.
        blocked.get_or_insert(format!(
            "as camadas L{}-{} estão em duas fatias escolhidas ao mesmo tempo",
            o[0], o[1]
        ));
    }
    for m in &missing {
        warnings.push(if m[0] == m[1] {
            format!("a camada L{} não está em nenhuma fatia escolhida", m[0])
        } else {
            format!("as camadas L{}-{} não estão em nenhuma fatia escolhida", m[0], m[1])
        });
    }

    // ── One range per machine ───────────────────────────────────────────────
    for (pos, (_, target, _)) in chosen.iter().enumerate() {
        if target.is_empty() {
            continue;
        }
        if let Some(other) = chosen[..pos].iter().find(|(_, t, _)| t == target) {
            let label = machines
                .iter()
                .find(|m| &m.node_id == target)
                .map(|m| m.label())
                .unwrap_or_else(|| target.clone());
            blocked.get_or_insert(format!(
                "{label} ficou com duas faixas (L{}-{} e L{}-{}) — um nó serve uma faixa por vez",
                available[other.0].start_layer,
                available[other.0].end_layer,
                available[chosen[pos].0].start_layer,
                available[chosen[pos].0].end_layer,
            ));
            break;
        }
    }

    // ── The steps themselves ────────────────────────────────────────────────
    let mut transfer_bytes = 0u64;
    let mut steps: Vec<Step> = Vec::new();

    for (pos, (i, target_id, gpu)) in chosen.iter().enumerate() {
        let c = &available[*i];
        let target = machines.iter().find(|m| &m.node_id == target_id);
        let ready = target.is_some_and(|t| t.holds(c.start_layer, c.end_layer).is_some());
        let source = if ready {
            None
        } else {
            machines
                .iter()
                .find(|m| Some(&m.node_id) != target.map(|t| &t.node_id) && m.holds(c.start_layer, c.end_layer).is_some())
        };
        if !ready && target.is_some() {
            transfer_bytes += c.size_bytes;
        }

        let mut problem = None;
        let mut note = None;

        if target_id.is_empty() {
            problem = Some(format!(
                "a fatia L{}-{} está sem máquina",
                c.start_layer, c.end_layer
            ));
        } else if target.is_none() {
            problem = Some(format!("a máquina {target_id} não está no cluster"));
        } else if !ready && source.is_none() {
            problem = Some(format!(
                "a fatia L{}-{} não existe em máquina nenhuma do cluster",
                c.start_layer, c.end_layer
            ));
        }

        if let (None, Some(t)) = (&problem, target) {
            let need = needed_mb(c.size_bytes);
            if !ready && t.disk_free_mb > 0 && need > t.disk_free_mb {
                problem = Some(format!(
                    "{} tem {} GB livres em disco e a fatia pesa {} GB",
                    t.label(),
                    gb(t.disk_free_mb),
                    gb(need)
                ));
            } else if t.mem_free_mb > 0 && need > usable_mb(t.mem_free_mb) {
                note = Some(format!(
                    "{} tem {} GB de memória livre para uma fatia de {} GB",
                    t.label(),
                    gb(t.mem_free_mb),
                    gb(need)
                ));
            } else if !ready {
                note = Some(format!("{} GB pela rede antes de subir", gb(need)));
            }
        }

        if let Some(p) = &problem {
            blocked.get_or_insert(p.clone());
        }
        if let Some(n) = &note {
            if n.contains("memória") {
                warnings.push(n.clone());
            }
        }

        steps.push(Step {
            position: pos as u32 + 1,
            start_layer: c.start_layer,
            end_layer: c.end_layer,
            total_layers: c.total_layers,
            name: c.name.clone(),
            size_bytes: c.size_bytes,
            target_node_id: target_id.clone(),
            target_host: target.map(|t| t.label()).unwrap_or_default(),
            ready,
            source_node_id: source.map(|m| m.node_id.clone()),
            source_host: source.map(|m| m.label()),
            source_path: source
                .and_then(|m| m.holds(c.start_layer, c.end_layer))
                .map(|s| s.path.clone()),
            target_path: target
                .and_then(|t| t.holds(c.start_layer, c.end_layer))
                .map(|s| s.path.clone()),
            primary: c.start_layer == 0,
            n_gpu_layers: *gpu,
            options: alternatives(c, machines),
            problem,
            note,
        });
    }

    // The auto planner runs out of machines before it runs out of slices, and
    // that reads better as a count than as "a fatia L16-31 está sem máquina".
    if auto && steps.iter().any(|s| s.target_node_id.is_empty()) {
        blocked = Some(format!(
            "são {} fatias para {} {} — um nó serve uma faixa por vez",
            steps.len(),
            machines.len(),
            if machines.len() == 1 { "máquina" } else { "máquinas" }
        ));
    }

    let machine_views = machines
        .iter()
        .map(|m| MachineView {
            node_id: m.node_id.clone(),
            label: m.label(),
            mem_free_mb: m.mem_free_mb,
            disk_free_mb: m.disk_free_mb,
            holds: m.held.iter().map(|s| [s.start_layer, s.end_layer]).collect(),
            taking: chosen
                .iter()
                .find(|(_, t, _)| t == &m.node_id)
                .map(|(i, ..)| [available[*i].start_layer, available[*i].end_layer]),
        })
        .collect();

    Plan {
        group: group.to_string(),
        total_layers,
        complete,
        transfer_bytes,
        steps,
        blocked,
        auto,
        available,
        machines: machine_views,
        missing,
        overlap,
        warnings,
        prompt_tokens: crate::gateway::prompt_budget_of(
            params.n_ctx,
            params.max_sessions,
            params.max_new_tokens,
        ),
        chunk_tokens: crate::gateway::prompt_chunk_of(params.prefill_chunk_tokens, params.n_batch),
        params: params.clone(),
    }
}

fn gb(mb: u64) -> String {
    format!("{:.1}", mb as f64 / 1024.0).replace('.', ",")
}

/// Every machine as a possible target for one range, with the reason.
///
/// Occupancy is deliberately *not* part of `ok`: a machine already holding
/// another range is exactly the one an operator wants to swap with, and hiding
/// it would make the swap impossible. Picking a busy machine is caught by the
/// one-range-per-node rule instead, which is the honest place for it.
fn alternatives(c: &Candidate, machines: &[Machine]) -> Vec<Alternative> {
    let need = needed_mb(c.size_bytes);
    machines
        .iter()
        .map(|m| {
            let has = m.holds(c.start_layer, c.end_layer).is_some();
            let no_disk = !has && m.disk_free_mb > 0 && need > m.disk_free_mb;
            let tight = m.mem_free_mb > 0 && need > usable_mb(m.mem_free_mb);
            Alternative {
                node_id: m.node_id.clone(),
                label: m.label(),
                has,
                ok: !no_disk,
                note: if no_disk {
                    Some(format!("só {} GB livres em disco", gb(m.disk_free_mb)))
                } else if tight {
                    Some(format!("{} GB de memória livre", gb(m.mem_free_mb)))
                } else if has {
                    Some("já tem a fatia".to_string())
                } else {
                    Some(format!("receberia {} GB", gb(need)))
                },
            }
        })
        .collect()
}

async fn plan_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeployRequest>,
) -> Response {
    let machines = survey(&state, &req.group).await;
    Json(plan(&req.group, &machines, req.selection.as_deref(), &req.params)).into_response()
}

// ── Carrying it out ──────────────────────────────────────────────────────────

async fn deploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<DeployRequest>,
) -> Response {
    // The cluster is re-surveyed rather than trusted from the preview: the
    // screen's copy may be a minute old, and a machine that has left in the
    // meantime would otherwise be handed a slice. What is *not* recomputed is
    // the operator's choice — it is validated against the fresh survey and
    // refused if it no longer holds.
    let machines = survey(&state, &req.group).await;
    let plan = plan(&req.group, &machines, req.selection.as_deref(), &req.params);

    if let Some(reason) = plan.blocked {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": reason })),
        )
            .into_response();
    }
    if plan.steps.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "nada para implantar" })),
        )
            .into_response();
    }

    let job_id = uuid::Uuid::new_v4().to_string();
    let title = format!("Implantar {} em {} nós", req.group, plan.steps.len());
    jobs()
        .write()
        .await
        .insert(job_id.clone(), new_job(&job_id, "deploy", title, "preparando"));

    let id = job_id.clone();
    tokio::spawn(async move {
        match run(state, plan, req.params, &id).await {
            Ok(()) => set_job(&id, "done", "pronto", "o modelo está atendendo na rede").await,
            Err(e) => {
                warn!("implantação falhou: {e}");
                set_job(&id, "failed", "falhou", &e).await;
            }
        }
    });

    Json(json!({ "started": true, "job_id": job_id })).into_response()
}

/// Copy what has to be copied, then bind every peer to its range.
async fn run(
    state: GatewayState,
    plan: Plan,
    params: RuntimeParams,
    job_id: &str,
) -> Result<(), String> {
    let key = state.auth.read().await.connection_key.clone();
    let to_move: Vec<&Step> = plan.steps.iter().filter(|s| !s.ready).collect();

    // ── Phase 1: transfers ───────────────────────────────────────────────────
    //
    // Each target pulls for itself, through the transfer path that already has
    // resume, MD5 checking and a disk-space guard. Nothing new moves bytes here.
    if !to_move.is_empty() {
        set_job(
            job_id,
            "running",
            "copiando fatias",
            &format!(
                "{} fatia(s), {:.1} GB no total",
                to_move.len(),
                plan.transfer_bytes as f64 / 1_073_741_824.0
            ),
        )
        .await;

        let mut watching: Vec<(String, String)> = Vec::new(); // (gateway, job id)
        for step in &to_move {
            let (Some(source_node), Some(source_path)) =
                (&step.source_node_id, &step.source_path)
            else {
                return Err(format!(
                    "a fatia L{}-{} não tem de onde vir",
                    step.start_layer, step.end_layer
                ));
            };
            let gateway = gateway_of(&state, &step.target_node_id)
                .await
                .ok_or_else(|| format!("{} não anuncia porta de gateway", step.target_host))?;

            let body: serde_json::Value = state
                .http
                .post(format!("{gateway}/api/transfer/pull"))
                .bearer_auth(&key)
                .json(&json!({
                    "node_id": source_node,
                    "remote_path": source_path,
                    "name": step.name,
                }))
                .send()
                .await
                .map_err(|e| format!("{} não aceitou puxar a fatia: {e}", step.target_host))?
                .json()
                .await
                .unwrap_or(json!({}));

            match body.get("job_id").and_then(|v| v.as_str()) {
                Some(tid) => {
                    info!(
                        "{} puxando L{}-{} de {}",
                        step.target_host, step.start_layer, step.end_layer, source_node
                    );
                    watching.push((gateway, tid.to_string()));
                }
                None => {
                    return Err(format!(
                        "{} recusou a cópia: {}",
                        step.target_host,
                        body.get("error").and_then(|e| e.as_str()).unwrap_or("motivo não informado")
                    ))
                }
            }
        }

        wait_for_transfers(&state, &key, &watching, job_id).await?;
    }

    // ── Phase 2: bind every peer to its range ────────────────────────────────
    set_job(job_id, "running", "subindo os nós", "atribuindo as faixas").await;
    set_job_progress(job_id, 92.0).await;

    // Where each file ended up is the target's own business, so ask it rather
    // than assume a path on someone else's filesystem.
    let machines = survey(&state, &plan.group).await;
    let mut targets = Vec::new();
    for step in &plan.steps {
        let path = machines
            .iter()
            .find(|m| m.node_id == step.target_node_id)
            .and_then(|m| m.holds(step.start_layer, step.end_layer))
            .map(|s| s.path.clone())
            .ok_or_else(|| {
                format!(
                    "{} não achou a fatia L{}-{} depois da cópia",
                    step.target_host, step.start_layer, step.end_layer
                )
            })?;

        targets.push(LaunchTarget {
            node_id: step.target_node_id.clone(),
            source_model_path: path,
            start_layer: step.start_layer,
            end_layer: step.end_layer,
            // Already a slice of exactly this range: nothing to cut.
            skip_split: true,
            n_gpu_layers: step.n_gpu_layers,
        });
    }

    // The pipeline is the layer order, so the entry point is whoever holds
    // layer 0 — the same machine the screen marks "processa o prompt".
    let primary = plan
        .steps
        .iter()
        .find(|s| s.primary)
        .map(|s| s.target_node_id.clone());

    let response = launch(
        ConnectionAuth,
        State(state),
        Json(LaunchRequest {
            base_name: plan.group.clone(),
            total_layers: plan.total_layers,
            targets,
            primary_node_id: primary,
            params,
        }),
    )
    .await;

    if !response.status().is_success() {
        return Err(format!("o launch recusou: HTTP {}", response.status()));
    }
    Ok(())
}

async fn gateway_of(state: &GatewayState, node_id: &str) -> Option<String> {
    state
        .all_peers()
        .await
        .iter()
        .find(|p| p.node_id == node_id)
        .and_then(|p| p.gateway_url())
}

/// Poll the machines doing the copying until they are all finished.
///
/// The bar tracks bytes rather than file count, because the slices are wildly
/// unequal — a set of 2 GB and 9,6 GB would otherwise sit at 50% for most of the
/// wait and then finish in a blink.
async fn wait_for_transfers(
    state: &GatewayState,
    key: &str,
    watching: &[(String, String)],
    job_id: &str,
) -> Result<(), String> {
    loop {
        let mut done = 0usize;
        let mut fraction_sum = 0.0;

        for (gateway, id) in watching {
            let body: serde_json::Value = state
                .http
                .get(format!("{gateway}/api/transfer/jobs"))
                .bearer_auth(key)
                .timeout(PEER_TIMEOUT)
                .send()
                .await
                .map_err(|e| format!("perdi contato com {gateway}: {e}"))?
                .json()
                .await
                .unwrap_or(json!({}));

            let job = body
                .get("jobs")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .find(|j| j.get("id").and_then(|v| v.as_str()) == Some(id.as_str()));

            let Some(job) = job else {
                // A job that vanished from the list finished and was pruned.
                done += 1;
                fraction_sum += 1.0;
                continue;
            };

            match job.get("state").and_then(|v| v.as_str()) {
                Some("done") => {
                    done += 1;
                    fraction_sum += 1.0;
                }
                Some("failed") => {
                    return Err(format!(
                        "a cópia falhou: {}",
                        job.get("error").and_then(|e| e.as_str()).unwrap_or("sem detalhe")
                    ))
                }
                _ => {
                    let total = job.get("total_bytes").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let got = job.get("received_bytes").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    if total > 0.0 {
                        fraction_sum += (got / total).clamp(0.0, 1.0);
                    }
                }
            }
        }

        // Phase 1 owns the first 90% of the bar; the launch is the rest.
        let pct = fraction_sum / watching.len().max(1) as f64 * 90.0;
        set_job_progress(job_id, pct).await;

        if done == watching.len() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1_073_741_824;

    struct Spec {
        id: &'static str,
        mem_free_mb: u64,
        disk_free_mb: u64,
        held: Vec<(u32, u32)>,
    }

    fn m(id: &'static str, mem_free_mb: u64, held: &[(u32, u32)]) -> Spec {
        Spec {
            id,
            mem_free_mb,
            disk_free_mb: 500_000,
            held: held.to_vec(),
        }
    }

    fn build(specs: Vec<Spec>) -> Vec<Machine> {
        specs
            .into_iter()
            .map(|s| Machine {
                node_id: s.id.to_string(),
                hostname: s.id.to_string(),
                mem_free_mb: s.mem_free_mb,
                disk_free_mb: s.disk_free_mb,
                held: s
                    .held
                    .iter()
                    .map(|(a, b)| HeldShard {
                        start_layer: *a,
                        end_layer: *b,
                        total_layers: 32,
                        name: format!("m_{a}_{b}.gguf"),
                        path: format!("/models/m_{a}_{b}.gguf"),
                        size_bytes: GIB,
                    })
                    .collect(),
            })
            .collect()
    }

    fn pick(start: u32, end: u32, target: &str) -> SelectedSlice {
        SelectedSlice {
            start_layer: start,
            end_layer: end,
            target_node_id: target.to_string(),
            n_gpu_layers: None,
        }
    }

    /// The knobs are their own axis; every test below is about the placement,
    /// so they all run on the defaults unless they say otherwise.
    fn plan(group: &str, machines: &[Machine], selection: Option<&[SelectedSlice]>) -> Plan {
        super::plan(group, machines, selection, &RuntimeParams::default())
    }

    // ── The suggestion ──────────────────────────────────────────────────────

    /// The happy shape: each machine already has one of the slices, so the
    /// deploy is pure launch and copies nothing.
    #[test]
    fn slices_already_in_place_need_no_transfer() {
        let ms = build(vec![m("a", 100_000, &[(0, 15)]), m("b", 200_000, &[(16, 31)])]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete, "0-15 e 16-31 cobrem 32 camadas");
        assert_eq!(p.transfer_bytes, 0);
        assert!(p.steps.iter().all(|s| s.ready));
        assert!(p.auto);
    }

    /// One machine holds both slices: the other has to be given one, and the
    /// plan has to name who sends it.
    #[test]
    fn a_missing_slice_is_planned_as_a_copy() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 200_000, &[]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);

        let copied: Vec<&Step> = p.steps.iter().filter(|s| !s.ready).collect();
        assert_eq!(copied.len(), 1, "só uma fatia precisa viajar");
        assert_eq!(copied[0].source_node_id.as_deref(), Some("a"));
        assert_eq!(copied[0].target_node_id, "b");
        assert_eq!(p.transfer_bytes, GIB);
    }

    /// Layer 0 runs every prompt, so it goes to the machine with the most room
    /// — the one place the assignment is not arbitrary.
    #[test]
    fn layer_zero_goes_to_the_roomiest_machine() {
        let ms = build(vec![
            m("pequena", 10_000, &[(0, 15), (16, 31)]),
            m("grande", 900_000, &[(0, 15), (16, 31)]),
        ]);
        let p = plan("m", &ms, None);
        let first = p.steps.iter().find(|s| s.start_layer == 0).unwrap();
        assert_eq!(first.target_node_id, "grande");
        assert!(first.primary, "quem tem a camada 0 recebe o prompt");
        assert_eq!(first.position, 1);
    }

    /// A node serves one range, so three slices onto two machines is a stop,
    /// not a deploy that half works.
    #[test]
    fn more_slices_than_machines_is_refused() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 9), (10, 19), (20, 31)]),
            m("b", 100_000, &[]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_some());
        assert!(p.blocked.unwrap().contains("3 fatias"));
    }

    /// A gap in the set is reported as incomplete rather than deployed into a
    /// pipeline that can never plan.
    #[test]
    fn a_gap_in_the_set_is_reported() {
        let ms = build(vec![m("a", 100_000, &[(0, 15)]), m("b", 100_000, &[(20, 31)])]);
        let p = plan("m", &ms, None);
        assert!(!p.complete, "faltam as camadas 16-19");
        assert_eq!(p.missing, vec![[16, 19]]);
        assert!(p.warnings.iter().any(|w| w.contains("L16-19")));
    }

    /// Two cuts of the same model live side by side. The suggestion has to pick
    /// one coherent chain instead of stacking 0-4, 0-10, 5-31 and 11-31 on top
    /// of each other.
    #[test]
    fn overlapping_slicings_are_not_stacked_by_the_planner() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 4), (5, 31)]),
            m("b", 100_000, &[(0, 10), (11, 31)]),
        ]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.overlap.is_empty(), "{:?}", p.overlap);
        assert!(p.complete);
        assert_eq!(p.steps.len(), 2, "duas fatias, não quatro");
        // Longest first from layer 0: 0-10 beats 0-4.
        assert_eq!((p.steps[0].start_layer, p.steps[0].end_layer), (0, 10));
        assert_eq!((p.steps[1].start_layer, p.steps[1].end_layer), (11, 31));
        assert_eq!(p.available.len(), 4, "as quatro continuam à disposição");
        assert_eq!(p.available.iter().filter(|c| c.selected).count(), 2);
    }

    #[test]
    fn a_set_nobody_holds_is_refused() {
        let ms = build(vec![m("a", 100_000, &[])]);
        let p = plan("m", &ms, None);
        assert!(p.blocked.unwrap().contains("nenhuma máquina"));
    }

    // ── The operator's own choice ───────────────────────────────────────────

    /// Reassigning by hand is honoured, not silently re-planned: the roomy
    /// machine would have taken layer 0, and the operator says otherwise.
    #[test]
    fn an_explicit_assignment_is_honoured() {
        let ms = build(vec![
            m("pequena", 10_000, &[(0, 15), (16, 31)]),
            m("grande", 900_000, &[(0, 15), (16, 31)]),
        ]);
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 15, "pequena"), pick(16, 31, "grande")]),
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(!p.auto);
        assert_eq!(p.steps[0].target_node_id, "pequena");
        assert_eq!(p.steps[1].target_node_id, "grande");
    }

    /// Mixing two slicings is exactly the thing that has to be refused: layers
    /// 5-10 would run twice and the answer would be quietly wrong.
    #[test]
    fn overlapping_selection_is_refused() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 10), (5, 31)]),
            m("b", 100_000, &[(0, 10), (5, 31)]),
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 10, "a"), pick(5, 31, "b")]));
        assert_eq!(p.overlap, vec![[5, 10]]);
        let blocked = p.blocked.expect("sobreposição tem que barrar");
        assert!(blocked.contains("L5-10"), "{blocked}");
        assert!(!p.complete);
    }

    /// A hole the operator left on purpose warns loudly but still deploys —
    /// half a model on the network is a legitimate step, it just will not serve.
    #[test]
    fn a_selection_with_a_hole_warns_without_blocking() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (20, 31)]),
            m("b", 100_000, &[(0, 15), (20, 31)]),
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(20, 31, "b")]));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(!p.complete);
        assert_eq!(p.missing, vec![[16, 19]]);
    }

    /// Choosing part of the model on purpose: one slice, and the rest reported
    /// as missing rather than invented.
    #[test]
    fn a_partial_selection_reports_what_is_left_out() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a")]));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert_eq!(p.missing, vec![[16, 31]]);
        assert_eq!(p.steps.len(), 1);
    }

    #[test]
    fn two_ranges_on_one_machine_is_refused() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 100_000, &[]),
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "a")]));
        let blocked = p.blocked.expect("um nó serve uma faixa");
        assert!(blocked.contains("duas faixas"), "{blocked}");
    }

    #[test]
    fn a_slice_with_no_machine_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "")]));
        assert!(p.blocked.unwrap().contains("sem máquina"));
    }

    #[test]
    fn a_machine_that_left_the_cluster_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "fantasma")]));
        assert!(p.blocked.unwrap().contains("não está no cluster"));
    }

    /// A range nobody in the cluster has cannot be deployed however the
    /// machines are shuffled.
    #[test]
    fn a_selected_range_nobody_holds_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 15)])]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "a")]));
        assert!(p.blocked.unwrap().contains("não existe em máquina nenhuma"));
    }

    /// Disk is a hard fact — `transfer.rs` would refuse the copy anyway, so it
    /// is better refused before anything moves.
    #[test]
    fn a_target_without_disk_room_for_the_copy_is_refused() {
        let mut ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 100_000, &[]),
        ]);
        ms[1].disk_free_mb = 100; // 100 MB for a 1 GiB slice
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "b")]));
        let blocked = p.blocked.expect("não cabe no disco");
        assert!(blocked.contains("disco"), "{blocked}");
    }

    /// Memory is an estimate, so it warns and marks the row instead of
    /// refusing: the reading is honest on Linux and unified on macOS, and a
    /// wrong stop would be worse than a loud line.
    #[test]
    fn a_target_short_on_memory_warns_and_still_deploys() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15), (16, 31)]),
            m("b", 200, &[(16, 31)]), // 200 MB livres para 1 GiB
        ]);
        let p = plan("m", &ms, Some(&[pick(0, 15, "a"), pick(16, 31, "b")]));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        let tight = p.steps.iter().find(|s| s.target_node_id == "b").unwrap();
        assert!(tight.note.as_deref().unwrap().contains("memória"));
        assert!(p.warnings.iter().any(|w| w.contains("memória")));
    }

    /// Every machine is offered for every range, including the one already busy
    /// elsewhere — that is the machine a swap needs.
    #[test]
    fn every_machine_is_offered_as_a_target() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(16, 31)]),
            m("c", 100_000, &[]),
        ]);
        let p = plan("m", &ms, None);
        let first = &p.steps[0];
        assert_eq!(first.options.len(), 3);
        let a = first.options.iter().find(|o| o.node_id == "a").unwrap();
        assert!(a.has && a.ok);
        let c = first.options.iter().find(|o| o.node_id == "c").unwrap();
        assert!(!c.has && c.ok, "c não tem a fatia mas pode receber");
    }

    /// The machine list carries what the screen needs to explain a refusal.
    #[test]
    fn the_plan_carries_the_machines_and_what_they_take() {
        let ms = build(vec![m("a", 100_000, &[(0, 15)]), m("b", 100_000, &[(16, 31)])]);
        let p = plan("m", &ms, None);
        assert_eq!(p.machines.len(), 2);
        let a = p.machines.iter().find(|v| v.node_id == "a").unwrap();
        assert_eq!(a.holds, vec![[0, 15]]);
        assert_eq!(a.taking, Some([0, 15]));
    }

    // ── Execution settings ──────────────────────────────────────────────────

    /// The screen fills its form from the plan, so the plan has to carry the
    /// settings back — including the two derived numbers that decide whether a
    /// harness's prompt fits.
    #[test]
    fn the_plan_echoes_the_settings_and_the_prompt_budget() {
        let ms = build(vec![m("a", 100_000, &[(0, 15)]), m("b", 100_000, &[(16, 31)])]);
        let params = RuntimeParams {
            n_ctx: 32768,
            n_batch: 3072,
            max_sessions: 2,
            max_new_tokens: 256,
            prefill_chunk_tokens: 4096,
            ..RuntimeParams::default()
        };
        let p = super::plan("m", &ms, None, &params);

        assert_eq!(p.params.n_ctx, 32768);
        // 32768 / 2 sessões − 256 gerados.
        assert_eq!(p.prompt_tokens, 16128);
        // O pedaço nunca passa de n_batch, por mais alto que seja o pedido.
        assert_eq!(p.chunk_tokens, 3072);
    }

    /// A cluster of unlike machines is the whole point of the per-peer offload:
    /// the number the operator typed for one peer must reach that peer and no
    /// other.
    #[test]
    fn per_machine_gpu_offload_rides_along_with_the_slice() {
        let ms = build(vec![
            m("com-placa", 100_000, &[(0, 15)]),
            m("so-cpu", 100_000, &[(16, 31)]),
        ]);
        let sel = [
            pick(0, 15, "com-placa"),
            SelectedSlice {
                n_gpu_layers: Some(0),
                ..pick(16, 31, "so-cpu")
            },
        ];
        let p = plan("m", &ms, Some(&sel));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert_eq!(p.steps[0].n_gpu_layers, None, "herda o valor do cluster");
        assert_eq!(p.steps[1].n_gpu_layers, Some(0));
    }

    // ── Reading a machine's free memory ─────────────────────────────────────

    #[test]
    fn nvidia_memory_comes_from_vram_and_everything_else_from_ram() {
        let nvidia = json!({
            "gpu_kind": "nvidia",
            "ram": { "available_mb": 3000.0 },
            "gpus": [{ "vram_total_mb": 16000.0, "vram_used_mb": 7600.0 }],
        });
        assert_eq!(free_memory_mb(Some(&nvidia)), 8400);

        let unified = json!({
            "gpu_kind": "unified",
            "ram": { "available_mb": 12000.0 },
            "gpus": [],
        });
        assert_eq!(free_memory_mb(Some(&unified)), 12000);

        // A peer that did not answer says nothing, and nothing is inferred.
        assert_eq!(free_memory_mb(None), 0);
    }
}
