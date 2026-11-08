use super::*;
// ── The decision ─────────────────────────────────────────────────────────────

/// A range that exists somewhere in the cluster, selected or not.
///
/// The same model is often sliced more than once — 0-4 / 5-31 on one machine and
/// 0-10 / 11-31 on another — and every one of those cuts is a legitimate thing
/// to deploy. The screen needs all of them to offer the choice, so the plan
/// carries the whole menu and not only the part it picked.
#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    /// Only what the slice itself declares. `null` on screen means "this file
    /// does not say how big the model is", which is a fact about the file.
    pub(super) total_layers: Option<u32>,
    pub(super) name: String,
    pub(super) size_bytes: u64,
    /// Every machine that already has this exact range.
    pub(super) holders: Vec<String>,
    /// Part of the plan being shown.
    pub(super) selected: bool,
}

/// A machine that could take a range, as the screen has to describe it.
#[derive(Debug, Clone, Serialize)]
pub struct MachineView {
    pub(super) node_id: String,
    pub(super) label: String,
    pub(super) mem_free_mb: u64,
    pub(super) gpu_free_mb: u64,
    pub(super) ram_free_mb: u64,
    pub(super) disk_free_mb: u64,
    pub(super) rtt_ms: Option<f64>,
    /// In-flight requests reported by the machine itself. `None` means the
    /// survey could not read that machine's live node data.
    pub(super) load: Option<u32>,
    /// Ranges of this set it already holds.
    pub(super) holds: Vec<[u32; 2]>,
    /// Range it takes in the plan being shown, if any.
    pub(super) taking: Option<[u32; 2]>,
    /// Capacity projection of this machine for the plan being prepared - the sum of the
    /// steps it would take, per GPU. `None` until the preview
    /// finishes (or when no step of this machine is materialized
    /// locally yet). See `enrich_capacity_previews`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capacity: Option<MachineCapacity>,
}

/// VRAM projection of a whole machine: the sum, per GPU, of every step of the
/// plan it would take. A machine with two non-adjacent steps (two
/// files, two processes) sums both over the same inventory of
/// devices - it is the same physical card receiving both.
#[derive(Debug, Clone, Serialize)]
pub struct MachineCapacity {
    pub(super) fits: bool,
    /// Some step of this machine was flagged by the heuristic that
    /// `fit_memory` may trim the request (context or GPU layers).
    pub(super) degraded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) note: Option<String>,
    pub(super) gpus: Vec<crate::webui::GpuCapacityProjection>,
}

/// One machine offered as the target of one slice, with the reason it works.
#[derive(Debug, Clone, Serialize)]
pub struct Alternative {
    pub(super) node_id: String,
    pub(super) label: String,
    /// Already has the file: nothing to copy.
    pub(super) has: bool,
    /// Nothing about this machine stops the assignment.
    pub(super) ok: bool,
    /// What it costs, or why it does not work.
    pub(super) note: Option<String>,
}

/// One slice feeding a merged step: the range, its file, and where it comes
/// from. A merged step serves the union of its parts, joined into one file on
/// the target.
#[derive(Debug, Clone, Serialize)]
pub struct StepPart {
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    pub(super) name: String,
    pub(super) size_bytes: u64,
    /// Already on the target: nothing to copy for this part.
    pub(super) ready: bool,
    /// Who sends it when it is not. `None` together with `ready: false` means
    /// nobody in the cluster has this slice at all.
    pub(super) source_node_id: Option<String>,
    pub(super) source_host: Option<String>,
    pub(super) source_path: Option<String>,
    /// Hash of the part at the source. It is what the copy is requested by: this way the
    /// target writes to `<shard_dir>/<model>/<sha>.gguf` instead of dropping the
    /// file loose, and knows how to check what arrived.
    pub(super) source_sha256: Option<String>,
    /// True when the source is a virtual range that still has to be cut out
    /// of its whole GGUF before it can be transferred or launched.
    pub(super) source_needs_split: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    /// 1-based place in the pipeline. Position 1 receives the prompt.
    pub(super) position: u32,
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    /// The model size this step will be launched with — the plan's answer, not
    /// the file's, so every step of one deploy carries the same number.
    pub(super) total_layers: Option<u32>,
    pub(super) name: String,
    pub(super) size_bytes: u64,
    pub(super) target_node_id: String,
    pub(super) target_host: String,
    /// Already on the target: nothing to copy, this one starts immediately.
    pub(super) ready: bool,
    /// Who sends it when it is not. `None` together with `ready: false` means
    /// nobody in the cluster has this slice at all.
    pub(super) source_node_id: Option<String>,
    pub(super) source_host: Option<String>,
    pub(super) source_path: Option<String>,
    /// Hash of the part at the source. It is what the copy is requested by: this way the
    /// target writes to `<shard_dir>/<model>/<sha>.gguf` instead of dropping the
    /// file loose, and knows how to check what arrived.
    pub(super) source_sha256: Option<String>,
    /// True when the source is a virtual range backed by a whole GGUF.
    pub(super) source_needs_split: bool,
    /// The target's own whole GGUF this range comes from, when the target is
    /// also the virtual range's source. Nothing is cut or copied by the deploy:
    /// the target is applied with the whole file and the range, and cuts its
    /// range locally, or points at it with a virtual slice
    /// (`webui::jobs_apply::run_apply`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) from_whole_path: Option<String>,
    /// Where it will be on the target once it lands, when known up front.
    pub(super) target_path: Option<String>,
    /// Entry point of the pipeline — whoever holds layer 0.
    pub(super) primary: bool,
    /// Per-machine GPU offload, when the operator set one. `None` means this
    /// peer takes `params.n_gpu_layers`.
    pub(super) n_gpu_layers: Option<u32>,
    /// Per-machine KV cache placement. `None` takes `params.offload_kqv`.
    pub(super) offload_kqv: Option<bool>,
    /// Per-machine prefill/decode role. `None` takes `params.inference_role`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) inference_role: Option<crate::webui::InferenceRole>,
    /// Every machine the operator could move this range to.
    pub(super) options: Vec<Alternative>,
    /// Set when this assignment cannot be carried out, and why.
    pub(super) problem: Option<String>,
    /// Set when it can, but the operator should read it first.
    pub(super) note: Option<String>,
    /// True when this step serves the union of `parts`, joined into one file
    /// on the target, instead of a single slice already in place.
    pub(super) merged: bool,
    /// The slices this step is assembled from, in layer order. Empty for a
    /// plain step.
    pub(super) parts: Vec<StepPart>,
    /// VRAM/RAM projection this step would take on this target, with the
    /// parameters of the plan being prepared. `None` until a locally materialized
    /// step (`target_path.is_some() && !merged`) is queried, or
    /// when that query failed - see `enrich_capacity_previews`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capacity: Option<crate::webui::CapacityPreview>,
}

/// One rank of an already sliced TP round. Unlike a `Step`, each rank
/// covers the whole model and must be started on the corresponding peer of the
/// collective, without going through the shard extractor again.
#[derive(Debug, Clone, Serialize)]
pub struct TpTarget {
    pub(super) rank: u32,
    pub(super) size: u32,
    pub(super) start_layer: u32,
    pub(super) end_layer: u32,
    pub(super) total_layers: Option<u32>,
    pub(super) name: String,
    pub(super) size_bytes: u64,
    pub(super) target_node_id: String,
    pub(super) target_host: String,
    pub(super) ready: bool,
    pub(super) source_node_id: Option<String>,
    pub(super) source_host: Option<String>,
    pub(super) source_path: Option<String>,
    pub(super) source_sha256: Option<String>,
    pub(super) target_path: Option<String>,
    pub(super) problem: Option<String>,
    pub(super) n_gpu_layers: Option<u32>,
    pub(super) offload_kqv: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub(super) group: String,
    pub(super) deployment_name: String,
    pub(super) strategy: DeployStrategy,
    /// Echoed so the screen never has to guess which question it asked. A whole
    /// model plan has exactly one step covering every layer.
    pub(super) kind: DeployKind,
    /// Projector selected by the catalog for the model, if any.
    pub(super) mmproj_path: Option<String>,
    /// All compatible projectors reported by the selected pipeline head.
    pub(super) mmproj_candidates: Vec<String>,
    /// Catalog reason when no projector could be associated.
    pub(super) mmproj_reason: Option<String>,
    /// True only when the surveyed GGUF metadata reports more than one expert.
    /// The UI must never infer this from a filename or architecture label.
    pub(super) is_moe: bool,
    /// Whether this plan can honor the requested inference mode.
    pub(super) inference_mode_supported: bool,
    /// Explicit reason when the selected execution mode cannot be honored.
    pub(super) inference_mode_reason: Option<String>,
    /// Conditional capabilities determined for the targets of this plan.
    pub(super) capabilities: crate::capabilities::DeploymentCapabilities,
    /// Real cut points from the whole GGUF proposer, when this model can be
    /// split locally for a destination that lacks the selected slice.
    pub(super) layer_options: Option<WholeLayerOptions>,
    /// Tensor parallelism group, when kind == tensor_parallel. The launch
    /// uses this to generate the workers' targets.
    pub(super) tp: Option<TpLaunch>,
    /// How many layers the original model has. `None` means nobody could say —
    /// which is a refusal (`blocked`), never a zero passed down the line.
    pub(super) total_layers: Option<u32>,
    /// True when that number was read off the coverage of the slices instead of
    /// out of their metadata. The deploy still runs; the screen has to say so,
    /// because a set of slices that stops at L31 looks exactly the same whether
    /// the model has 32 layers or 40 and the tail is missing from the cluster.
    pub(super) total_layers_inferred: bool,
    pub(super) steps: Vec<Step>,
    /// Targets per rank when the group was obtained from `tp_parts`. Stays empty on the
    /// fallback path that extracts the ranks from whole models.
    pub(super) tp_targets: Vec<TpTarget>,
    /// Every layer from 0 to `total_layers` is covered exactly once.
    pub(super) complete: bool,
    /// Bytes that have to cross the network before this can serve.
    pub(super) transfer_bytes: u64,
    /// Set when the plan cannot be carried out at all, and why.
    pub(super) blocked: Option<String>,
    /// True when the node chose this, false when it is validating the
    /// operator's own selection.
    pub(super) auto: bool,
    /// True when the operator asked each machine's adjacent slices to be
    /// joined into one file — the plan reflects it in its steps.
    pub(super) merge: bool,
    /// Every range anyone holds, so the screen can offer the ones left out.
    pub(super) available: Vec<Candidate>,
    /// Every machine, so the screen can offer them as targets.
    pub(super) machines: Vec<MachineView>,
    /// Layer ranges the selection does not cover.
    pub(super) missing: Vec<[u32; 2]>,
    /// Layer ranges the selection covers more than once. Never empty together
    /// with `blocked: None`.
    pub(super) overlap: Vec<[u32; 2]>,
    /// Things worth reading that do not stop the deploy.
    pub(super) warnings: Vec<String>,
    /// The execution settings this deploy would launch with, defaults filled
    /// in. Echoed rather than left to the screen so the form starts from the
    /// node's own numbers instead of a second copy of them in TypeScript.
    pub(super) params: RuntimeParams,
    /// Prompt tokens one request will be allowed, under `params`. This is the
    /// number that decides whether a harness fits — see `gateway::prompt_budget`.
    pub(super) prompt_tokens: u32,
    /// Prompt tokens per round trip through the pipeline, under `params`.
    pub(super) chunk_tokens: u32,
    /// Candidate routes between the chosen hops. The latency may be null
    /// when the gateway has not measured the peer yet.
    pub(super) route_options: Vec<RouteOption>,
    pub(super) load_distribution: Vec<LoadDistribution>,
    /// Vision projector for the head and MTP draft for the tail: the
    /// compatible files in the network, the choice and how each one gets to
    /// its machine. Filled by `attach_sidecars`.
    pub(super) sidecars: SidecarsPlan,
    /// Every file this deploy would copy between machines, slices and
    /// sidecars alike (`planned_copies`). A deploy that copies anything needs
    /// the operator's explicit `allow_transfers`: a file transfer is an act of
    /// the operator, never a side effect of pressing Deploy.
    pub(super) copies: Vec<PlannedCopy>,
}

/// One file that crosses the network before the deploy can start.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PlannedCopy {
    /// `slice`, `mmproj` or `mtp`.
    pub(super) kind: &'static str,
    pub(super) name: String,
    pub(super) size_bytes: u64,
    /// The layer range, for a slice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) layers: Option<[u32; 2]>,
    pub(super) source_node_id: String,
    pub(super) source_host: String,
    pub(super) target_node_id: String,
    pub(super) target_host: String,
}

/// The copies `lifecycle::run` would make for this plan, in the same order
/// and with the same skips: a part already on its target, or one cut out on
/// the machine that loads it, crosses nothing.
pub(super) fn planned_copies(plan: &Plan) -> Vec<PlannedCopy> {
    let mut out = Vec::new();
    let mut slice = |target_node_id: &str,
                     target_host: &str,
                     ready: bool,
                     source_node_id: &Option<String>,
                     source_host: &Option<String>,
                     name: &str,
                     size_bytes: u64,
                     start: u32,
                     end: u32| {
        let Some(source) = source_node_id else { return };
        if ready || source == target_node_id {
            return;
        }
        out.push(PlannedCopy {
            kind: "slice",
            name: name.to_string(),
            size_bytes,
            layers: Some([start, end]),
            source_node_id: source.clone(),
            source_host: source_host.clone().unwrap_or_else(|| source.clone()),
            target_node_id: target_node_id.to_string(),
            target_host: target_host.to_string(),
        });
    };
    if !plan.tp_targets.is_empty() {
        for t in &plan.tp_targets {
            slice(&t.target_node_id, &t.target_host, t.ready, &t.source_node_id, &t.source_host, &t.name, t.size_bytes, t.start_layer, t.end_layer);
        }
    } else {
        for step in &plan.steps {
            if step.merged {
                for part in &step.parts {
                    slice(&step.target_node_id, &step.target_host, part.ready, &part.source_node_id, &part.source_host, &part.name, part.size_bytes, part.start_layer, part.end_layer);
                }
            } else {
                slice(&step.target_node_id, &step.target_host, step.ready, &step.source_node_id, &step.source_host, &step.name, step.size_bytes, step.start_layer, step.end_layer);
            }
        }
    }
    for slot in [&plan.sidecars.vision, &plan.sidecars.mtp]
        .into_iter()
        .flatten()
        .chain(&plan.sidecars.vision_heads)
    {
        let (Some(file), Some(source)) = (&slot.file, &slot.source_node_id) else {
            continue;
        };
        if !slot.needs_copy() || *source == slot.target_node_id {
            continue;
        }
        out.push(PlannedCopy {
            kind: match slot.kind {
                crate::catalog::SidecarKind::Mmproj => "mmproj",
                crate::catalog::SidecarKind::Mtp => "mtp",
            },
            name: file.file_name.clone(),
            size_bytes: file.size_bytes,
            layers: None,
            source_node_id: source.clone(),
            source_host: slot.source_host.clone().unwrap_or_else(|| source.clone()),
            target_node_id: slot.target_node_id.clone(),
            target_host: slot.target_host.clone(),
        });
    }
    out
}

/// Why a plan that would copy files cannot go on. Deployments no longer copy
/// weights between machines: each machine serves from its own copy of the
/// model, downloaded there or placed by hand (`/api/transfer/*` stays only so
/// an older admin can still deploy to this node, and is deprecated). The
/// reason names each machine and what it lacks, `None` when nothing would be
/// copied.
pub(crate) fn copies_refusal(copies: &[PlannedCopy]) -> Option<String> {
    if copies.is_empty() {
        return None;
    }
    let mut lacking: Vec<String> = Vec::new();
    for copy in copies {
        let what = match (copy.kind, copy.layers) {
            ("slice", Some([start, end])) => format!("{} (layers {start}-{end})", copy.name),
            ("mmproj", _) => format!("the vision projector {}", copy.name),
            ("mtp", _) => format!("the MTP draft {}", copy.name),
            _ => copy.name.clone(),
        };
        let line = format!("{} has no {what}", copy.target_host);
        if !lacking.contains(&line) {
            lacking.push(line);
        }
    }
    Some(format!(
        "{}. Files are not copied between machines: download the model there, or place the file in one of its model folders and rescan",
        lacking.join("; ")
    ))
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteOption {
    pub(super) from_node_id: String,
    pub(super) to_node_id: String,
    pub(super) rtt_ms: Option<f64>,
    pub(super) selected: bool,
    pub(super) note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoadDistribution {
    pub(super) node_id: String,
    pub(super) current: Option<u32>,
    pub(super) projected: Option<u32>,
    pub(super) selected: bool,
}

#[derive(Debug, Clone)]
pub(super) struct RequestedModelIdentity {
    pub(super) proof: Option<crate::deployment::DeploymentIdentity>,
    /// Paths only count as proof on the peer that announced them. They exist
    /// even when the hash could not be computed, for the approved reservation.
    pub(super) paths_by_peer: HashMap<String, String>,
}

pub(super) fn deployment_identity_matches(
    desired: &RequestedModelIdentity,
    current: &crate::deployment::DeploymentModel,
    peer_id: &str,
    current_path: Option<&str>,
) -> bool {
    let known_current = current.identity.clone().or_else(|| {
        current
            .pom_id
            .clone()
            .map(|pom_id| crate::deployment::DeploymentIdentity::PomId { pom_id })
    });
    if desired.proof.is_some() && desired.proof == known_current {
        return true;
    }
    desired
        .paths_by_peer
        .get(peer_id)
        .zip(current_path)
        .is_some_and(|(desired_path, current_path)| desired_path == current_path)
}

pub(super) fn node_is_idle(snapshot: &serde_json::Value) -> bool {
    snapshot
        .get("config")
        .is_some_and(|config| config.is_null())
        && snapshot
            .pointer("/status/state")
            .and_then(|state| state.as_str())
            == Some("disabled")
        && snapshot
            .get("active_deployment")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .is_empty()
        && snapshot
            .get("active_model")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .is_empty()
}

pub(super) fn node_state_detail(snapshot: &serde_json::Value) -> String {
    format!(
        "state={}, model={}, deployment={}",
        snapshot
            .pointer("/status/state")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown"),
        snapshot
            .get("active_model")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("none"),
        snapshot
            .get("active_deployment")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("none"),
    )
}

pub(super) const DEPLOY_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const DEPLOY_IDLE_POLL: Duration = Duration::from_millis(250);
/// Reloading a model takes minutes; a `redeploy` waits for each machine to come back to
/// serving before touching the next. The same ceiling as the HTTP call for
/// reconfiguration.
pub(super) const REDEPLOY_READY_TIMEOUT: Duration = Duration::from_secs(1800);

/// Every distinct range anyone holds, in layer order.
pub(super) fn candidates(machines: &[Machine]) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for m in machines {
        for s in &m.held {
            match out
                .iter_mut()
                .find(|c| c.start_layer == s.start_layer && c.end_layer == s.end_layer)
            {
                Some(c) if !c.holders.contains(&m.node_id) => c.holders.push(m.node_id.clone()),
                Some(_) => {}
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
pub(super) enum ModelSize {
    /// A slice said so, in `pom.original_block_count`.
    Declared(u32),
    /// No slice said so, but the ranges reach this far from layer 0, which is
    /// the same reading `catalog::build_shard_sets` makes for one machine.
    Inferred(u32),
    /// Nothing to read and nothing to deduce: no slice declares a total and
    /// none of them starts at layer 0.
    Unknown,
}

impl ModelSize {
    pub(super) fn value(self) -> Option<u32> {
        match self {
            ModelSize::Declared(n) | ModelSize::Inferred(n) => Some(n),
            ModelSize::Unknown => None,
        }
    }
    pub(super) fn inferred(self) -> bool {
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
pub(super) fn model_size(candidates: &[Candidate]) -> ModelSize {
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
pub(super) fn nameless(candidates: &[Candidate]) -> String {
    const SHOWN: usize = 4;
    let missing: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.total_layers.is_none())
        .collect();
    let mut out = missing
        .iter()
        .take(SHOWN)
        .map(|c| format!("L{}-{} in {}", c.start_layer, c.end_layer, c.name))
        .collect::<Vec<_>>()
        .join(", ");
    if missing.len() > SHOWN {
        out.push_str(&format!(" and {} more", missing.len() - SHOWN));
    }
    out
}

/// The chain the node would pick on its own: start at layer 0 and always take
/// the longest range that begins where the last one ended.
///
/// Longest-first is not an aesthetic choice. The chain with the fewest slices
/// is the one most likely to fit the cluster, and taking ranges only where the
/// previous one ended makes overlap impossible by
/// construction — which matters the moment a model has been sliced twice with
/// different cuts. When nothing continues the chain the sweep jumps to the next
/// range that starts later, which leaves a hole and says so.
pub(super) fn auto_chain(candidates: &[Candidate], total: Option<u32>) -> Vec<usize> {
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

/// Complete mode favors the finest existing cut so more eligible peers can
/// participate. It never invents a new boundary and still refuses overlap in
/// the common coverage pass below.
pub(super) fn auto_chain_complete(candidates: &[Candidate], total: Option<u32>) -> Vec<usize> {
    let total = total.unwrap_or(0);
    let mut chain = Vec::new();
    let mut next = 0u32;
    loop {
        let pick = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.start_layer == next)
            .min_by_key(|(_, c)| (c.end_layer, std::cmp::Reverse(c.holders.len())))
            .map(|(i, _)| i)
            .or_else(|| {
                candidates
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.start_layer > next)
                    .min_by_key(|(_, c)| (c.start_layer, c.end_layer))
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
/// A machine that already holds a slice is preferred because a copy is costly,
/// but a peer without that file is still a valid target when its disk and
/// combined GPU/RAM capacity can receive it. Complete mode then breaks ties by
/// the measured RTT, current in-flight load and available capacity. This keeps
/// the planner data-driven: node ids are only values from the live survey.
pub(super) fn target_capacity_mb(machine: &Machine) -> u64 {
    machine
        .mem_free_mb
        .max(machine.gpu_free_mb.saturating_add(machine.ram_free_mb))
}

pub(super) fn target_capacity_class(candidate: &Candidate, machine: &Machine) -> u8 {
    if machine
        .holds_ready(candidate.start_layer, candidate.end_layer)
        .is_some()
    {
        return 0;
    }
    let need = needed_mb(candidate.size_bytes);
    let capacity = target_capacity_mb(machine);
    if capacity == 0 {
        return 3;
    }
    if capacity >= need {
        if machine.gpu_free_mb >= need {
            1
        } else {
            2
        }
    } else {
        4
    }
}

pub(super) fn auto_assign(
    chain: &[usize],
    candidates: &[Candidate],
    machines: &[Machine],
    strategy: DeployStrategy,
    preferred_node_ids: &[String],
) -> Vec<String> {
    let mut taken: Vec<String> = Vec::new();
    let mut targets = vec![String::new(); chain.len()];

    for (slot, &ci) in chain.iter().enumerate() {
        let c = &candidates[ci];
        let mut possible: Vec<&Machine> = machines.iter().collect();
        possible.sort_by_key(|m| {
            (
                taken.contains(&m.node_id),
                target_capacity_class(c, m),
                !c.holders.contains(&m.node_id),
                !preferred_node_ids.iter().any(|id| id == &m.node_id),
                if strategy == DeployStrategy::Complete {
                    m.rtt_ms
                        .map(|rtt| (rtt.max(0.0) * 1000.0) as u64)
                        .unwrap_or(u64::MAX)
                } else {
                    0
                },
                if strategy == DeployStrategy::Complete {
                    if m.load_known {
                        m.load
                    } else {
                        u32::MAX
                    }
                } else {
                    0
                },
                std::cmp::Reverse(m.mem_free_mb),
            )
        });
        if let Some(m) = possible.first() {
            if !taken.contains(&m.node_id) {
                taken.push(m.node_id.clone());
            }
            targets[slot] = m.node_id.clone();
        }
    }

    targets
}

/// Roughly what the weights need once loaded, in MB, with the same headroom the
/// slicing planner reserves.
pub(super) fn needed_mb(size_bytes: u64) -> u64 {
    size_bytes / MIB
}

pub(super) fn usable_mb(free_mb: u64) -> u64 {
    (free_mb as f64 * (1.0 - DEFAULT_RESERVE_PCT / 100.0)) as u64
}

/// Decide who serves what — or check what the operator decided.
#[cfg(test)]
pub(super) fn plan(
    group: &str,
    machines: &[Machine],
    selection: Option<&[SelectedSlice]>,
    params: &RuntimeParams,
    merge: bool,
    kind: DeployKind,
    tp: Option<TpLaunch>,
) -> Plan {
    plan_with_options(
        group,
        machines,
        selection,
        params,
        merge,
        kind,
        tp,
        DeployStrategy::Simple,
        group,
        &[],
    )
}

#[cfg(test)]
pub(super) fn plan_with_options(
    group: &str,
    machines: &[Machine],
    selection: Option<&[SelectedSlice]>,
    params: &RuntimeParams,
    merge: bool,
    kind: DeployKind,
    tp: Option<TpLaunch>,
    strategy: DeployStrategy,
    deployment_name: &str,
    preferred_node_ids: &[String],
) -> Plan {
    plan_with_rtt_options(
        group,
        machines,
        selection,
        params,
        merge,
        kind,
        tp,
        strategy,
        deployment_name,
        preferred_node_ids,
        &PairwiseRtt::default(),
        None,
        None,
    )
}

pub(super) fn combine_known(values: impl Iterator<Item = Option<bool>>) -> Option<bool> {
    let mut found = false;
    for value in values {
        found = true;
        if value == Some(false) {
            return Some(false);
        }
        if value.is_none() {
            return None;
        }
    }
    found.then_some(true)
}

pub(super) fn selected_model_capability(
    steps: &[Step],
    machines: &[Machine],
    select: impl Fn(&crate::gguf::ModelCapabilityEvidence) -> Option<bool>,
) -> Option<bool> {
    let mut values = Vec::new();
    for step in steps {
        let ranges = if step.parts.is_empty() {
            vec![(step.start_layer, step.end_layer)]
        } else {
            step.parts
                .iter()
                .map(|part| (part.start_layer, part.end_layer))
                .collect()
        };
        for (start_layer, end_layer) in ranges {
            let evidence = machines
                .iter()
                .find(|machine| machine.node_id == step.target_node_id)
                .and_then(|machine| machine.holds(start_layer, end_layer))
                .or_else(|| {
                    machines
                        .iter()
                        .find_map(|machine| machine.holds(start_layer, end_layer))
                })
                .and_then(|shard| shard.model_capabilities.as_ref())
                .and_then(&select);
            values.push(evidence);
        }
    }
    combine_known(values.into_iter())
}

pub(super) fn selected_final_model_capability(
    steps: &[Step],
    machines: &[Machine],
    select: impl Fn(&crate::gguf::ModelCapabilityEvidence) -> Option<bool>,
) -> Option<bool> {
    let step = steps.last()?;
    let (start_layer, end_layer) = step
        .parts
        .last()
        .map(|part| (part.start_layer, part.end_layer))
        .unwrap_or((step.start_layer, step.end_layer));
    machines
        .iter()
        .find(|machine| machine.node_id == step.target_node_id)
        .and_then(|machine| machine.holds(start_layer, end_layer))
        .or_else(|| {
            machines
                .iter()
                .find_map(|machine| machine.holds(start_layer, end_layer))
        })
        .and_then(|shard| shard.model_capabilities.as_ref())
        .and_then(select)
}

pub(super) fn selected_runtime_capability(
    steps: &[Step],
    machines: &[Machine],
    capability: &str,
) -> Option<bool> {
    combine_known(steps.iter().map(|step| {
        machines
            .iter()
            .find(|machine| machine.node_id == step.target_node_id)
            .and_then(|machine| machine.inference_capabilities.as_ref())
            .map(|caps| caps.iter().any(|candidate| candidate == capability))
    }))
}

pub(super) fn selected_runtime_capability_for_step(
    step: &Step,
    machines: &[Machine],
    capability: &str,
) -> Option<bool> {
    machines
        .iter()
        .find(|machine| machine.node_id == step.target_node_id)
        .and_then(|machine| machine.inference_capabilities.as_ref())
        .map(|caps| caps.iter().any(|candidate| candidate == capability))
}

pub(super) fn runtime_capability_for_ui(
    value: Option<bool>,
    source: &str,
    supported_reason: &str,
    unsupported_reason: &str,
    unknown_reason: &str,
) -> crate::capabilities::Capability {
    match value {
        Some(true) => crate::capabilities::supported(source, supported_reason),
        Some(false) => {
            let mut capability = crate::capabilities::supported(source, unsupported_reason);
            capability.warning = Some(format!(
                "Limitation of the runtime: {unsupported_reason}; the option stays actionable and may not be executed."
            ));
            capability
        }
        None => {
            let mut capability = crate::capabilities::unknown(source, unknown_reason);
            capability.warning = Some(
                "The runtime did not confirm this capability; turning it on is at the user's own risk."
                    .to_string(),
            );
            capability
        }
    }
}

pub(super) fn capability_from_bool(
    value: Option<bool>,
    source: &str,
    supported_reason: &str,
    unsupported_reason: &str,
    unknown_reason: &str,
) -> crate::capabilities::Capability {
    match value {
        Some(true) => crate::capabilities::supported(source, supported_reason),
        Some(false) => crate::capabilities::unsupported(source, unsupported_reason),
        None => crate::capabilities::unknown(source, unknown_reason),
    }
}

pub(super) fn no_embeddings_topology(
    steps: &[Step],
    total_layers: Option<u32>,
    kind: DeployKind,
) -> crate::capabilities::NoEmbeddingsEvaluation {
    let mut ranges = Vec::new();
    if kind == DeployKind::Shards || (kind == DeployKind::Whole && steps.len() > 1) {
        for step in steps {
            let range = (step.start_layer, step.end_layer);
            if !ranges.contains(&range) {
                ranges.push(range);
            }
        }
    } else if !steps.is_empty() {
        ranges.push((steps[0].start_layer, steps[0].end_layer));
    }
    let count = if ranges.is_empty() {
        None
    } else {
        Some(ranges.len() as u32)
    };
    let peers = steps
        .iter()
        .map(|step| {
            let index = ranges
                .iter()
                .position(|range| *range == (step.start_layer, step.end_layer))
                .map(|value| value as u32);
            crate::capabilities::PeerTopology {
                node_id: step.target_node_id.clone(),
                start_layer: Some(step.start_layer),
                end_layer: Some(step.end_layer),
                total_layers,
                peer_index: index,
                peer_count: count,
            }
        })
        .collect::<Vec<_>>();
    crate::capabilities::evaluate_no_embeddings(&peers)
}

pub(super) fn capabilities_for_plan(
    steps: &[Step],
    machines: &[Machine],
    total_layers: Option<u32>,
    kind: DeployKind,
) -> (
    crate::capabilities::DeploymentCapabilities,
    crate::capabilities::NoEmbeddingsEvaluation,
) {
    let no_embeddings = no_embeddings_topology(steps, total_layers, kind);
    let no_thinking = capability_from_bool(
        selected_model_capability(steps, machines, |evidence| evidence.thinking),
        "GGUF tokenizer.chat_template + tokenizer.ggml.tokens",
        "the model template and vocabulary allow thinking control",
        "the model template and vocabulary do not offer thinking control",
        "the model template or vocabulary could not be confirmed",
    );
    let lookup = selected_runtime_capability(steps, machines, "speculate/v1-lookup");
    let speculative_decoding = runtime_capability_for_ui(
        lookup,
        "runtime capabilities of the selected peers",
        "all the selected peers announce speculative lookup",
        "at least one selected peer does not announce speculative lookup",
        "the runtime capabilities of at least one peer could not be read",
    );
    let hidden_state_f8 = capability_from_bool(
        selected_runtime_capability(steps, machines, crate::pipeline::CAP_FRAME_DTYPE_F8),
        "runtime capabilities of the selected peers",
        "all the selected peers announce f8 hidden state",
        "at least one selected peer does not announce f8 hidden state",
        "the runtime capabilities of at least one peer could not be read",
    );
    let hidden_state_i4g64 = capability_from_bool(
        selected_runtime_capability(steps, machines, crate::pipeline::CAP_FRAME_DTYPE_I4G64),
        "runtime capabilities of the selected peers",
        "all the selected peers announce i4g64 hidden state",
        "at least one selected peer does not announce i4g64 hidden state",
        "the runtime capabilities of at least one peer could not be read",
    );
    let hidden_state_i4g128 = capability_from_bool(
        selected_runtime_capability(steps, machines, crate::pipeline::CAP_FRAME_DTYPE_I4G128),
        "runtime capabilities of the selected peers",
        "all the selected peers announce i4g128 hidden state",
        "at least one selected peer does not announce i4g128 hidden state",
        "the runtime capabilities of at least one peer could not be read",
    );
    let model_mtp = selected_final_model_capability(steps, machines, |evidence| evidence.mtp);
    let runtime_mtp = combine_known(
        [
            lookup,
            selected_runtime_capability(steps, machines, "speculate/v1-mtp-sliced"),
            steps.last().and_then(|step| {
                selected_runtime_capability_for_step(step, machines, "speculate/v1-mtp")
            }),
        ]
        .into_iter(),
    );
    let mtp = match model_mtp {
        Some(false) => crate::capabilities::unsupported(
            "selected GGUF file",
            "the selected GGUF carries no proven nextn prediction",
        ),
        Some(true) => {
            let mut capability = crate::capabilities::supported(
                "selected GGUF file",
                "the selected GGUF carries proven nextn metadata and tensors",
            );
            match runtime_mtp {
                Some(true) => {}
                Some(false) => {
                    capability.warning = Some(
                        "Limitation of the runtime: one or more peers do not announce the required MTP protocol; the option stays actionable and may not be executed."
                            .to_string(),
                    );
                }
                None => {
                    capability.warning = Some(
                        "The peers' runtime did not confirm the MTP protocol; turning it on is at the user's own risk."
                            .to_string(),
                    );
                }
            }
            capability
        }
        None => {
            let mut capability = crate::capabilities::unknown(
                "selected GGUF file",
                "nextn metadata or tensors of the selected GGUF could not be confirmed",
            );
            if runtime_mtp.is_some() {
                capability.warning = Some(
                    "The runtime was queried, but it does not replace the proof from the model; turning it on is at the user's own risk."
                        .to_string(),
                );
            }
            capability
        }
    };
    let mut no_embeddings = no_embeddings.capability.clone();
    let unsafe_peers = no_embeddings_topology(steps, total_layers, kind)
        .peers
        .into_iter()
        .filter(|peer| peer.safe == Some(false))
        .map(|peer| format!("{}: {}", peer.node_id, peer.reason))
        .collect::<Vec<_>>();
    let unknown_peers = no_embeddings_topology(steps, total_layers, kind)
        .peers
        .into_iter()
        .filter(|peer| peer.safe.is_none())
        .map(|peer| format!("{}: {}", peer.node_id, peer.reason))
        .collect::<Vec<_>>();
    if !unsafe_peers.is_empty() {
        no_embeddings.reason = format!(
            "control visible, but these peers must keep embeddings: {}",
            unsafe_peers.join("; ")
        );
    } else if !unknown_peers.is_empty() {
        no_embeddings.reason = format!(
            "control visible, but the topology could not be confirmed on: {}",
            unknown_peers.join("; ")
        );
    }
    (
        crate::capabilities::DeploymentCapabilities {
            fit_memory: if kind == DeployKind::TensorParallel {
                crate::capabilities::unsupported(
                    "Tensor Parallelism strategy",
                    "the automatic memory fit is not compatible with Tensor Parallelism",
                )
            } else {
                crate::capabilities::supported(
                    "plan strategy",
                    "this strategy allows the automatic memory fit",
                )
            },
            no_thinking,
            no_embeddings,
            speculative_decoding,
            mtp,
            hidden_state_f8,
            hidden_state_i4g64,
            hidden_state_i4g128,
        },
        no_embeddings_topology(steps, total_layers, kind),
    )
}

pub(super) fn requested_capability_block(
    params: &RuntimeParams,
    capabilities: &crate::capabilities::DeploymentCapabilities,
    no_embeddings: &crate::capabilities::NoEmbeddingsEvaluation,
) -> Option<String> {
    crate::capabilities::validate_no_embeddings(no_embeddings, params.no_embeddings)
        .err()
        .or_else(|| {
            let requested = match params.hidden_state_dtype {
                crate::wire::WireDtype::F8 => Some(("f8", &capabilities.hidden_state_f8)),
                crate::wire::WireDtype::I4G64 => {
                    Some(("i4g64", &capabilities.hidden_state_i4g64))
                }
                crate::wire::WireDtype::I4G128 => {
                    Some(("i4g128", &capabilities.hidden_state_i4g128))
                }
                crate::wire::WireDtype::F32 | crate::wire::WireDtype::F16 => None,
            };
            if let Some((dtype, capability)) = requested {
                match capability.state {
                    crate::capabilities::CapabilityState::Supported => None,
                    crate::capabilities::CapabilityState::Unsupported => Some(format!(
                        "Hidden state precision {dtype} was requested, but {}",
                        capability.reason
                    )),
                    crate::capabilities::CapabilityState::Unknown => Some(format!(
                        "Hidden state precision {dtype} was requested, but the capability of the peers could not be confirmed: {}",
                        capability.reason
                    )),
                }
            } else {
                None
            }
        })
        .or_else(|| {
            if params.fit_memory
                && capabilities.fit_memory.state == crate::capabilities::CapabilityState::Unknown
            {
                Some(format!(
                    "Fit to memory cannot be confirmed: {}",
                    capabilities.fit_memory.reason
                ))
            } else {
                None
            }
        })
}

/// File name of a projector, to compare copies of the same file on machines
/// with different folders.
fn projector_file_name(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

/// The head's path for the projector the screen asked for: the same path, or
/// the only candidate with the same file name. `None` when the head does not
/// have that projector (or has more than one with the same name, which cannot
/// be disambiguated).
pub(super) fn resolve_head_projector(requested: &str, candidates: &[String]) -> Option<String> {
    if candidates.iter().any(|candidate| candidate == requested) {
        return Some(requested.to_string());
    }
    let name = projector_file_name(requested);
    let mut same_name = candidates
        .iter()
        .filter(|candidate| projector_file_name(candidate) == name);
    match (same_name.next(), same_name.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    }
}

pub(super) fn plan_with_rtt_options(
    group: &str,
    machines: &[Machine],
    selection: Option<&[SelectedSlice]>,
    params: &RuntimeParams,
    merge: bool,
    kind: DeployKind,
    tp: Option<TpLaunch>,
    strategy: DeployStrategy,
    deployment_name: &str,
    preferred_node_ids: &[String],
    pairwise_rtt: &PairwiseRtt,
    layer_options: Option<WholeLayerOptions>,
    requested_mmproj_path: Option<&str>,
) -> Plan {
    let mut available = candidates(machines);
    let size = model_size(&available);
    let total_layers = size.value();
    let tp_pre_sliced = kind == DeployKind::TensorParallel && has_tp_parts(machines);
    let is_moe = machines
        .iter()
        .flat_map(|machine| &machine.held)
        .any(|shard| shard.is_moe);
    let inference_mode_reason = if params.inference_mode == InferenceMode::Moe && !is_moe {
        Some("MoE requires real expert metadata in the GGUF of this model.".to_string())
    } else if kind == DeployKind::TensorParallel && is_moe {
        Some("Tensor Parallelism does not support MoE in this version.".to_string())
    } else {
        None
    };
    let inference_mode_supported = inference_mode_reason.is_none();

    // What we are planning: (index into `available`, target node id).
    let auto = selection.is_none();
    // (index into `available`, target node id, that machine's overrides)
    let mut chosen: Vec<(usize, String, PeerOverrides)> = Vec::new();
    let mut blocked: Option<String> = None;
    let mut warnings: Vec<String> = Vec::new();

    if let Some(reason) = &inference_mode_reason {
        blocked = Some(reason.clone());
    }

    // A sliced deployment lists the whole GGUF of the model too (survey), for a prefill machine the operator places
    // by hand; the automatic plan chains the slices only.
    let whole_only = |c: &Candidate| {
        kind == DeployKind::Shards
            && machines.iter().all(|m| {
                m.holds(c.start_layer, c.end_layer)
                    .is_none_or(|shard| shard.kind == HeldKind::Whole)
            })
    };
    match selection {
        None => {
            let pool: Vec<usize> = (0..available.len())
                .filter(|&i| !whole_only(&available[i]))
                .collect();
            let slices: Vec<Candidate> = pool.iter().map(|&i| available[i].clone()).collect();
            let chain: Vec<usize> = match strategy {
                DeployStrategy::Simple => auto_chain(&slices, total_layers),
                DeployStrategy::Complete => auto_chain_complete(&slices, total_layers),
            }
            .into_iter()
            .map(|i| pool[i])
            .collect();
            let targets = auto_assign(&chain, &available, machines, strategy, preferred_node_ids);
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
                        "{} exists on no machine of the cluster",
                        kind.unit(group, s.start_layer, s.end_layer)
                    ));
                    continue;
                };
                if chosen.iter().any(|(j, target, ..)| {
                    let existing = &available[*j];
                    existing.start_layer == s.start_layer
                        && existing.end_layer == s.end_layer
                        && target == &s.target_node_id
                }) {
                    // Repeating the exact same row is an idempotent UI action.
                    // It must not turn a valid replica on another peer into a
                    // refusal, and it must not create two launch steps on one
                    // peer for the same file.
                    continue;
                }
                chosen.push((
                    i,
                    s.target_node_id.clone(),
                    PeerOverrides {
                        n_gpu_layers: s.n_gpu_layers,
                        offload_kqv: s.offload_kqv,
                        inference_role: s.inference_role,
                    },
                ));
            }
            chosen.sort_by_key(|(i, ..)| (available[*i].start_layer, available[*i].end_layer));
        }
    }

    // ── Tensor parallelism: the primary is who the operator chose ────────────
    //
    // The whole model is a single candidate; its target is the primary (rank 0), not
    // the machine the automatic planner would suggest. The group checks
    // (size, workers in the cluster, whole model present) refuse the plan
    // before the shardability check, which reads the GGUF and runs on the endpoint
    // `/api/ui/tp/check`.
    if kind == DeployKind::TensorParallel {
        if let Some(tp) = &tp {
            if let Some(reason) = tp_group_checks(tp, machines, group) {
                blocked.get_or_insert(reason);
            }
            // The screen's selection may contain the whole model once for the
            // primary and once more for a worker. A worker is a rank of the collective,
            // not an additional pipeline stage. We keep only the primary's row,
            // preserving its overrides, and `launch_targets` creates the
            // workers exactly once below.
            if let Some(primary_index) = chosen
                .iter()
                .position(|(_, target, _)| target == &tp.primary_node_id)
            {
                let (candidate, _, overrides) = chosen.remove(primary_index);
                chosen.clear();
                chosen.push((candidate, tp.primary_node_id.clone(), overrides));
            } else if let Some((candidate, _, overrides)) = chosen.first().cloned() {
                chosen.clear();
                chosen.push((candidate, tp.primary_node_id.clone(), overrides));
            }
        }
    }

    // Vision belongs to the selected pipeline head, not to whichever peer
    // happened to report a layer-0 file first. Downstream peers receive hidden
    // state and must never be asked to load the raw image projector.
    let selected_head_projector = chosen
        .iter()
        .find(|(index, target, _)| !target.is_empty() && available[*index].start_layer == 0)
        .and_then(|(index, target, _)| {
            let candidate = &available[*index];
            machines
                .iter()
                .find(|machine| machine.node_id == *target)
                .and_then(|machine| {
                    machine.held.iter().find(|shard| {
                        shard.start_layer == candidate.start_layer
                            && shard.end_layer == candidate.end_layer
                            && shard.name == candidate.name
                    })
                })
        });
    let mut mmproj_candidates = selected_head_projector
        .map(|shard| shard.mmproj_candidates.clone())
        .unwrap_or_default();
    if let Some(path) = selected_head_projector.and_then(|shard| shard.mmproj_path.clone()) {
        if !mmproj_candidates.iter().any(|candidate| candidate == &path) {
            mmproj_candidates.push(path);
        }
    }
    mmproj_candidates.sort();
    mmproj_candidates.dedup();
    // The projector the screen asked for is resolved against the **head's**
    // copy: each machine loads its own file, and the chosen path may have come
    // from another machine in the catalog. The same path is accepted;
    // otherwise, the same file name among the head's candidates. With no known
    // candidates, the path goes through as is and is validated on the head.
    let requested_mmproj_path = requested_mmproj_path.filter(|path| !path.is_empty());
    let resolved_request = requested_mmproj_path.map(|requested| {
        if mmproj_candidates.is_empty() {
            Some(requested.to_string())
        } else {
            resolve_head_projector(requested, &mmproj_candidates)
        }
    });
    let mmproj_path = match &resolved_request {
        Some(resolved) => resolved.clone(),
        None => selected_head_projector.and_then(|shard| shard.mmproj_path.clone()),
    };
    let mmproj_reason = selected_head_projector
        .and_then(|shard| shard.mmproj_reason.clone())
        .or_else(|| {
            mmproj_path.is_none().then(|| {
                "the catalog did not prove a projector belonging to the selected model".into()
            })
        });
    if let (Some(requested), Some(None)) = (requested_mmproj_path, &resolved_request) {
        blocked = Some(format!(
            "the chosen projector ({}) does not exist on the machine that processes \
             the prompt; pick one of its projectors",
            projector_file_name(requested)
        ));
    }

    for (i, ..) in &chosen {
        available[*i].selected = true;
    }

    if available.is_empty() {
        blocked.get_or_insert(if kind.whole() {
            // Both readings of an empty answer, because from here they are the
            // same fact: the catalog skips a whole model whose header did not
            // yield a layer count, and a model nobody has never appears either.
            format!(
                "no machine of the cluster has the whole model {group} with a readable layer count in the GGUF"
            )
        } else {
            format!("no machine of the cluster has slices of {group}")
        });
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
                "no slice of {group} states how many layers the model has ({}), and none of them starts at layer 0 so that the total could be deduced. Reslice the whole model from the Slice screen, which writes this in every slice, or bring the slice that starts at layer 0 into the cluster.",
                nameless(&available)
            ));
        }
        ModelSize::Inferred(n) => warnings.push(format!(
            "the total number of layers is not written in the slices ({}), so it was deduced from their coverage: {n} layers. If the original model has more, the missing layers are on no machine and the answer comes out wrong. Reslice from the Slice screen to write the total.",
            nameless(&available)
        )),
        _ => {}
    }

    if !available.is_empty() && chosen.is_empty() {
        blocked.get_or_insert(
            if kind.whole() {
                "no model selected"
            } else {
                "no slice selected"
            }
            .to_string(),
        );
    }

    // ── Prefill machines of a sliced deployment ─────────────────────────────
    //
    // A machine with the prefill role runs the whole model beside the slices: it reads prompts for the pipeline and
    // answers requests on its own, so it stands outside their layer order, and the coverage below counts the slices.
    let prefill_entry = |overrides: &PeerOverrides| {
        kind == DeployKind::Shards
            && overrides.inference_role.unwrap_or(params.inference_role)
                == crate::webui::InferenceRole::Prefill
    };
    for (i, target, overrides) in &chosen {
        let c = &available[*i];
        let whole = c.start_layer == 0 && total_layers.is_some_and(|total| c.end_layer + 1 == total);
        if prefill_entry(overrides) && !whole {
            let on = if target.is_empty() { String::new() } else { format!(" on {target}") };
            blocked.get_or_insert(format!(
                "a prefill machine runs the whole model: {}{on} is a slice; choose the whole model for it, or give \
                 it the prefill and decode role",
                kind.unit(group, c.start_layer, c.end_layer)
            ));
        }
    }
    if kind == DeployKind::Shards
        && chosen.iter().any(|(_, _, overrides)| prefill_entry(overrides))
        && params.inference_service.is_none()
    {
        blocked.get_or_insert(
            "a prefill machine needs an inference service that splits prefill and decode".to_string(),
        );
    }

    // ── Coverage: gaps and overlap refuse ───────────────────────────────────
    let mut missing: Vec<[u32; 2]> = Vec::new();
    let mut overlap: Vec<[u32; 2]> = Vec::new();
    let mut next = 0u32;
    let mut covered_exact: Option<(u32, u32)> = None;
    for (i, _, overrides) in &chosen {
        if prefill_entry(overrides) {
            continue;
        }
        let c = &available[*i];
        if covered_exact == Some((c.start_layer, c.end_layer)) {
            continue;
        }
        covered_exact = Some((c.start_layer, c.end_layer));
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
            "layers L{}-{} are in two chosen slices at the same time",
            o[0], o[1]
        ));
    }
    for m in &missing {
        let message = if m[0] == m[1] {
            format!("layer L{} is in no chosen slice", m[0])
        } else {
            format!("layers L{}-{} are in no chosen slice", m[0], m[1])
        };
        blocked.get_or_insert(message.clone());
        warnings.push(message);
    }

    // ── Preserve the selected pipeline steps, optionally merging neighbours ─
    //
    // A node may serve several selected ranges. `merge` says that adjacent
    // ranges on the same machine should become one step that joins them into a
    // single file on that machine. Separated ranges remain independent steps,
    // because contiguity is not a validity rule for the global pipeline.
    //
    // "Sem máquina" is not a machine. Each unplaced range keeps its own group
    // and its own line, so it is named separately in the plan.
    let mut groups: Vec<Vec<(usize, String, PeerOverrides)>> = Vec::new();
    for entry in &chosen {
        let can_merge_with_previous = merge
            && !entry.1.is_empty()
            && groups.last().is_some_and(|group| {
                group.last().is_some_and(|previous| {
                    previous.1 == entry.1
                        && available[entry.0].start_layer
                            == available[previous.0].end_layer.saturating_add(1)
                })
            });
        if can_merge_with_previous {
            groups
                .last_mut()
                .expect("previous group")
                .push(entry.clone());
        } else {
            groups.push(vec![entry.clone()]);
        }
    }
    // `chosen` is already in layer order and groups are formed in that order,
    // so the pipeline order is preserved without regrouping by machine.

    let mut transfer_bytes = 0u64;
    let mut steps: Vec<Step> = Vec::new();

    for grp in groups {
        let target_id = &grp[0].1;
        let target = machines.iter().find(|m| &m.node_id == target_id);
        let first = &available[grp[0].0];
        let last = &available[grp[grp.len() - 1].0];
        let start_layer = first.start_layer;
        let end_layer = last.end_layer;

        // Groups contain only adjacent ranges for the merge path. A separated
        // range has its own group and remains an ordinary pipeline step.
        let merged = merge && grp.len() > 1;

        if merged {
            let mut parts: Vec<StepPart> = Vec::new();
            for (i, _, _) in &grp {
                let c = &available[*i];
                let part_ready =
                    target.is_some_and(|t| t.holds_ready(c.start_layer, c.end_layer).is_some());
                let source = if part_ready {
                    None
                } else {
                    machines.iter().find(|m| {
                        (Some(&m.node_id) != target.map(|t| &t.node_id)
                            || m.holds(c.start_layer, c.end_layer)
                                .is_some_and(|shard| shard.virtual_slice))
                            && m.holds(c.start_layer, c.end_layer).is_some()
                    })
                };
                if !part_ready
                    && target.is_some()
                    && source.is_none_or(|source| source.node_id != *target_id)
                {
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
                    source_sha256: source
                        .and_then(|m| m.holds(c.start_layer, c.end_layer))
                        .and_then(|s| s.sha256.clone()),
                    source_needs_split: source.is_some_and(|m| {
                        m.holds(c.start_layer, c.end_layer)
                            .is_some_and(|shard| shard.virtual_slice)
                    }),
                });
            }

            let size_bytes = parts.iter().map(|p| p.size_bytes).sum();
            let ready = parts.iter().all(|p| p.ready);

            let mut problem = None;
            let mut note = None;

            if target_id.is_empty() {
                problem = Some(format!(
                    "range L{}-{} has no machine",
                    start_layer, end_layer
                ));
            } else if target.is_none() {
                problem = Some(format!("machine {target_id} is not in the cluster"));
            } else if !ready {
                if let Some(orphan) = parts
                    .iter()
                    .find(|p| !p.ready && p.source_node_id.is_none())
                {
                    problem = Some(format!(
                        "slice L{}-{} exists on no machine of the cluster",
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
                        "{} has {} GB free on disk and merging these slices needs {} GB",
                        t.label(),
                        gb(t.disk_free_mb),
                        gb(disk_need)
                    ));
                } else if target_capacity_mb(t) > 0 && mem_need > usable_mb(target_capacity_mb(t)) {
                    note = Some(format!(
                        "{} has {} GB of free memory for the merged model of {} GB",
                        t.label(),
                        gb(t.mem_free_mb),
                        gb(mem_need)
                    ));
                } else if !ready {
                    note = Some(format!(
                        "this machine does not have this file ({} GB): files are not copied between machines",
                        gb(mem_need)
                    ));
                }
            }

            if let Some(p) = &problem {
                blocked.get_or_insert(p.clone());
            }
            if let Some(n) = &note {
                if n.contains("memory") {
                    warnings.push(n.clone());
                }
            }

            steps.push(Step {
                // Numbered after the sort below: the place in the pipeline is
                // the place in the layer order, not the order the grouping
                // happened to walk.
                position: 0,
                start_layer,
                end_layer,
                total_layers,
                // What the operator reads. The real identity is the hash of the
                // part, which travels in `part_sha256`; a "qwen3-8b L0-15" here
                // is a screen label, not the key of anything.
                name: format!("{group} L{start_layer}-{end_layer}"),
                size_bytes,
                target_node_id: target_id.clone(),
                target_host: target.map(|t| t.label()).unwrap_or_default(),
                ready,
                source_node_id: None,
                source_host: None,
                source_path: None,
                source_sha256: None,
                source_needs_split: false,
                from_whole_path: None,
                target_path: None,
                primary: start_layer == 0,
                n_gpu_layers: grp[0].2.n_gpu_layers,
                offload_kqv: grp[0].2.offload_kqv,
                inference_role: grp[0].2.inference_role,
                options: merged_alternatives(&parts, machines),
                problem,
                note,
                merged: true,
                parts,
                // The join has not happened yet - there is no local file to
                // query (see `enrich_capacity_previews`).
                capacity: None,
            });
        } else {
            // Plain steps: one range, one file. A separated range has its own
            // group and remains an ordinary step in the pipeline.
            for (i, target_id, over) in &grp {
                let c = &available[*i];
                let target = machines.iter().find(|m| &m.node_id == target_id);
                let ready =
                    target.is_some_and(|t| t.holds_ready(c.start_layer, c.end_layer).is_some());
                let source = if ready {
                    None
                } else {
                    machines.iter().find(|m| {
                        (Some(&m.node_id) != target.map(|t| &t.node_id)
                            || m.holds(c.start_layer, c.end_layer)
                                .is_some_and(|shard| shard.virtual_slice))
                            && m.holds(c.start_layer, c.end_layer).is_some()
                    })
                };
                if !tp_pre_sliced
                    && !ready
                    && target.is_some()
                    && source.is_none_or(|source| source.node_id != *target_id)
                {
                    transfer_bytes += c.size_bytes;
                }

                let mut problem = None;
                let mut note = None;

                let unit = kind.unit(group, c.start_layer, c.end_layer);
                if target_id.is_empty() {
                    problem = Some(format!("{unit} has no machine"));
                } else if target.is_none() {
                    problem = Some(format!("machine {target_id} is not in the cluster"));
                } else if !ready && source.is_none() {
                    problem = Some(format!("{unit} exists on no machine of the cluster"));
                }

                // A target that points at its range of its own whole model
                // writes no slice (`CAP_RANGE_LOAD`).
                let from_whole = source
                    .filter(|m| m.node_id == *target_id)
                    .and_then(|m| m.holds(c.start_layer, c.end_layer))
                    .is_some_and(|shard| shard.virtual_slice);
                let writes_nothing = from_whole
                    && target.is_some_and(|t| {
                        t.inference_capabilities.as_ref().is_some_and(|caps| {
                            caps.iter().any(|cap| cap == crate::pipeline::CAP_RANGE_LOAD)
                        })
                    });
                if let (None, Some(t)) = (&problem, target) {
                    let need = needed_mb(c.size_bytes);
                    if !ready && !writes_nothing && t.disk_free_mb > 0 && need > t.disk_free_mb {
                        problem = Some(format!(
                            "{} has {} GB free on disk and {unit} weighs {} GB",
                            t.label(),
                            gb(t.disk_free_mb),
                            gb(need)
                        ));
                    } else if target_capacity_mb(t) > 0 && need > usable_mb(target_capacity_mb(t)) {
                        note = Some(format!(
                            "{} has {} GB of free memory for {} of {} GB",
                            t.label(),
                            gb(t.mem_free_mb),
                            kind.indefinite(),
                            gb(need)
                        ));
                    } else if !ready && !from_whole {
                        // Files are not copied between machines any more: this
                        // plan is blocked (`copies_refusal`). The note says what
                        // the machine lacks and how it gets it.
                        note = Some(if kind.whole() {
                            format!(
                                "this machine does not have the model ({} GB): download it here, or place the file in one of its model folders and rescan",
                                gb(need)
                            )
                        } else {
                            format!(
                                "this machine does not have this slice ({} GB): files are not copied between machines",
                                gb(need)
                            )
                        });
                    }
                }

                // Nothing travels, but a target without range loading still
                // writes the range as a real slice: say so before the deploy.
                if problem.is_none() && note.is_none() && from_whole && !writes_nothing {
                    note = Some(format!(
                        "{} cuts this range from its own copy: {} GB written to disk (it does not announce {})",
                        target.map(|t| t.label()).unwrap_or_default(),
                        gb(needed_mb(c.size_bytes)),
                        crate::pipeline::CAP_RANGE_LOAD
                    ));
                }
                if let Some(p) = &problem {
                    blocked.get_or_insert(p.clone());
                }
                if let Some(n) = &note {
                    if n.contains("memory") {
                        warnings.push(n.clone());
                    }
                }

                steps.push(Step {
                    position: 0,
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
                    source_sha256: source
                        .and_then(|m| m.holds(c.start_layer, c.end_layer))
                        .and_then(|s| s.sha256.clone()),
                    source_needs_split: source.is_some_and(|m| {
                        m.holds(c.start_layer, c.end_layer)
                            .is_some_and(|shard| shard.virtual_slice)
                    }),
                    from_whole_path: source
                        .filter(|m| m.node_id == *target_id)
                        .and_then(|m| m.holds(c.start_layer, c.end_layer))
                        .filter(|shard| shard.virtual_slice)
                        .map(|shard| shard.path.clone()),
                    target_path: target
                        .and_then(|t| t.holds_ready(c.start_layer, c.end_layer))
                        .map(|s| s.path.clone()),
                    primary: c.start_layer == 0,
                    n_gpu_layers: over.n_gpu_layers,
                    offload_kqv: over.offload_kqv,
                    inference_role: over.inference_role,
                    options: alternatives(c, machines, kind),
                    problem,
                    note,
                    merged: false,
                    parts: Vec::new(),
                    capacity: None,
                });
            }
        }
    }

    // ── The order the answer comes back in ──────────────────────────────────
    //
    // The pipeline order *is* the layer order, so `position` is only honest in
    // layer order, and `run` reads the launch order straight off this list.
    // Groups are formed in layer order, and this final sort keeps the contract
    // explicit even if a future grouping change walks a different collection.
    steps.sort_by_key(|s| (s.start_layer, s.end_layer));
    for (i, s) in steps.iter_mut().enumerate() {
        s.position = i as u32 + 1;
    }

    let mut tp_targets = Vec::new();
    if tp_pre_sliced {
        if let Some(tp) = &tp {
            let overrides = steps
                .first()
                .map(|step| (step.n_gpu_layers, step.offload_kqv));
            let (resolved, bytes, reason) = tp_targets_for(
                tp,
                machines,
                group,
                total_layers,
                overrides.and_then(|value| value.0),
                overrides.and_then(|value| value.1),
            );
            tp_targets = resolved;
            transfer_bytes = transfer_bytes.saturating_add(bytes);
            if let Some(reason) = reason {
                blocked.get_or_insert(reason);
            }
            if let (Some(step), Some(rank0)) = (
                steps.iter_mut().find(|step| step.primary),
                tp_targets.first(),
            ) {
                step.ready = rank0.ready;
                step.source_node_id = rank0.source_node_id.clone();
                step.source_host = rank0.source_host.clone();
                step.source_path = rank0.source_path.clone();
                step.source_sha256 = rank0.source_sha256.clone();
                step.target_path = rank0.target_path.clone();
                step.size_bytes = rank0.size_bytes;
                step.problem = rank0.problem.clone();
            }
        }
    }

    // The auto planner runs out of machines before it runs out of slices, and
    // that reads better as a count than as one message per missing range.
    if auto && steps.iter().any(|s| s.target_node_id.is_empty()) {
        blocked = Some(format!(
            "there are {} slices for {} {} - some slices were left without a machine",
            steps.len(),
            machines.len(),
            if machines.len() == 1 {
                "machine"
            } else {
                "machines"
            }
        ));
    }

    let machine_views = machines
        .iter()
        .map(|m| MachineView {
            node_id: m.node_id.clone(),
            label: m.label(),
            mem_free_mb: m.mem_free_mb,
            gpu_free_mb: m.gpu_free_mb,
            ram_free_mb: m.ram_free_mb,
            disk_free_mb: m.disk_free_mb,
            rtt_ms: m.rtt_ms,
            load: m.load_known.then_some(m.load),
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
            // Filled in later, asynchronously, by
            // `enrich_capacity_previews` - this planner is synchronous and does not
            // talk to the network.
            capacity: None,
        })
        .collect();

    let selected_ids: std::collections::HashSet<&str> = steps
        .iter()
        .map(|step| step.target_node_id.as_str())
        .collect();
    let mut projected: std::collections::HashMap<&str, Option<u32>> = machines
        .iter()
        .map(|m| (m.node_id.as_str(), m.load_known.then_some(m.load)))
        .collect();
    for step in &steps {
        if let Some(value) = projected.get_mut(step.target_node_id.as_str()) {
            if let Some(load) = value {
                *load = load.saturating_add(1);
            }
        }
    }
    let load_distribution = machines
        .iter()
        .map(|machine| LoadDistribution {
            node_id: machine.node_id.clone(),
            current: machine.load_known.then_some(machine.load),
            projected: projected
                .get(machine.node_id.as_str())
                .cloned()
                .unwrap_or_else(|| machine.load_known.then_some(machine.load)),
            selected: selected_ids.contains(machine.node_id.as_str()),
        })
        .collect();

    // Build edges between consecutive logical layer stages, not between every
    // pair of machines in the plan. Two exact copies of L0-14 are one stage
    // with two source choices; treating them as L0-14 -> L0-14 would invent a
    // backward pipeline hop. A route option is only useful when its target is
    // one of the selected machines serving the immediately next stage. A peer
    // that merely has an inference endpoint, or serves a later stage, is not a
    // bridge for this handoff.
    let mut stage_ranges: Vec<(u32, u32)> = Vec::new();
    for step in &steps {
        let range = (step.start_layer, step.end_layer);
        if !stage_ranges.contains(&range) {
            stage_ranges.push(range);
        }
    }
    let (capabilities, no_embeddings_evaluation) =
        capabilities_for_plan(&steps, machines, total_layers, kind);
    if let Some(reason) =
        requested_capability_block(params, &capabilities, &no_embeddings_evaluation)
    {
        if blocked.is_none() {
            blocked = Some(reason);
        } else {
            warnings.push(reason);
        }
    }

    let mut route_options = Vec::new();
    for pair in stage_ranges.windows(2) {
        let from_range = pair[0];
        let to_range = pair[1];
        let from_nodes: Vec<String> = steps
            .iter()
            .filter(|step| {
                (step.start_layer, step.end_layer) == from_range && !step.target_node_id.is_empty()
            })
            .map(|step| step.target_node_id.clone())
            .collect();
        let to_nodes: Vec<String> = steps
            .iter()
            .filter(|step| {
                (step.start_layer, step.end_layer) == to_range && !step.target_node_id.is_empty()
            })
            .map(|step| step.target_node_id.clone())
            .collect();
        let selected_from = from_nodes.first();
        let selected_to = to_nodes.first();

        for from in &from_nodes {
            for to in &to_nodes {
                if from == to {
                    continue;
                }
                let Some(machine) = machines.iter().find(|machine| &machine.node_id == to) else {
                    continue;
                };
                if machine.inference_url.is_none() {
                    continue;
                }
                let selected = selected_from == Some(from) && selected_to == Some(to);
                let rtt_ms = pairwise_rtt.get(from, to);
                route_options.push(RouteOption {
                    from_node_id: from.clone(),
                    to_node_id: to.clone(),
                    rtt_ms,
                    selected,
                    note: if selected {
                        "chosen route".to_string()
                    } else if rtt_ms.is_some() {
                        "alternative route measured on the source peer".to_string()
                    } else {
                        "alternative route without a directional RTT measurement".to_string()
                    },
                });
            }
        }
    }

    Plan {
        sidecars: SidecarsPlan::default(),
        group: group.to_string(),
        deployment_name: deployment_name.to_string(),
        strategy,
        kind,
        mmproj_path,
        mmproj_candidates,
        mmproj_reason,
        tp,
        is_moe,
        inference_mode_supported,
        inference_mode_reason,
        capabilities,
        layer_options,
        total_layers,
        total_layers_inferred: size.inferred(),
        complete,
        transfer_bytes,
        steps,
        tp_targets,
        copies: Vec::new(),
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
        route_options,
        load_distribution,
    }
}

pub(super) fn gb(mb: u64) -> String {
    format!("{:.1}", mb as f64 / 1024.0).replace('.', ",")
}

/// Every machine as a possible target for one range, with the reason.
///
/// Occupancy is deliberately *not* part of `ok`: a machine already holding
/// another range is exactly the one an operator wants to swap with, and hiding
/// it would make the swap impossible. Picking a busy machine is caught by the
/// one-range-per-node rule instead, which is the honest place for it.
pub(super) fn alternatives(
    c: &Candidate,
    machines: &[Machine],
    kind: DeployKind,
) -> Vec<Alternative> {
    let need = needed_mb(c.size_bytes);
    machines
        .iter()
        .map(|m| {
            let has = m.holds_ready(c.start_layer, c.end_layer).is_some();
            let no_disk = !has && m.disk_free_mb > 0 && need > m.disk_free_mb;
            let tight = target_capacity_mb(m) > 0 && need > usable_mb(target_capacity_mb(m));
            Alternative {
                node_id: m.node_id.clone(),
                label: m.label(),
                has,
                ok: !no_disk,
                note: if no_disk {
                    Some(format!("only {} GB free on disk", gb(m.disk_free_mb)))
                } else if tight {
                    Some(format!("{} GB of free memory", gb(m.mem_free_mb)))
                } else if has {
                    Some(
                        if kind.whole() {
                            "already has the model"
                        } else {
                            "already has the slice"
                        }
                        .to_string(),
                    )
                } else {
                    Some(format!("would receive {} GB", gb(need)))
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
pub(super) fn merged_alternatives(parts: &[StepPart], machines: &[Machine]) -> Vec<Alternative> {
    let need = needed_mb(parts.iter().map(|p| p.size_bytes).sum());
    machines
        .iter()
        .map(|m| {
            let has = parts
                .iter()
                .all(|p| m.holds_ready(p.start_layer, p.end_layer).is_some());
            let no_disk = !has && m.disk_free_mb > 0 && need > m.disk_free_mb;
            let tight = target_capacity_mb(m) > 0 && need > usable_mb(target_capacity_mb(m));
            Alternative {
                node_id: m.node_id.clone(),
                label: m.label(),
                has,
                ok: !no_disk,
                note: if no_disk {
                    Some(format!("only {} GB free on disk", gb(m.disk_free_mb)))
                } else if tight {
                    Some(format!("{} GB of free memory", gb(m.mem_free_mb)))
                } else if has {
                    Some("already has all the slices".to_string())
                } else {
                    Some(format!("would receive {} GB", gb(need)))
                },
            }
        })
        .collect()
}

/// Remote shardability check: asks the primary (the owner of the whole
/// model) whether its GGUF can be split into `tp_size` ranks, without writing anything.
///
/// `Some(reason)` = the plan must be refused. The check fails closed: if the
/// primary does not answer (or is an old binary without the route), the plan is
/// refused with an honest reason, instead of letting the job fail halfway through the
/// apply.
pub(super) async fn tp_shardability_check(
    state: &GatewayState,
    machines: &[Machine],
    tp: &TpLaunch,
) -> Option<String> {
    let primary = machines.iter().find(|m| m.node_id == tp.primary_node_id)?;
    let Some(path) = primary.held.first().map(|s| s.path.clone()) else {
        return Some(format!(
            "the primary {} does not have the whole model",
            primary.label()
        ));
    };
    let Some(primary_peer) = state
        .all_peers()
        .await
        .into_iter()
        .find(|peer| peer.node_id == primary.node_id)
    else {
        return Some(format!("could not reach the primary {}", primary.label()));
    };

    // `/api/ui/tp/check` is authenticated like every operator `/api/ui/*`: without the
    // key, the primary answers 401 and the check never passes - on no
    // machine, because this does not depend on who the primary is. `for_peer` resolves
    // only the TLS (client pinned by node_id); the credential is per request, as
    // in the rest of the file.
    let body = json!({ "model_path": path, "tp_size": tp.size });
    let resp = match state
        .control_post_json(&primary_peer, "/api/ui/tp/check", &body, PEER_TIMEOUT * 4)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Some(format!(
                "could not validate the shard on {}: {e:#}",
                primary.label()
            ))
        }
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => {
            return Some(format!(
                "{} answered the shard check badly",
                primary.label()
            ))
        }
    };
    tp_check_verdict(&body, &primary.label())
}

/// How the response of `/api/ui/tp/check` is read.
///
/// Separated from the call to be testable without a live peer - and because the branch without
/// `ok` has lied once already. `/api/ui/tp/check` is an authenticated route
/// (`webui::tp_check` receives `ConnectionAuth`), and a peer that refuses returns
/// `{"error":"missing Authorization header"}`: valid JSON, without `ok`. Calling
/// that "old binary" sent the operator to check the binary version on
/// both machines while the real defect was the credential that
/// `tp_shardability_check` did not even send (17/08, with Mac and ksia on the same commit).
/// Whoever refuses usually says why; repeating their reason is always better than
/// guessing ours.
pub(super) fn tp_check_verdict(body: &serde_json::Value, label: &str) -> Option<String> {
    match body.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => None,
        Some(false) => Some(
            body.get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("the model is not shardable on path C")
                .to_string(),
        ),
        None => Some(match body.get("error").and_then(|v| v.as_str()) {
            Some(err) => format!("{label} refused the shard check: {err}"),
            None => format!("{label} does not have the shard check (old binary?)"),
        }),
    }
}

/// A whole model on several machines: each machine left at `Both` (its own role, else the deployment's) runs as
/// `Shared`, which keeps its plugin's sessions on, so the machines can hand conversations to each other and take a
/// prompt a prefill machine read. Nothing to do for one machine or for slices.
pub(super) fn share_whole_model_roles(
    kind: DeployKind,
    steps: &mut [Step],
    default: crate::webui::InferenceRole,
) {
    let mut machines: Vec<&str> = steps.iter().map(|step| step.target_node_id.as_str()).collect();
    machines.sort();
    machines.dedup();
    if kind != DeployKind::Whole || machines.len() < 2 {
        return;
    }
    for step in steps {
        step.inference_role = Some(step.inference_role.unwrap_or(default).shared());
    }
}
