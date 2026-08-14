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
//!   refusal, not a warning — unless `merge` is on, in which case adjacent
//!   ranges on the same machine become one *merged unit*: the machine joins
//!   the slices into a single file (`splitter::merge`) and serves the union.
//!   A merged step carries `parts`, one per slice, each with its own source.
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
use crate::plan::{self, DEFAULT_RESERVE_PCT};
use crate::webui::{
    jobs, launch, new_job, set_job, set_job_progress, LaunchRequest, LaunchTarget, MergeSource,
    RuntimeParams, PEER_TIMEOUT,
};

const MIB: u64 = 1024 * 1024;

pub fn routes() -> Router<GatewayState> {
    Router::new()
        .route("/api/ui/deploy/plan", post(plan_endpoint))
        .route("/api/ui/deploy", post(deploy_endpoint))
        .route("/api/ui/deploy/node", post(add_node_endpoint))
        .route("/api/ui/deploy/node/remove", post(remove_node_endpoint))
        .route("/api/ui/undeploy", post(undeploy_endpoint))
}

/// Camadas que a faixa nova compartilha com as que ja estao no ar.
///
/// Nao e recusa: e o que a tela mostra ao operador para ele saber que esta
/// criando uma rota paralela em vez de preencher um buraco.
pub fn overlapping_layers(existentes: &[(u32, u32)], nova: (u32, u32)) -> Vec<u32> {
    (nova.0..=nova.1)
        .filter(|c| existentes.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
}

/// Posicao desta faixa na ordem das camadas, e quantas faixas ficam no total.
///
/// `peer_index`/`peer_count` nao sao a lista de alvos de uma implantacao: sao
/// a posicao no pipeline, que e a ordem das camadas. Quem soma um no de cada
/// vez precisa dizer isso explicitamente, senao o unico alvo da requisicao
/// vira "peer 1 de 1" e todo no somado se declara o ultimo da divisao.
///
/// Uma faixa identica a uma existente e substituicao, nao um peer a mais.
pub fn split_position(existentes: &[(u32, u32)], nova: (u32, u32)) -> (u32, u32) {
    let mut faixas: Vec<(u32, u32)> = existentes
        .iter()
        .copied()
        .filter(|f| *f != nova)
        .collect();
    faixas.push(nova);
    faixas.sort();
    let index = faixas.iter().position(|f| *f == nova).unwrap_or(0) as u32;
    (index, faixas.len() as u32)
}

/// Camadas que ficam sem ninguem depois de tirar esta maquina.
///
/// Vazio significa que outra maquina cobre a mesma faixa - a rede perde uma
/// rota paralela e segue servindo, e o aviso ao operador e ameno. Nao vazio
/// significa buraco, e ai a confirmacao e explicita.
pub fn uncovered_after_removal(restantes: &[(u32, u32)], removida: (u32, u32)) -> Vec<u32> {
    (removida.0..=removida.1)
        .filter(|c| !restantes.iter().any(|(i, f)| c >= i && c <= f))
        .collect()
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
    /// cluster-wide number. See `LaunchTarget::n_gpu_layers` for why offload is
    /// decided per machine.
    #[serde(default)]
    pub n_gpu_layers: Option<u32>,
    /// Whether this machine's KV cache lives on its GPU. `None` takes the
    /// cluster's answer.
    ///
    /// Per machine for the same reason offload is: the cache is sized by the
    /// context window, and a box with 16 GB of VRAM and a CPU-only box in one
    /// pipeline cannot both be right about where it fits. One cluster-wide
    /// switch meant the machine with less memory decided for the machine with
    /// more, or the other way round.
    #[serde(default)]
    pub offload_kqv: Option<bool>,
}

/// What one machine of a deploy disagrees with the cluster about.
///
/// Everything else in `RuntimeParams` has to agree across the pipeline — the
/// context window and batch set the prompt budget the gateway enforces, the
/// sampling only runs on the last peer — so only what is genuinely a property
/// of the hardware belongs here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PeerOverrides {
    pub n_gpu_layers: Option<u32>,
    pub offload_kqv: Option<bool>,
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
    /// Join each machine's adjacent slices into a single file, so a node
    /// serves the union of the ranges it already holds. `false` keeps the rule
    /// that a node serves one range.
    #[serde(default)]
    pub merge: bool,
}

// ── What a machine can tell us about itself ──────────────────────────────────

/// One slice of the chosen set, as it exists on one machine.
#[derive(Debug, Clone)]
struct HeldShard {
    start_layer: u32,
    end_layer: u32,
    /// How many layers the *original* model had, when the slice says so.
    ///
    /// `None` is the normal state of an old slice: only `split_model.py` writes
    /// `rdmap2p.original_block_count`, so anything cut before it — the captain's
    /// `qwen_0_15.gguf` / `qwen_16_31.gguf` — carries no total at all. It stays
    /// an `Option` all the way to `model_size` because a missing total and a
    /// total of zero are different facts, and collapsing them is what let a
    /// deploy start and die at the far end (`runtime::NodeConfig::validate`).
    total_layers: Option<u32>,
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
                .flat_map(|set| {
                    set.get("shards")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                })
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
                let used = g
                    .get("vram_used_mb")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
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
        total_layers: v
            .get("total_layers")
            .and_then(|t| t.as_u64())
            .map(|t| t as u32)
            .filter(|t| *t > 0),
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
    /// Only what the slice itself declares. `null` on screen means "this file
    /// does not say how big the model is", which is a fact about the file.
    total_layers: Option<u32>,
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

/// One slice feeding a merged step: the range, its file, and where it comes
/// from. A merged step serves the union of its parts, joined into one file on
/// the target.
#[derive(Debug, Clone, Serialize)]
pub struct StepPart {
    start_layer: u32,
    end_layer: u32,
    name: String,
    size_bytes: u64,
    /// Already on the target: nothing to copy for this part.
    ready: bool,
    /// Who sends it when it is not. `None` together with `ready: false` means
    /// nobody in the cluster has this slice at all.
    source_node_id: Option<String>,
    source_host: Option<String>,
    source_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    /// 1-based place in the pipeline. Position 1 receives the prompt.
    position: u32,
    start_layer: u32,
    end_layer: u32,
    /// The model size this step will be launched with — the plan's answer, not
    /// the file's, so every step of one deploy carries the same number.
    total_layers: Option<u32>,
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
    /// Per-machine KV cache placement. `None` takes `params.offload_kqv`.
    offload_kqv: Option<bool>,
    /// Every machine the operator could move this range to.
    options: Vec<Alternative>,
    /// Set when this assignment cannot be carried out, and why.
    problem: Option<String>,
    /// Set when it can, but the operator should read it first.
    note: Option<String>,
    /// True when this step serves the union of `parts`, joined into one file
    /// on the target, instead of a single slice already in place.
    merged: bool,
    /// The slices this step is assembled from, in layer order. Empty for a
    /// plain step.
    parts: Vec<StepPart>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    group: String,
    /// How many layers the original model has. `None` means nobody could say —
    /// which is a refusal (`blocked`), never a zero passed down the line.
    total_layers: Option<u32>,
    /// True when that number was read off the coverage of the slices instead of
    /// out of their metadata. The deploy still runs; the screen has to say so,
    /// because a set of slices that stops at L31 looks exactly the same whether
    /// the model has 32 layers or 40 and the tail is missing from the cluster.
    total_layers_inferred: bool,
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
    /// True when the operator asked each machine's adjacent slices to be
    /// joined into one file — the plan reflects it in its steps.
    merge: bool,
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

/// How many layers the model being deployed has, and how we know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelSize {
    /// A slice said so, in `rdmap2p.original_block_count`.
    Declared(u32),
    /// No slice said so, but the ranges reach this far from layer 0, which is
    /// the same reading `catalog::build_shard_sets` makes for one machine.
    Inferred(u32),
    /// Nothing to read and nothing to deduce: no slice declares a total and
    /// none of them starts at layer 0.
    Unknown,
}

impl ModelSize {
    fn value(self) -> Option<u32> {
        match self {
            ModelSize::Declared(n) | ModelSize::Inferred(n) => Some(n),
            ModelSize::Unknown => None,
        }
    }
    fn inferred(self) -> bool {
        matches!(self, ModelSize::Inferred(_))
    }
}

/// Decide the model's layer count from every range the cluster holds.
///
/// Declared beats deduced, and the deduction is deliberately cluster-wide: one
/// machine holding only L16–31 can conclude nothing on its own, while the two
/// machines together plainly reach layer 32. Overlapping cuts of the same model
/// (0–4 / 5–31 next to 0–15 / 16–31) are why this walks the reach instead of
/// summing lengths.
fn model_size(candidates: &[Candidate]) -> ModelSize {
    if let Some(declared) = candidates.iter().filter_map(|c| c.total_layers).max() {
        return ModelSize::Declared(declared);
    }
    let mut reach = 0u32;
    loop {
        let next = candidates
            .iter()
            .filter(|c| c.start_layer <= reach && c.end_layer >= reach)
            .map(|c| c.end_layer + 1)
            .max();
        match next {
            Some(n) => reach = n,
            None => break,
        }
    }
    if reach > 0 {
        ModelSize::Inferred(reach)
    } else {
        ModelSize::Unknown
    }
}

/// The slices that carry no layer total, named the way the operator sees them
/// on screen, so a refusal points at files instead of at a concept.
fn nameless(candidates: &[Candidate]) -> String {
    const SHOWN: usize = 4;
    let missing: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.total_layers.is_none())
        .collect();
    let mut out = missing
        .iter()
        .take(SHOWN)
        .map(|c| format!("L{}-{} em {}", c.start_layer, c.end_layer, c.name))
        .collect::<Vec<_>>()
        .join(", ");
    if missing.len() > SHOWN {
        out.push_str(&format!(" e mais {}", missing.len() - SHOWN));
    }
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
fn auto_chain(candidates: &[Candidate], total: Option<u32>) -> Vec<usize> {
    let total = total.unwrap_or(0);
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
    merge: bool,
) -> Plan {
    let mut available = candidates(machines);
    let size = model_size(&available);
    let total_layers = size.value();

    // What we are planning: (index into `available`, target node id).
    let auto = selection.is_none();
    // (index into `available`, target node id, that machine's overrides)
    let mut chosen: Vec<(usize, String, PeerOverrides)> = Vec::new();
    let mut blocked: Option<String> = None;
    let mut warnings: Vec<String> = Vec::new();

    match selection {
        None => {
            let chain = auto_chain(&available, total_layers);
            let targets = auto_assign(&chain, &available, machines);
            chosen = chain
                .into_iter()
                .zip(targets)
                .map(|(i, t)| (i, t, PeerOverrides::default()))
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
                chosen.push((
                    i,
                    s.target_node_id.clone(),
                    PeerOverrides {
                        n_gpu_layers: s.n_gpu_layers,
                        offload_kqv: s.offload_kqv,
                    },
                ));
            }
            chosen.sort_by_key(|(i, ..)| (available[*i].start_layer, available[*i].end_layer));
        }
    }

    for (i, ..) in &chosen {
        available[*i].selected = true;
    }

    if available.is_empty() {
        blocked.get_or_insert(format!("nenhuma máquina do cluster tem fatias de {group}"));
    }

    // ── How big is the model ────────────────────────────────────────────────
    //
    // Refused here, in front of the operator, instead of being carried down as
    // a zero that only `runtime::NodeConfig::validate` objects to — by which
    // point `launch` has already stopped inference on every target machine.
    //
    // Ahead of "nenhuma fatia selecionada" on purpose: a set nobody can size is
    // unusable however the operator ticks the boxes, and finding that out one
    // click later is the same dead end one step further in.
    match size {
        ModelSize::Unknown if !available.is_empty() => {
            blocked.get_or_insert(format!(
                "nenhuma fatia de {group} diz de quantas camadas é o modelo ({}), \
                 e nenhuma delas começa na camada 0 para que o total possa ser deduzido. \
                 Refatie o modelo inteiro pela tela de Fatiar, que grava esse dado em cada \
                 fatia, ou traga para o cluster a fatia que começa na camada 0.",
                nameless(&available)
            ));
        }
        ModelSize::Inferred(n) => warnings.push(format!(
            "o total de camadas não está gravado nas fatias ({}), então foi deduzido da \
             cobertura delas: {n} camadas. Se o modelo original tiver mais, as camadas que \
             faltam não estão em máquina nenhuma e a resposta sai errada. Refatie pela tela \
             de Fatiar para gravar o total.",
            nameless(&available)
        )),
        _ => {}
    }

    if !available.is_empty() && chosen.is_empty() {
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
    if let Some(total) = total_layers.filter(|t| next < *t) {
        missing.push([next, total - 1]);
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
            format!(
                "as camadas L{}-{} não estão em nenhuma fatia escolhida",
                m[0], m[1]
            )
        });
    }

    // ── One range per machine, or one merged unit per machine ───────────────
    //
    // A node serves one file, so two ranges on the same machine can only both
    // come up when they are joined first. `merge` says that is what the
    // operator wants: each machine's adjacent slices become a single step that
    // merges them into one file on that machine. Without it, two ranges on one
    // machine stay the refusal they always were.
    let mut groups: Vec<Vec<(usize, String, PeerOverrides)>> = Vec::new();
    for entry in &chosen {
        match groups.iter_mut().find(|g| g[0].1 == entry.1) {
            Some(g) => g.push(entry.clone()),
            None => groups.push(vec![entry.clone()]),
        }
    }
    // `chosen` is already in layer order, so each group is too; ordering the
    // groups by their first range keeps the pipeline order intact.
    groups.sort_by_key(|g| (available[g[0].0].start_layer, available[g[0].0].end_layer));

    let mut transfer_bytes = 0u64;
    let mut steps: Vec<Step> = Vec::new();
    let mut position = 0u32;

    for grp in groups {
        let target_id = &grp[0].1;
        let target = machines.iter().find(|m| &m.node_id == target_id);
        let first = &available[grp[0].0];
        let last = &available[grp[grp.len() - 1].0];
        let start_layer = first.start_layer;
        let end_layer = last.end_layer;

        // Adjacency is what makes a merge possible: `splitter::merge` joins a
        // chain of neighbours and refuses anything else.
        let adjacent = grp
            .windows(2)
            .all(|w| available[w[1].0].start_layer == available[w[0].0].end_layer + 1);
        let merged = merge && grp.len() > 1 && adjacent;

        if grp.len() > 1 && !merge {
            blocked.get_or_insert(format!(
                "{} ficou com duas faixas (L{}-{} e L{}-{}) — junte as fatias vizinhas na \
                 tela, ou mova uma delas para outra máquina",
                target
                    .map(|m| m.label())
                    .unwrap_or_else(|| target_id.clone()),
                first.start_layer,
                first.end_layer,
                last.start_layer,
                last.end_layer,
            ));
        } else if grp.len() > 1 && merge && !adjacent {
            blocked.get_or_insert(format!(
                "as fatias de {} não são vizinhas (L{}-{} e L{}-{}) — só dá para juntar \
                 fatias adjacentes",
                target
                    .map(|m| m.label())
                    .unwrap_or_else(|| target_id.clone()),
                first.start_layer,
                first.end_layer,
                last.start_layer,
                last.end_layer,
            ));
        }

        if merged {
            let mut parts: Vec<StepPart> = Vec::new();
            for (i, _, _) in &grp {
                let c = &available[*i];
                let part_ready =
                    target.is_some_and(|t| t.holds(c.start_layer, c.end_layer).is_some());
                let source = if part_ready {
                    None
                } else {
                    machines.iter().find(|m| {
                        Some(&m.node_id) != target.map(|t| &t.node_id)
                            && m.holds(c.start_layer, c.end_layer).is_some()
                    })
                };
                if !part_ready && target.is_some() {
                    transfer_bytes += c.size_bytes;
                }
                parts.push(StepPart {
                    start_layer: c.start_layer,
                    end_layer: c.end_layer,
                    name: c.name.clone(),
                    size_bytes: c.size_bytes,
                    ready: part_ready,
                    source_node_id: source.map(|m| m.node_id.clone()),
                    source_host: source.map(|m| m.label()),
                    source_path: source
                        .and_then(|m| m.holds(c.start_layer, c.end_layer))
                        .map(|s| s.path.clone()),
                });
            }

            let size_bytes = parts.iter().map(|p| p.size_bytes).sum();
            let ready = parts.iter().all(|p| p.ready);

            let mut problem = None;
            let mut note = None;

            if target_id.is_empty() {
                problem = Some(format!(
                    "a faixa L{}-{} está sem máquina",
                    start_layer, end_layer
                ));
            } else if target.is_none() {
                problem = Some(format!("a máquina {target_id} não está no cluster"));
            } else if !ready {
                if let Some(orphan) = parts
                    .iter()
                    .find(|p| !p.ready && p.source_node_id.is_none())
                {
                    problem = Some(format!(
                        "a fatia L{}-{} não existe em máquina nenhuma do cluster",
                        orphan.start_layer, orphan.end_layer
                    ));
                }
            }

            if let (None, Some(t)) = (&problem, target) {
                // The join writes the union file on top of the parts, which
                // stay on disk — so free space must cover the union plus
                // whatever still has to be pulled. A fully-ready join is not
                // exempt: it still needs room to write the union.
                let missing: u64 = parts
                    .iter()
                    .filter(|p| !p.ready)
                    .map(|p| p.size_bytes)
                    .sum();
                let disk_need = needed_mb(size_bytes + missing);
                let mem_need = needed_mb(size_bytes);
                if t.disk_free_mb > 0 && disk_need > t.disk_free_mb {
                    problem = Some(format!(
                        "{} tem {} GB livres em disco e juntar essas fatias precisa de {} GB",
                        t.label(),
                        gb(t.disk_free_mb),
                        gb(disk_need)
                    ));
                } else if t.mem_free_mb > 0 && mem_need > usable_mb(t.mem_free_mb) {
                    note = Some(format!(
                        "{} tem {} GB de memória livre para o modelo junto de {} GB",
                        t.label(),
                        gb(t.mem_free_mb),
                        gb(mem_need)
                    ));
                } else if !ready {
                    note = Some(format!("{} GB pela rede antes de subir", gb(mem_need)));
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

            position += 1;
            steps.push(Step {
                position,
                start_layer,
                end_layer,
                total_layers,
                name: plan::shard_filename(group, start_layer, end_layer),
                size_bytes,
                target_node_id: target_id.clone(),
                target_host: target.map(|t| t.label()).unwrap_or_default(),
                ready,
                source_node_id: None,
                source_host: None,
                source_path: None,
                target_path: None,
                primary: start_layer == 0,
                n_gpu_layers: grp[0].2.n_gpu_layers,
                offload_kqv: grp[0].2.offload_kqv,
                options: merged_alternatives(&parts, machines),
                problem,
                note,
                merged: true,
                parts,
            });
        } else {
            // Plain steps: one range, one file. A group of more than one
            // already set `blocked`; the steps are still listed so the screen
            // can show which ranges are where.
            for (i, target_id, over) in &grp {
                let c = &available[*i];
                let target = machines.iter().find(|m| &m.node_id == target_id);
                let ready = target.is_some_and(|t| t.holds(c.start_layer, c.end_layer).is_some());
                let source = if ready {
                    None
                } else {
                    machines.iter().find(|m| {
                        Some(&m.node_id) != target.map(|t| &t.node_id)
                            && m.holds(c.start_layer, c.end_layer).is_some()
                    })
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

                position += 1;
                steps.push(Step {
                    position,
                    start_layer: c.start_layer,
                    end_layer: c.end_layer,
                    total_layers,
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
                    n_gpu_layers: over.n_gpu_layers,
                    offload_kqv: over.offload_kqv,
                    options: alternatives(c, machines),
                    problem,
                    note,
                    merged: false,
                    parts: Vec::new(),
                });
            }
        }
    }

    // The auto planner runs out of machines before it runs out of slices, and
    // that reads better as a count than as "a fatia L16-31 está sem máquina".
    if auto && steps.iter().any(|s| s.target_node_id.is_empty()) {
        blocked = Some(format!(
            "são {} fatias para {} {} — um nó serve uma faixa por vez",
            steps.len(),
            machines.len(),
            if machines.len() == 1 {
                "máquina"
            } else {
                "máquinas"
            }
        ));
    }

    let machine_views = machines
        .iter()
        .map(|m| MachineView {
            node_id: m.node_id.clone(),
            label: m.label(),
            mem_free_mb: m.mem_free_mb,
            disk_free_mb: m.disk_free_mb,
            holds: m
                .held
                .iter()
                .map(|s| [s.start_layer, s.end_layer])
                .collect(),
            // A merged machine serves the union of its ranges as one file, so
            // the table names that union rather than the first slice.
            taking: steps
                .iter()
                .find(|s| s.target_node_id == m.node_id)
                .map(|s| [s.start_layer, s.end_layer]),
        })
        .collect();

    Plan {
        group: group.to_string(),
        total_layers,
        total_layers_inferred: size.inferred(),
        complete,
        transfer_bytes,
        steps,
        blocked,
        auto,
        merge,
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

/// Every machine as a possible home for a merged step.
///
/// A machine "has" the merged unit when it already holds every part — the
/// merge then runs locally with nothing crossing the network. Otherwise it
/// would receive the parts, so the cost is the union's size.
fn merged_alternatives(parts: &[StepPart], machines: &[Machine]) -> Vec<Alternative> {
    let need = needed_mb(parts.iter().map(|p| p.size_bytes).sum());
    machines
        .iter()
        .map(|m| {
            let has = parts
                .iter()
                .all(|p| m.holds(p.start_layer, p.end_layer).is_some());
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
                    Some("já tem todas as fatias".to_string())
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
    Json(plan(
        &req.group,
        &machines,
        req.selection.as_deref(),
        &req.params,
        req.merge,
    ))
    .into_response()
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
    let plan = plan(
        &req.group,
        &machines,
        req.selection.as_deref(),
        &req.params,
        req.merge,
    );

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
    jobs().write().await.insert(
        job_id.clone(),
        new_job(&job_id, "deploy", title, "preparando"),
    );

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

    // One unit of movement: a slice that has to reach a target. A merged step
    // moves one file per part; a plain step moves its one slice.
    struct Pull {
        target_node_id: String,
        target_host: String,
        source_node_id: String,
        source_path: String,
        name: String,
        start_layer: u32,
        end_layer: u32,
    }
    let mut pulls: Vec<Pull> = Vec::new();
    for step in &plan.steps {
        if step.merged {
            for part in &step.parts {
                if part.ready {
                    continue;
                }
                let (Some(source_node), Some(source_path)) =
                    (&part.source_node_id, &part.source_path)
                else {
                    return Err(format!(
                        "a fatia L{}-{} não tem de onde vir",
                        part.start_layer, part.end_layer
                    ));
                };
                pulls.push(Pull {
                    target_node_id: step.target_node_id.clone(),
                    target_host: step.target_host.clone(),
                    source_node_id: source_node.clone(),
                    source_path: source_path.clone(),
                    name: part.name.clone(),
                    start_layer: part.start_layer,
                    end_layer: part.end_layer,
                });
            }
        } else if !step.ready {
            let (Some(source_node), Some(source_path)) = (&step.source_node_id, &step.source_path)
            else {
                return Err(format!(
                    "a fatia L{}-{} não tem de onde vir",
                    step.start_layer, step.end_layer
                ));
            };
            pulls.push(Pull {
                target_node_id: step.target_node_id.clone(),
                target_host: step.target_host.clone(),
                source_node_id: source_node.clone(),
                source_path: source_path.clone(),
                name: step.name.clone(),
                start_layer: step.start_layer,
                end_layer: step.end_layer,
            });
        }
    }

    // ── Phase 1: transfers ───────────────────────────────────────────────────
    //
    // Each target pulls for itself, through the transfer path that already has
    // resume, MD5 checking and a disk-space guard. Nothing new moves bytes here.
    if !pulls.is_empty() {
        set_job(
            job_id,
            "running",
            "copiando fatias",
            &format!(
                "{} fatia(s), {:.1} GB no total",
                pulls.len(),
                plan.transfer_bytes as f64 / 1_073_741_824.0
            ),
        )
        .await;

        let mut watching: Vec<(String, String)> = Vec::new(); // (gateway, job id)
        for pull in &pulls {
            let gateway = gateway_of(&state, &pull.target_node_id)
                .await
                .ok_or_else(|| format!("{} não anuncia porta de gateway", pull.target_host))?;

            let body: serde_json::Value = state
                .http
                .post(format!("{gateway}/api/transfer/pull"))
                .bearer_auth(&key)
                .json(&json!({
                    "node_id": pull.source_node_id,
                    "remote_path": pull.source_path,
                    "name": pull.name,
                }))
                .send()
                .await
                .map_err(|e| format!("{} não aceitou puxar a fatia: {e}", pull.target_host))?
                .json()
                .await
                .unwrap_or(json!({}));

            match body.get("job_id").and_then(|v| v.as_str()) {
                Some(tid) => {
                    info!(
                        "{} puxando L{}-{} de {}",
                        pull.target_host, pull.start_layer, pull.end_layer, pull.source_node_id
                    );
                    watching.push((gateway, tid.to_string()));
                }
                None => {
                    return Err(format!(
                        "{} recusou a cópia: {}",
                        pull.target_host,
                        body.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("motivo não informado")
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
        let target_machine = machines.iter().find(|m| m.node_id == step.target_node_id);

        if step.merged {
            // Each part's path on the target is resolved the same way; the
            // apply merges them into one file there before serving.
            let mut merge: Vec<MergeSource> = Vec::new();
            for part in &step.parts {
                let path = target_machine
                    .and_then(|m| m.holds(part.start_layer, part.end_layer))
                    .map(|s| s.path.clone())
                    .ok_or_else(|| {
                        format!(
                            "{} não achou a fatia L{}-{} depois da cópia",
                            step.target_host, part.start_layer, part.end_layer
                        )
                    })?;
                merge.push(MergeSource {
                    source_model_path: path,
                    start_layer: part.start_layer,
                    end_layer: part.end_layer,
                });
            }
            let anchor = merge
                .first()
                .map(|m| m.source_model_path.clone())
                .ok_or_else(|| format!("{} não tem fatias para juntar", step.target_host))?;

            targets.push(LaunchTarget {
                node_id: step.target_node_id.clone(),
                // Unused when `merge` is set: the apply validates the parts and
                // never reads this. Present only because `LaunchTarget` requires
                // the field; the first part names a real file for the record.
                source_model_path: anchor,
                start_layer: step.start_layer,
                end_layer: step.end_layer,
                // Nothing to cut: the merge produces the exact range.
                skip_split: true,
                merge: Some(merge),
                n_gpu_layers: step.n_gpu_layers,
                offload_kqv: step.offload_kqv,
                // A implantação completa descreve o pipeline inteiro, então a
                // posição na lista de alvos é a posição real.
                peer_index: None,
                peer_count: None,
            });
            continue;
        }

        let path = target_machine
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
            merge: None,
            n_gpu_layers: step.n_gpu_layers,
            offload_kqv: step.offload_kqv,
            peer_index: None,
            peer_count: None,
        });
    }

    // The pipeline is the layer order, so the entry point is whoever holds
    // layer 0 — the same machine the screen marks "processa o prompt".
    let primary = plan
        .steps
        .iter()
        .find(|s| s.primary)
        .map(|s| s.target_node_id.clone());

    // `plan` refuses an unknown size before anything is copied, so this is a
    // fact by the time we get here rather than a default worth inventing.
    let total_layers = plan
        .total_layers
        .ok_or_else(|| format!("as fatias de {} não dizem o total de camadas", plan.group))?;

    let response = launch(
        ConnectionAuth,
        State(state),
        Json(LaunchRequest {
            base_name: plan.group.clone(),
            // `group` is the selected value from the deployment model picker.
            // Its name is the cluster identity, not a heuristic profile name.
            model_label: Some(plan.group.clone()),
            total_layers,
            targets,
            primary_node_id: primary,
            params,
        }),
    )
    .await;

    if !response.status().is_success() {
        return Err(refusal(response).await);
    }
    Ok(())
}

// ── Somar ou tirar uma maquina de um modelo ja no ar ─────────────────────────
//
// Os dois caminhos tocam **uma** maquina. `deploy_endpoint` reimplanta o
// conjunto inteiro, e usar aquele caminho aqui pararia a inferencia de quem ja
// esta servindo - exatamente o que somar uma maquina existe para evitar.

#[derive(Debug, Deserialize)]
pub struct AddNodeRequest {
    pub group: String,
    pub target_node_id: String,
    pub start_layer: u32,
    pub end_layer: u32,
    #[serde(default)]
    pub params: RuntimeParams,
}

#[derive(Debug, Deserialize)]
pub struct RemoveNodeRequest {
    pub group: String,
    pub target_node_id: String,
}

/// Faixas que este grupo tem no ar agora, uma por peer que o esta servindo.
async fn live_ranges(state: &GatewayState, group: &str) -> Vec<(String, u32, u32)> {
    state
        .all_peers()
        .await
        .into_iter()
        .filter(|p| p.model == group && p.total_layers > 0)
        .map(|p| (p.node_id, p.start_layer, p.end_layer))
        .collect()
}

async fn add_node_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<AddNodeRequest>,
) -> Response {
    if req.start_layer > req.end_layer {
        return bad(json!({ "error": "faixa de camadas invertida" }));
    }

    // Re-varre em vez de confiar na tela: a previa pode ter um minuto, e uma
    // maquina que saiu nesse meio tempo receberia uma fatia.
    let machines = survey(&state, &req.group).await;
    let Some(target) = machines.iter().find(|m| m.node_id == req.target_node_id) else {
        return bad(json!({ "error": "a maquina escolhida nao esta no cluster" }));
    };
    let Some(shard) = target.holds(req.start_layer, req.end_layer) else {
        return bad(json!({
            "error": format!(
                "{} nao tem a fatia L{}-{}; mande-a para la antes",
                target.label(), req.start_layer, req.end_layer
            )
        }));
    };
    let source_model_path = shard.path.clone();

    let outras: Vec<(u32, u32)> = live_ranges(&state, &req.group)
        .await
        .into_iter()
        .filter(|(id, _, _)| id != &req.target_node_id)
        .map(|(_, i, f)| (i, f))
        .collect();
    let sobrepostas = overlapping_layers(&outras, (req.start_layer, req.end_layer));

    let candidatos = candidates(&machines);
    let Some(total_layers) = model_size(&candidatos).value() else {
        return bad(json!({
            "error": format!("as fatias de {} nao dizem o total de camadas", req.group)
        }));
    };
    if req.end_layer >= total_layers {
        return bad(json!({
            "error": format!(
                "a fatia L{}-{} passa do fim de um modelo de {total_layers} camadas",
                req.start_layer, req.end_layer
            )
        }));
    }

    // `peer_index`/`peer_count` alimentam duas decisoes so: se este no exporta
    // o estado residual e se ele pula os embeddings. As duas perguntam "sou o
    // ultimo?", e a resposta vem da ordem das camadas no cluster - nao da
    // posicao numa lista de alvos que este caminho, por existir para tocar uma
    // maquina so, nunca vai montar.
    let (peer_index, peer_count) = split_position(&outras, (req.start_layer, req.end_layer));
    let target_node_id = req.target_node_id.clone();
    let response = crate::webui::launch(
        ConnectionAuth,
        State(state),
        Json(LaunchRequest {
            base_name: req.group.clone(),
            model_label: Some(req.group.clone()),
            total_layers,
            // Um alvo so: o `launch` para a inferencia dos alvos que recebe, e
            // esta lista tem exatamente a maquina que esta entrando.
            targets: vec![LaunchTarget {
                node_id: target_node_id.clone(),
                source_model_path,
                start_layer: req.start_layer,
                end_layer: req.end_layer,
                skip_split: true,
                merge: None,
                n_gpu_layers: None,
                offload_kqv: None,
                // Um alvo por requisição, mas a posição é a do pipeline: sem
                // isto o nó somado se diz o último da divisão.
                peer_index: Some(peer_index),
                peer_count: Some(peer_count),
            }],
            // Quem e a entrada do pipeline nao muda por isto: so quem fica com
            // a camada 0 recebe o prompt, e o `launch` so marca `primary` no
            // alvo que ele recebe.
            primary_node_id: if req.start_layer == 0 {
                Some(target_node_id)
            } else {
                None
            },
            params: req.params,
        }),
    )
    .await;

    if !response.status().is_success() {
        let reason = refusal(response).await;
        return bad(json!({ "error": reason }));
    }

    Json(json!({
        "applied": true,
        "overlapping_layers": sobrepostas,
        "peer_index": peer_index,
        "peer_count": peer_count,
    }))
    .into_response()
}

async fn remove_node_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<RemoveNodeRequest>,
) -> Response {
    let vivas = live_ranges(&state, &req.group).await;
    let Some((_, inicio, fim)) = vivas
        .iter()
        .find(|(id, _, _)| id == &req.target_node_id)
        .cloned()
    else {
        return bad(json!({ "error": "essa maquina nao esta servindo este modelo" }));
    };
    let restantes: Vec<(u32, u32)> = vivas
        .iter()
        .filter(|(id, _, _)| id != &req.target_node_id)
        .map(|(_, i, f)| (*i, *f))
        .collect();
    let descobertas = uncovered_after_removal(&restantes, (inicio, fim));

    // Quem libera a memoria e o dono dela, como no botao "Liberar fatia": este
    // no so encaminha o pedido.
    let key = state.auth.read().await.connection_key.clone();
    if req.target_node_id == state.network.node_id {
        state.runtime.stop().await;
    } else {
        let Some(base) = gateway_of(&state, &req.target_node_id).await else {
            return bad(json!({ "error": "peer sem porta de gateway anunciada" }));
        };
        match state
            .http
            .post(format!("{base}/api/ui/unload"))
            .bearer_auth(&key)
            .timeout(PEER_TIMEOUT)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => {
                return bad(json!({ "error": format!("o peer recusou: HTTP {}", r.status()) }))
            }
            Err(e) => return bad(json!({ "error": format!("peer inacessivel: {e}") })),
        }
    }

    Json(json!({
        "removed": true,
        "uncovered_layers": descobertas,
    }))
    .into_response()
}

// ── Tirando a pipeline inteira do ar ─────────────────────────────────────────
//
// O contrário de "Implantar na rede". Remover uma máquina do modelo já existia
// (`/api/ui/deploy/node/remove`), mas desfazer a implantação inteira era
// clicar máquina por máquina, na ordem certa, torcendo para não esquecer
// nenhuma — e uma esquecida é RAM e VRAM presas numa fatia que não serve mais
// para nada, porque o pipeline só planeja com a cobertura fechada.
//
// **O que isto NÃO faz: mexer no disco.** Nenhuma fatia é apagada. Desimplantar
// é devolver a memória; os arquivos continuam onde estão e a mesma implantação
// volta com um clique em vez de com 17 minutos de transferência por máquina.
// Apagar fatias é o que a tela de Modelos faz, com o seu próprio pedido de
// confirmação, e é uma decisão diferente desta.

/// Quem pode tirar a pipeline inteira do ar.
///
/// **Primeira versão: qualquer nó da rede pode.** É deliberado, e é onde a
/// restrição "somente o NÓ PRINCIPAL desimplanta" entra quando chegar a hora:
/// aqui dentro, em nenhum outro lugar. `undeploy_endpoint` já chama esta função
/// antes de tocar em qualquer máquina, então fechar o portão é trocar o corpo
/// desta função por
///
/// ```ignore
/// if role != Role::Principal {
///     return Err("somente o nó principal desimplanta a pipeline".into());
/// }
/// ```
///
/// e atualizar `any_node_may_undeploy_for_now`, o teste que hoje documenta a
/// porta aberta — de propósito, para que fechá-la seja uma edição consciente e
/// não um efeito colateral.
///
/// Note que o portão é do **papel deste nó**, o que recebeu o clique, e não do
/// papel de quem foi chamado: as chamadas para os peers usam `/api/ui/unload`,
/// que é a mesma rota do botão de descarregar uma máquina só e continua valendo
/// para qualquer nó.
fn undeploy_authority(_role: &crate::config::Role) -> Result<(), String> {
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct UndeployRequest {
    /// Modelo cuja pipeline sai do ar. Vazio significa "a que estiver no ar",
    /// que é o caso do painel: ele mostra uma pipeline de cada vez.
    #[serde(default)]
    pub group: String,
}

/// O que aconteceu com **uma** máquina. Uma linha por máquina, sempre — inclusive
/// para a que não respondeu.
#[derive(Debug, Serialize)]
pub struct UndeployOutcome {
    pub node_id: String,
    pub hostname: String,
    pub start_layer: u32,
    pub end_layer: u32,
    /// `true` só quando a máquina confirmou. Um peer calado é `false` com o
    /// motivo em `error`; contá-lo como sucesso seria dizer que a memória foi
    /// liberada sem ninguém ter dito isso.
    pub released: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Descarrega o modelo de todas as máquinas que servem `group`.
///
/// Responde 200 com o relatório mesmo quando alguma máquina falha: metade
/// desimplantada é o estado real do cluster, e escondê-lo atrás de um erro
/// deixaria o operador sem saber em quais máquinas ainda há memória presa.
/// Só pedido inválido (nenhuma pipeline no ar, ou ambiguidade sobre qual) é 400.
async fn undeploy_endpoint(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(req): Json<UndeployRequest>,
) -> Response {
    {
        let role = state.auth.read().await.role.clone();
        if let Err(e) = undeploy_authority(&role) {
            return (axum::http::StatusCode::FORBIDDEN, Json(json!({ "error": e }))).into_response();
        }
    }

    let peers = state.all_peers().await;
    let group = match resolve_group(&req.group, &peers) {
        Ok(g) => g,
        Err(e) => return bad(json!({ "error": e })),
    };

    // Re-varre em vez de confiar na tela, como o `deploy`: uma máquina que saiu
    // no meio tempo não pode virar uma linha de falha inventada.
    let serving: Vec<PeerInfo> = peers
        .into_iter()
        .filter(|p| p.model == group && p.total_layers > 0)
        .collect();
    if serving.is_empty() {
        return bad(json!({
            "error": format!("nenhuma máquina está servindo {group}")
        }));
    }

    info!(
        "desimplantando {group} de {} máquina(s)",
        serving.len()
    );

    let key = state.auth.read().await.connection_key.clone();
    let self_id = state.network.node_id.clone();

    // Em paralelo: com onze peers e o prazo de um controle por peer, em série a
    // tela ficaria quase um minuto sem resposta.
    let mut results: Vec<UndeployOutcome> =
        futures::future::join_all(serving.into_iter().map(|peer| {
            let state = state.clone();
            let key = key.clone();
            let self_id = self_id.clone();
            async move {
                let error = release_one(&state, &peer, &key, &self_id).await.err();
                UndeployOutcome {
                    node_id: peer.node_id.clone(),
                    hostname: if peer.hostname.is_empty() {
                        peer.node_id[..12.min(peer.node_id.len())].to_string()
                    } else {
                        peer.hostname.clone()
                    },
                    start_layer: peer.start_layer,
                    end_layer: peer.end_layer,
                    released: error.is_none(),
                    error,
                }
            }
        }))
        .await;
    results.sort_by_key(|r| r.start_layer);

    let released = results.iter().filter(|r| r.released).count();
    let failed = results.len() - released;
    if failed > 0 {
        warn!("{failed} máquina(s) não confirmaram a desimplantação de {group}");
    }

    Json(json!({
        "group": group,
        "released": released,
        "failed": failed,
        "results": results,
    }))
    .into_response()
}

/// Qual pipeline o pedido quer derrubar.
///
/// Vazio e uma só no ar é a intenção óbvia. Vazio com duas é ambiguidade que
/// esta função se recusa a resolver por conta própria: derrubar a errada custa
/// uma reimplantação inteira.
fn resolve_group(requested: &str, peers: &[PeerInfo]) -> Result<String, String> {
    if !requested.is_empty() {
        return Ok(requested.to_string());
    }
    let mut models: Vec<String> = peers
        .iter()
        .filter(|p| p.total_layers > 0 && !p.model.is_empty())
        .map(|p| p.model.clone())
        .collect();
    models.sort();
    models.dedup();
    match models.len() {
        0 => Err("nenhuma pipeline no ar para desimplantar".to_string()),
        1 => Ok(models.remove(0)),
        _ => Err(format!(
            "há mais de uma pipeline no ar ({}); diga qual desimplantar",
            models.join(", ")
        )),
    }
}

/// Libera a memória de **uma** máquina, pela mesma porta do botão que já existe.
///
/// Quem mata o processo de inferência é sempre o dono da memória: este nó só
/// encaminha, exatamente como `remove_node_endpoint`.
async fn release_one(
    state: &GatewayState,
    peer: &PeerInfo,
    key: &str,
    self_id: &str,
) -> Result<(), String> {
    if peer.node_id == self_id {
        state.runtime.stop().await;
        return Ok(());
    }
    let base = peer
        .gateway_url()
        .ok_or_else(|| "peer sem porta de gateway anunciada".to_string())?;
    match state
        .http
        .post(format!("{base}/api/ui/unload"))
        .bearer_auth(key)
        .timeout(PEER_TIMEOUT)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(format!("a máquina recusou: HTTP {}", r.status())),
        Err(e) => Err(format!("máquina inacessível: {e}")),
    }
}

fn bad(body: serde_json::Value) -> Response {
    (axum::http::StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// The reason a refused launch gives, not just its status code.
///
/// `launch` answers `{"error": "..."}` and the operator only ever saw
/// "HTTP 400", which is the same dead end this whole screen exists to avoid.
async fn refusal(response: Response) -> String {
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|s| s.to_string())
        });
    match body {
        Some(reason) => format!("o launch recusou: {reason}"),
        None => format!("o launch recusou: HTTP {status}"),
    }
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
                        job.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("sem detalhe")
                    ))
                }
                _ => {
                    let total = job
                        .get("total_bytes")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    let got = job
                        .get("received_bytes")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
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

/// Tirar a pipeline inteira do ar.
#[cfg(test)]
mod undeploy_tests {
    use super::*;
    use crate::config::Role;
    use crate::gateway::testkit::{body_of, state_with};
    use axum::routing::post as axum_post;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    /// Um peer servindo `qwen35` na faixa dada.
    fn peer(node_id: &str, host: &str, gateway_port: u16, range: (u32, u32)) -> PeerInfo {
        let mut p = crate::gateway::testkit::peer(node_id, host, gateway_port);
        p.model = "qwen35".into();
        p.start_layer = range.0;
        p.end_layer = range.1;
        p.total_layers = 32;
        p
    }

    /// Uma máquina que responde `/api/ui/unload` com o status pedido, contando
    /// quantas vezes foi chamada.
    async fn fake_machine(status: axum::http::StatusCode) -> (u16, Arc<AtomicU32>) {
        let hits = Arc::new(AtomicU32::new(0));
        let counter = hits.clone();
        let app = Router::new().route(
            "/api/ui/unload",
            axum_post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (status, Json(json!({ "unloaded": true })))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, hits)
    }

    /// O relatório tem uma linha por máquina, e uma máquina que não confirmou
    /// **não** conta como liberada. Fingir sucesso aqui deixaria memória presa
    /// numa caixa que o operador acha que esvaziou.
    #[tokio::test]
    async fn every_machine_is_reported_and_a_silent_one_is_not_a_success() {
        let (ok_port, ok_hits) = fake_machine(axum::http::StatusCode::OK).await;
        let (bad_port, bad_hits) = fake_machine(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        let state = state_with("eu", vec![
            peer("no-a", "ksia", ok_port, (0, 15)),
            peer("no-b", "macbook", bad_port, (16, 31)),
        ])
        .await;

        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                group: "qwen35".into(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_of(resp).await;

        assert_eq!(body["group"], "qwen35");
        assert_eq!(body["released"], 1);
        assert_eq!(body["failed"], 1);

        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "uma linha por máquina, sempre");
        // Ordenado pela camada inicial: o relatório se lê na ordem do pipeline.
        assert_eq!(results[0]["hostname"], "ksia");
        assert_eq!(results[0]["released"], true);
        assert!(results[0].get("error").is_none());

        assert_eq!(results[1]["hostname"], "macbook");
        assert_eq!(results[1]["released"], false);
        assert!(
            results[1]["error"].as_str().unwrap().contains("500"),
            "o motivo tem que ser o que a máquina disse: {}",
            results[1]["error"]
        );

        // As duas foram mesmo chamadas: nenhuma foi pulada por causa da outra.
        assert_eq!(ok_hits.load(Ordering::SeqCst), 1);
        assert_eq!(bad_hits.load(Ordering::SeqCst), 1);
    }

    /// Uma máquina fora do ar é falha com motivo, não uma linha que some.
    #[tokio::test]
    async fn an_unreachable_machine_still_gets_a_line() {
        let (ok_port, _) = fake_machine(axum::http::StatusCode::OK).await;
        // Porta que ninguém atende: reservada e imediatamente devolvida.
        let dead = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        let state = state_with("eu", vec![
            peer("no-a", "ksia", ok_port, (0, 15)),
            peer("no-b", "sumida", dead, (16, 31)),
        ])
        .await;

        let body = body_of(
            undeploy_endpoint(
                ConnectionAuth,
                State(state),
                Json(UndeployRequest {
                    group: "qwen35".into(),
                }),
            )
            .await,
        )
        .await;

        assert_eq!(body["failed"], 1);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results[1]["hostname"], "sumida");
        assert_eq!(results[1]["released"], false);
        assert!(results[1]["error"].is_string());
    }

    /// Sem pipeline no ar não há o que desimplantar, e dizer isso é melhor que
    /// devolver um relatório vazio que se lê como sucesso.
    #[tokio::test]
    async fn undeploying_nothing_is_a_refusal_not_an_empty_report() {
        let state = state_with("eu", vec![]).await;
        let resp = undeploy_endpoint(
            ConnectionAuth,
            State(state),
            Json(UndeployRequest {
                group: String::new(),
            }),
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    /// Qual pipeline derrubar: óbvio com uma, recusa com duas. Derrubar a
    /// errada custa uma reimplantação inteira, então o palpite não é nosso.
    #[test]
    fn the_group_is_only_guessed_when_there_is_no_doubt() {
        let uma = vec![peer("a", "h", 1, (0, 15)), peer("b", "h", 2, (16, 31))];
        assert_eq!(resolve_group("", &uma).unwrap(), "qwen35");
        assert_eq!(resolve_group("outro", &uma).unwrap(), "outro");

        let mut duas = uma.clone();
        duas[1].model = "llama3".into();
        let erro = resolve_group("", &duas).unwrap_err();
        assert!(erro.contains("llama3") && erro.contains("qwen35"), "{erro}");

        assert!(resolve_group("", &[]).is_err());
    }

    /// **Documenta a porta aberta de propósito.** Nesta primeira versão qualquer
    /// nó desimplanta; quando a restrição "somente o nó principal" entrar em
    /// `undeploy_authority`, é este teste que tem que mudar junto - e é isso que
    /// torna o fechamento uma decisão, e não um efeito colateral.
    #[test]
    fn any_node_may_undeploy_for_now() {
        assert!(undeploy_authority(&Role::Node).is_ok());
        assert!(undeploy_authority(&Role::Principal).is_ok());
    }
}

#[cfg(test)]
mod tests {
    /// Um no somado no meio do pipeline NAO e o ultimo da divisao.
    ///
    /// `peer_index`/`peer_count` respondem a uma pergunta so - "sou o ultimo?" -
    /// e dela saem duas decisoes do llama.cpp: exportar o estado residual e
    /// pular os embeddings. O caminho incremental manda um alvo por vez, e
    /// deixar o `launch` deduzir a posicao da lista faria todo no somado se
    /// declarar o ultimo. O sintoma e o peer do meio responder
    /// "get_embeddings_ith: invalid embeddings id 0, reason: no embeddings",
    /// porque o motor subiu sem captura e a inferencia so descobre no primeiro
    /// token.
    #[test]
    fn a_node_added_in_the_middle_is_not_the_tail_of_the_split() {
        let (index, count) = super::split_position(&[(0, 11), (32, 39)], (12, 31));
        assert_eq!(count, 3, "tres faixas depois de somar esta");
        assert_eq!(index, 1, "12-31 fica no meio, entre 0-11 e 32-39");
        assert!(index + 1 < count, "nao pode se dizer o ultimo da divisao");
    }

    #[test]
    fn a_node_added_at_the_tail_is_the_tail_of_the_split() {
        let (index, count) = super::split_position(&[(0, 11), (12, 31)], (32, 39));
        assert_eq!((index, count), (2, 3));
    }

    /// Substituir quem ja serve aquela faixa nao inventa um peer a mais.
    #[test]
    fn replacing_the_same_range_keeps_the_count() {
        let (index, count) = super::split_position(&[(0, 11), (12, 31)], (12, 31));
        assert_eq!((index, count), (1, 2));
    }

    /// Faixa sobreposta no cluster deixa de ser erro: e assim que se soma uma
    /// rota paralela. A invariante "uma camada, um no" continua valendo, mas
    /// dentro de um plano, e quem a garante e `plan_pipeline`, que varre camada
    /// a camada escolhendo um peer por posicao.
    #[test]
    fn overlapping_ranges_are_reported_but_not_refused() {
        let sobrepostas = super::overlapping_layers(&[(0, 19), (20, 39)], (0, 19));
        assert_eq!(sobrepostas, (0..=19).collect::<Vec<u32>>());
    }

    #[test]
    fn a_range_that_fills_a_gap_overlaps_nothing() {
        assert!(super::overlapping_layers(&[(0, 19)], (20, 39)).is_empty());
    }

    #[test]
    fn a_partial_overlap_reports_only_the_shared_layers() {
        assert_eq!(
            super::overlapping_layers(&[(0, 19)], (15, 25)),
            vec![15, 16, 17, 18, 19]
        );
    }

    /// Tirar uma replica nao descobre camada nenhuma: a rede so perde uma rota
    /// paralela. Avisar de buraco aqui seria mentira.
    #[test]
    fn removing_a_replica_uncovers_nothing() {
        assert!(super::uncovered_after_removal(&[(0, 19), (20, 39)], (0, 19)).is_empty());
    }

    /// Tirar a ultima daquela faixa descobre as camadas dela, e o pipeline para
    /// de planejar ate alguem assumir.
    #[test]
    fn removing_the_last_holder_uncovers_its_layers() {
        assert_eq!(
            super::uncovered_after_removal(&[(20, 39)], (0, 19)),
            (0..=19).collect::<Vec<u32>>(),
        );
    }

    #[test]
    fn a_partially_replicated_range_uncovers_only_the_rest() {
        assert_eq!(
            super::uncovered_after_removal(&[(0, 9), (20, 39)], (0, 19)),
            (10..=19).collect::<Vec<u32>>()
        );
    }

    use super::*;

    const GIB: u64 = 1_073_741_824;

    struct Spec {
        id: &'static str,
        mem_free_mb: u64,
        disk_free_mb: u64,
        held: Vec<(u32, u32)>,
        /// What each of this machine's slices declares as the model's size.
        /// `None` is a slice cut before `rdmap2p.original_block_count` existed.
        declares: Option<u32>,
    }

    fn m(id: &'static str, mem_free_mb: u64, held: &[(u32, u32)]) -> Spec {
        Spec {
            id,
            mem_free_mb,
            disk_free_mb: 500_000,
            held: held.to_vec(),
            declares: Some(32),
        }
    }

    /// A machine whose slices carry no layer total, like the captain's
    /// `qwen_0_15.gguf` / `qwen_16_31.gguf`.
    fn nameless_machine(id: &'static str, mem_free_mb: u64, held: &[(u32, u32)]) -> Spec {
        Spec {
            declares: None,
            ..m(id, mem_free_mb, held)
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
                        total_layers: s.declares,
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
            offload_kqv: None,
        }
    }

    /// The knobs are their own axis; every test below is about the placement,
    /// so they all run on the defaults unless they say otherwise.
    fn plan(group: &str, machines: &[Machine], selection: Option<&[SelectedSlice]>) -> Plan {
        super::plan(group, machines, selection, &RuntimeParams::default(), false)
    }

    /// The merge axis, flipped on; everything else still defaults.
    fn merged(group: &str, machines: &[Machine], selection: &[SelectedSlice]) -> Plan {
        super::plan(
            group,
            machines,
            Some(selection),
            &RuntimeParams::default(),
            true,
        )
    }

    // ── The suggestion ──────────────────────────────────────────────────────

    /// The happy shape: each machine already has one of the slices, so the
    /// deploy is pure launch and copies nothing.
    #[test]
    fn slices_already_in_place_need_no_transfer() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 200_000, &[(16, 31)]),
        ]);
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
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(20, 31)]),
        ]);
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

    // ── Merged units ─────────────────────────────────────────────────────────

    /// The whole point of the toggle: adjacent slices on the same machine
    /// become one file, so the machine serves the union. Everything is already
    /// there, so nothing travels and the plan is complete with one step fewer.
    #[test]
    fn merge_joins_adjacent_slices_on_one_machine() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (8, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let p = merged(
            "m",
            &ms,
            &[pick(0, 7, "a"), pick(8, 15, "a"), pick(16, 31, "b")],
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete, "0-7 + 8-15 + 16-31 cobrem 32 camadas");
        assert_eq!(p.transfer_bytes, 0, "a já tem as duas fatias");
        assert_eq!(p.steps.len(), 2);

        let joined = p.steps.iter().find(|s| s.merged).unwrap();
        assert_eq!((joined.start_layer, joined.end_layer), (0, 15));
        assert!(joined.ready);
        assert_eq!(joined.parts.len(), 2, "duas fatias entram no arquivo");
        assert!(joined.parts.iter().all(|part| part.ready));
        assert_eq!(joined.name, "m_0_15.gguf", "nome do arquivo junto");

        let solo = p.steps.iter().find(|s| !s.merged).unwrap();
        assert_eq!((solo.start_layer, solo.end_layer), (16, 31));
        // The merged machine's table row names the union.
        let a = p.machines.iter().find(|m| m.node_id == "a").unwrap();
        assert_eq!(a.taking, Some([0, 15]));
    }

    /// Merging is what makes a machine with two files able to serve them both;
    /// without the toggle the same two ranges stay the old refusal.
    #[test]
    fn the_toggle_is_what_allows_two_ranges_on_one_machine() {
        let ms = build(vec![m("a", 100_000, &[(0, 7), (8, 15)])]);
        let p = merged("m", &ms, &[pick(0, 7, "a"), pick(8, 15, "a")]);
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.steps.len() == 1 && p.steps[0].merged);
    }

    /// `splitter::merge` only knows how to join neighbours, so a machine whose
    /// two ranges are separated by a gap cannot serve both — the refusal names
    /// the gap.
    #[test]
    fn merge_requires_adjacency() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7), (9, 15)]),
            m("b", 100_000, &[(8, 8)]),
        ]);
        let p = merged(
            "m",
            &ms,
            &[pick(0, 7, "a"), pick(9, 15, "a"), pick(8, 8, "b")],
        );
        let blocked = p.blocked.expect("fatias separadas não se juntam");
        assert!(blocked.contains("vizinhas"), "{blocked}");
    }

    /// A merged unit may need copies: each part that is missing has its own
    /// source, and the transfer byte count is the union of the missing ones.
    #[test]
    fn merged_parts_keep_their_sources() {
        let ms = build(vec![
            m("a", 100_000, &[(0, 7)]),
            m("b", 100_000, &[(8, 15)]),
            m("d", 100_000, &[(0, 7), (8, 15)]),
            m("e", 100_000, &[(16, 31)]),
            m("c", 100_000, &[]),
        ]);
        let p = merged(
            "m",
            &ms,
            &[pick(0, 7, "c"), pick(8, 15, "c"), pick(16, 31, "e")],
        );
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        let joined = p.steps.iter().find(|s| s.merged).unwrap();
        assert!(!joined.ready);
        assert_eq!(p.transfer_bytes, 2 * GIB, "as duas fatias viajam");
        let first = &joined.parts[0];
        assert_eq!(first.source_node_id.as_deref(), Some("a"));
        let second = &joined.parts[1];
        assert_eq!(second.source_node_id.as_deref(), Some("b"));
        // A machine holding every part is offered as ready for the merged unit.
        let option = joined
            .options
            .iter()
            .find(|o| o.node_id == "d" && o.has)
            .expect("d tem as duas fatias, logo está pronta para juntar");
        assert!(option.ok);
    }

    /// A merged unit whose target holds no part and no machine holds a part is
    /// a dead end, reported with the offending range named.
    #[test]
    fn a_merged_unit_with_an_unobtainable_part_is_refused() {
        let ms = build(vec![m("a", 100_000, &[(0, 7)])]);
        let p = merged("m", &ms, &[pick(0, 7, "a"), pick(8, 15, "a")]);
        let blocked = p.blocked.expect("L8-15 não existe em máquina nenhuma");
        assert!(blocked.contains("L8-15"), "{blocked}");
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
        let p = plan(
            "m",
            &ms,
            Some(&[pick(0, 15, "a"), pick(16, 31, "fantasma")]),
        );
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
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
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
        let ms = build(vec![
            m("a", 100_000, &[(0, 15)]),
            m("b", 100_000, &[(16, 31)]),
        ]);
        let params = RuntimeParams {
            n_ctx: 32768,
            n_batch: 3072,
            max_sessions: 2,
            max_new_tokens: 256,
            prefill_chunk_tokens: 4096,
            ..RuntimeParams::default()
        };
        let p = super::plan("m", &ms, None, &params, false);

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

    /// Onde o cache KV mora é decidido pela VRAM da máquina, e as máquinas do
    /// cluster não se parecem: a escolha feita para uma não pode aparecer na
    /// outra, nem apagar a que não foi escolhida.
    #[test]
    fn per_machine_kv_cache_rides_along_with_the_slice() {
        let ms = build(vec![
            m("com-placa", 100_000, &[(0, 15)]),
            m("so-cpu", 100_000, &[(16, 31)]),
        ]);
        let sel = [
            pick(0, 15, "com-placa"),
            SelectedSlice {
                offload_kqv: Some(false),
                ..pick(16, 31, "so-cpu")
            },
        ];
        let p = plan("m", &ms, Some(&sel));
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert_eq!(p.steps[0].offload_kqv, None, "herda a escolha do cluster");
        assert_eq!(p.steps[1].offload_kqv, Some(false));
        // Um eixo não mexe no outro: nenhuma das duas pediu offload.
        assert_eq!(p.steps[1].n_gpu_layers, None);
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

    // ── How many layers the model has ───────────────────────────────────────
    //
    // Reproduced on the captain's own cluster: both machines answer
    // `"total_layers": null` for the `qwen` set (slices cut before
    // `split_model.py` wrote `rdmap2p.original_block_count`). That `null`
    // became a `0`, the plan called itself complete, and every peer's apply
    // died on `runtime::NodeConfig::validate` with "total_layers não pode ser
    // zero" — after `launch` had already stopped inference on all of them.

    /// The exact shape of the captain's `qwen` set: no metadata anywhere, but
    /// the two ranges plainly reach layer 32 between them.
    #[test]
    fn slices_without_metadata_take_the_total_from_their_coverage() {
        let ms = build(vec![
            nameless_machine("a", 100_000, &[(0, 15)]),
            nameless_machine("b", 200_000, &[(16, 31)]),
        ]);
        let p = plan("qwen", &ms, None);

        assert_eq!(
            p.total_layers,
            Some(32),
            "0-15 e 16-31 alcançam a camada 32"
        );
        assert!(p.total_layers_inferred, "veio da cobertura, não do arquivo");
        assert!(p.blocked.is_none(), "{:?}", p.blocked);
        assert!(p.complete);
        assert!(
            p.warnings.iter().any(|w| w.contains("deduzido")),
            "a tela tem de dizer que o total foi deduzido: {:?}",
            p.warnings
        );
    }

    /// The plan is what `launch` is handed, so a zero here is a peer that stops
    /// serving for nothing. There is no path that produces one.
    #[test]
    fn a_plan_never_carries_a_total_of_zero() {
        for machines in [
            build(vec![nameless_machine("a", 100_000, &[(0, 15), (16, 31)])]),
            build(vec![nameless_machine("a", 100_000, &[(0, 31)])]),
            build(vec![m("a", 100_000, &[(0, 15)])]),
        ] {
            let p = plan("m", &machines, None);
            assert_ne!(p.total_layers, Some(0));
            assert!(p.total_layers.is_some() || p.blocked.is_some());
        }
    }

    /// Nothing declares a total and nothing starts at layer 0: the honest
    /// answer is a refusal that names the files, not a guess and not a zero.
    #[test]
    fn a_set_that_cannot_say_its_size_is_refused_by_name() {
        let ms = build(vec![nameless_machine("a", 100_000, &[(16, 31)])]);
        let p = plan("qwen", &ms, None);

        assert_eq!(p.total_layers, None);
        let reason = p.blocked.expect("sem total e sem camada 0 é recusa");
        assert!(
            reason.contains("m_16_31.gguf"),
            "a recusa tem de dizer qual fatia está sem o dado: {reason}"
        );
        assert!(
            reason.contains("Fatiar"),
            "e o que fazer a respeito: {reason}"
        );
    }

    /// Metadata is a fact and coverage is a deduction, so the fact wins — and
    /// the layers the set does not reach are reported as missing instead of
    /// disappearing with the total.
    #[test]
    fn a_declared_total_beats_the_coverage_and_exposes_the_gap() {
        let mut ms = build(vec![m("a", 100_000, &[(0, 15)])]);
        ms[0].held[0].total_layers = Some(40);
        let p = plan("m", &ms, None);

        assert_eq!(p.total_layers, Some(40));
        assert!(!p.total_layers_inferred);
        assert_eq!(p.missing, vec![[16, 39]]);
        assert!(!p.complete);
    }

    /// Half a nameless set is a half deploy, and before the fix it was the
    /// silent one: with the total at zero the tail check was skipped, the plan
    /// called itself complete and the screen armed the button.
    #[test]
    fn half_of_a_nameless_set_is_reported_as_a_gap() {
        let ms = build(vec![nameless_machine("a", 100_000, &[(0, 15), (16, 31)])]);
        let p = plan("qwen", &ms, Some(&[pick(0, 15, "a")]));

        assert_eq!(p.total_layers, Some(32));
        assert_eq!(p.missing, vec![[16, 31]], "L16-31 ficou de fora");
        assert!(!p.complete, "meia rede não é uma rede completa");
    }

    /// The deduction walks the reach from layer 0, so a model sliced twice with
    /// different cuts still resolves to one size, and a set that starts later
    /// resolves to none.
    #[test]
    fn the_deduction_walks_the_reach_from_layer_zero() {
        let size = |held: &[(u32, u32)]| {
            let ms = build(vec![nameless_machine("a", 100_000, held)]);
            model_size(&candidates(&ms))
        };
        assert_eq!(size(&[(0, 15), (16, 31)]), ModelSize::Inferred(32));
        // Two different cuts of the same model, side by side.
        assert_eq!(
            size(&[(0, 4), (5, 31), (0, 15), (16, 31)]),
            ModelSize::Inferred(32)
        );
        // A hole stops the reach where the hole is.
        assert_eq!(size(&[(0, 15), (20, 31)]), ModelSize::Inferred(16));
        assert_eq!(size(&[(4, 31)]), ModelSize::Unknown);
        assert_eq!(size(&[]), ModelSize::Unknown);
    }

    /// A peer running an older build answers `"total_layers": 0` where this one
    /// answers `null`. Both mean "this file does not say".
    #[test]
    fn a_shard_that_reports_zero_layers_is_a_shard_with_no_total() {
        let shard = parse_shard(&json!({
            "shard_range": [0, 15],
            "total_layers": 0,
            "name": "qwen_0_15.gguf",
            "path": "/models/qwen/qwen_0_15.gguf",
            "size_bytes": 2569314208u64,
        }))
        .expect("a fatia é válida, só não diz o total");
        assert_eq!(shard.total_layers, None);

        let declared = parse_shard(&json!({
            "shard_range": [0, 5],
            "total_layers": 40,
            "name": "phi_0_5.gguf",
            "path": "/models/phi/phi_0_5.gguf",
            "size_bytes": 1u64,
        }))
        .unwrap();
        assert_eq!(declared.total_layers, Some(40));
    }
}
