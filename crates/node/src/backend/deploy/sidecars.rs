use super::*;
use crate::catalog::{Sidecar, SidecarKind, SidecarLink, SidecarMode};
// ── Vision projector and MTP draft of a deployment ───────────────────────────
//
// The projector loads where the prompt is read (the pipeline head under
// llama.cpp, the last stage under a plugin's split) and an external MTP draft
// on the tail. Neither has to live next to the model: the operator links them to the
// model (`SidecarLink`, kept on the principal) or picks them on the deploy
// screen, by file name + size. Here that choice is resolved against the
// surveyed machines: the target already has the file, or it is copied from a
// machine that has it.

/// A sidecar named the way it crosses machines.
pub type SidecarRef = crate::catalog::SidecarFile;

/// One machine that holds a sidecar, and where.
#[derive(Debug, Clone, Serialize)]
pub struct SidecarHolder {
    pub(super) node_id: String,
    pub(super) hostname: String,
    pub(super) path: String,
}

/// A compatible sidecar the screen can offer, with every machine that has it.
#[derive(Debug, Clone, Serialize)]
pub struct SidecarOption {
    pub(super) kind: SidecarKind,
    pub(super) file_name: String,
    pub(super) size_bytes: u64,
    pub(super) general_name: Option<String>,
    pub(super) quant: Option<String>,
    pub(super) holders: Vec<SidecarHolder>,
}

/// How one sidecar reaches the machine that loads it.
#[derive(Debug, Clone, Serialize)]
pub struct SidecarSlot {
    pub(super) kind: SidecarKind,
    pub(super) mode: SidecarMode,
    pub(super) file: Option<SidecarRef>,
    /// The choice came from the model's saved link, not from the screen.
    pub(super) from_link: bool,
    pub(super) target_node_id: String,
    pub(super) target_host: String,
    /// The file on the target, when it already has it.
    pub(super) target_path: Option<String>,
    /// Where it is copied from, when the target does not have it.
    pub(super) source_node_id: Option<String>,
    pub(super) source_host: Option<String>,
    pub(super) source_path: Option<String>,
    pub(super) problem: Option<String>,
}

impl SidecarSlot {
    pub(super) fn needs_copy(&self) -> bool {
        self.mode == SidecarMode::File && self.target_path.is_none() && self.source_path.is_some()
    }
}

/// Everything the deploy screen needs about sidecars for this plan.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SidecarsPlan {
    /// The model label the links are keyed by.
    pub(super) model_key: Option<String>,
    /// The model GGUF carries its own `nextn` block.
    pub(super) embedded_mtp: bool,
    /// Compatible files anywhere in the network, both kinds.
    pub(super) options: Vec<SidecarOption>,
    pub(super) vision: Option<SidecarSlot>,
    /// The projector for every other head of a whole model on several machines (a prefill/decode split, where each
    /// machine answers requests and so reads their images): the same file, resolved and copied per machine.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) vision_heads: Vec<SidecarSlot>,
    pub(super) mtp: Option<SidecarSlot>,
    /// The principal could not be read; the links shown are the local copy.
    pub(super) links_stale: bool,
}

/// What the operator asked for one kind: the screen's choice, or nothing (and
/// then the saved link decides).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SidecarChoice {
    pub mode: Option<SidecarMode>,
    #[serde(default)]
    pub file: Option<SidecarRef>,
}

pub(super) fn compatible_options(
    sidecars: &HashMap<String, Vec<Sidecar>>,
    machines: &[Machine],
    shape: &ModelShape,
) -> Vec<SidecarOption> {
    let mut options: Vec<SidecarOption> = Vec::new();
    let mut node_ids: Vec<&String> = sidecars.keys().collect();
    node_ids.sort();
    for node_id in node_ids {
        let hostname = machines
            .iter()
            .find(|machine| &machine.node_id == node_id)
            .map(|machine| machine.hostname.clone())
            .unwrap_or_else(|| node_id.chars().take(8).collect());
        for sidecar in &sidecars[node_id] {
            if !sidecar.compatible_with(shape.architecture.as_deref(), shape.embedding_length) {
                continue;
            }
            let holder = SidecarHolder {
                node_id: node_id.clone(),
                hostname: hostname.clone(),
                path: sidecar.path.clone(),
            };
            match options.iter_mut().find(|option| {
                option.kind == sidecar.kind
                    && option.file_name == sidecar.file_name
                    && option.size_bytes == sidecar.size_bytes
            }) {
                Some(option) => {
                    if !option
                        .holders
                        .iter()
                        .any(|existing| existing.node_id == holder.node_id)
                    {
                        option.holders.push(holder);
                    }
                }
                None => options.push(SidecarOption {
                    kind: sidecar.kind,
                    file_name: sidecar.file_name.clone(),
                    size_bytes: sidecar.size_bytes,
                    general_name: sidecar.general_name.clone(),
                    quant: sidecar.quant.clone(),
                    holders: vec![holder],
                }),
            }
        }
    }
    options.sort_by(|a, b| {
        (a.kind, &a.file_name, a.size_bytes).cmp(&(b.kind, &b.file_name, b.size_bytes))
    });
    options
}

/// Resolve one kind for one target machine.
fn resolve_slot(
    kind: SidecarKind,
    mode: SidecarMode,
    file: Option<SidecarRef>,
    from_link: bool,
    target: &Step,
    options: &[SidecarOption],
) -> SidecarSlot {
    let mut slot = SidecarSlot {
        kind,
        mode,
        file: file.clone(),
        from_link,
        target_node_id: target.target_node_id.clone(),
        target_host: target.target_host.clone(),
        target_path: None,
        source_node_id: None,
        source_host: None,
        source_path: None,
        problem: None,
    };
    let Some(file) = file.filter(|_| mode == SidecarMode::File) else {
        return slot;
    };
    let Some(option) = options.iter().find(|option| {
        option.kind == kind
            && option.file_name == file.file_name
            && option.size_bytes == file.size_bytes
    }) else {
        slot.problem = Some(format!(
            "no machine on the network has {} ({}) compatible with this model",
            file.file_name,
            human_bytes(file.size_bytes)
        ));
        return slot;
    };
    if let Some(local) = option
        .holders
        .iter()
        .find(|holder| holder.node_id == target.target_node_id)
    {
        slot.target_path = Some(local.path.clone());
        return slot;
    }
    let source = &option.holders[0];
    slot.source_node_id = Some(source.node_id.clone());
    slot.source_host = Some(source.hostname.clone());
    slot.source_path = Some(source.path.clone());
    slot
}

fn human_bytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
}

/// The operator's choice for one kind, falling back to the saved link.
fn choice_or_link(
    choice: Option<&SidecarChoice>,
    links: &[SidecarLink],
    model_key: &str,
    kind: SidecarKind,
) -> Option<(SidecarMode, Option<SidecarRef>, bool)> {
    if let Some(mode) = choice.and_then(|choice| choice.mode) {
        return Some((mode, choice.and_then(|choice| choice.file.clone()), false));
    }
    links
        .iter()
        .find(|link| link.model_key == model_key && link.kind == kind)
        .map(|link| {
            let file =
                link.file_name
                    .clone()
                    .zip(link.size_bytes)
                    .map(|(file_name, size_bytes)| SidecarRef {
                        file_name,
                        size_bytes,
                    });
            (link.mode, file, true)
        })
}

/// Whether the step `[start, end]` of a `total`-layer model is the one that reads the prompt, and so loads the
/// vision projector: layer 0 under llama.cpp, the last stage under a plugin's inference service (a whole model is
/// both).
pub(super) fn vision_reader(start: u32, end: u32, total: u32, plugin: bool) -> bool {
    if plugin {
        end.saturating_add(1) == total
    } else {
        start == 0
    }
}

/// Fill `plan.sidecars` and fold its copies and refusals into the plan.
///
/// `vision_on_tail`: the deployment runs on a plugin's inference service, whose split serves the requests from the
/// last stage ([`vision_reader`]); the projector goes there instead of to the head.
///
/// Vision is only resolved when the operator turned it on; when no projector
/// is chosen or linked, the catalog's own sibling projector (the old path in
/// `plan_with_rtt_options`) still applies. MTP is resolved when `mtp_on` (the
/// deployment asks for MTP; the preview always shows it, since the screen keeps
/// its params until the deploy); with nothing chosen or linked the tail keeps looking next to
/// its file, and a model with embedded `nextn` needs nothing.
#[allow(clippy::too_many_arguments)]
pub(super) fn attach_sidecars(
    plan: &mut Plan,
    machines: &[Machine],
    sidecars: &HashMap<String, Vec<Sidecar>>,
    shape: Option<&ModelShape>,
    links: &[SidecarLink],
    links_stale: bool,
    vision_on: bool,
    vision: Option<&SidecarChoice>,
    vision_on_tail: bool,
    mtp_on: bool,
    mtp: Option<&SidecarChoice>,
) {
    let Some(shape) = shape else {
        plan.sidecars = SidecarsPlan {
            links_stale,
            ..SidecarsPlan::default()
        };
        return;
    };
    let options = compatible_options(sidecars, machines, shape);
    let mut out = SidecarsPlan {
        model_key: Some(shape.model_key.clone()),
        embedded_mtp: shape.embedded_mtp,
        options,
        vision: None,
        vision_heads: Vec::new(),
        mtp: None,
        links_stale,
    };
    // A model without `nextn` in its GGUF still runs MTP with an external
    // draft. The capability read from the file alone says "unsupported" and
    // hides the toggle; a compatible draft anywhere in the network says yes.
    if plan.capabilities.mtp.state == crate::capabilities::CapabilityState::Unsupported {
        if let Some(draft) = out
            .options
            .iter()
            .find(|option| option.kind == SidecarKind::Mtp)
        {
            plan.capabilities.mtp = crate::capabilities::supported(
                "sidecar",
                format!("compatible external MTP available: {}", draft.file_name),
            );
        }
    }
    let head = plan
        .steps
        .iter()
        .find(|step| step.start_layer == 0)
        .cloned();
    let tail = plan
        .total_layers
        .and_then(|total| plan.steps.iter().find(|step| step.end_layer + 1 == total))
        .cloned();

    // The projector goes where the prompt is read: llama.cpp's pipeline enters at layer 0, a plugin's staged split
    // serves from its last stage (`plugin_stage_link`).
    let reader = if vision_on_tail { tail.as_ref() } else { head.as_ref() };
    if vision_on {
        if let (Some(reader), Some((mode, file, from_link))) = (
            reader,
            choice_or_link(vision, links, &shape.model_key, SidecarKind::Mmproj),
        ) {
            if mode == SidecarMode::File {
                let slot = resolve_slot(
                    SidecarKind::Mmproj,
                    mode,
                    file,
                    from_link,
                    reader,
                    &out.options,
                );
                if let Some(path) = &slot.target_path {
                    plan.mmproj_path = Some(path.clone());
                    plan.mmproj_reason = None;
                }
                // the other machines that hold the whole model answer too: each gets the projector
                let whole = |step: &Step| plan.total_layers.is_some_and(|total| step.end_layer + 1 == total);
                if whole(reader) {
                    for other in plan.steps.iter().filter(|step| {
                        step.start_layer == 0 && whole(step) && step.target_node_id != reader.target_node_id
                    }) {
                        out.vision_heads.push(resolve_slot(
                            SidecarKind::Mmproj,
                            mode,
                            slot.file.clone(),
                            from_link,
                            other,
                            &out.options,
                        ));
                    }
                }
                out.vision = Some(slot);
            }
        }
    }

    if mtp_on {
        if let (Some(tail), Some((mode, file, from_link))) = (
            tail.as_ref(),
            choice_or_link(mtp, links, &shape.model_key, SidecarKind::Mtp),
        ) {
            out.mtp = Some(resolve_slot(
                SidecarKind::Mtp,
                mode,
                file,
                from_link,
                tail,
                &out.options,
            ));
        }
    }

    for slot in [&out.vision, &out.mtp].into_iter().flatten().chain(&out.vision_heads) {
        if let Some(problem) = &slot.problem {
            plan.blocked.get_or_insert(problem.clone());
        }
        if slot.needs_copy() {
            if let Some(file) = &slot.file {
                plan.transfer_bytes = plan.transfer_bytes.saturating_add(file.size_bytes);
                plan.warnings.push(format!(
                    "{} copies {} ({}) from {} before starting",
                    slot.target_host,
                    file.file_name,
                    human_bytes(file.size_bytes),
                    slot.source_host.as_deref().unwrap_or("another machine"),
                ));
            }
        }
    }
    plan.sidecars = out;
}

/// After the copies, the files are found again on the fresh survey by name and
/// size: that is where `transfer` put them on each target.
pub(super) fn local_sidecar_path(
    sidecars: &HashMap<String, Vec<Sidecar>>,
    node_id: &str,
    kind: SidecarKind,
    file: &SidecarRef,
) -> Option<String> {
    sidecars.get(node_id)?.iter().find_map(|sidecar| {
        (sidecar.kind == kind
            && sidecar.file_name == file.file_name
            && sidecar.size_bytes == file.size_bytes)
            .then(|| sidecar.path.clone())
    })
}
