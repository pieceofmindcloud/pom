use super::*;
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

/// What is being put into service: a set of slices, or one whole model.
///
/// Not two flows. The whole-model case is read into the same `HeldShard` /
/// `Candidate` / `Step` shapes as a slice covering every layer, so the planner,
/// the disk and memory checks, the transfer phase and `launch` are one code
/// path. This only says which files the survey looks at, and which noun the
/// refusals use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployKind {
    /// A set of slices that together cover the model. The default, and what
    /// every caller written before whole-model deploys existed sends.
    #[default]
    Shards,
    /// One un-sliced GGUF, served whole by a single machine.
    Whole,
    /// One model split across a group of machines by tensor parallelism
    /// (path C): one primary + K workers. The primary holds the whole model;
    /// each worker extracts its own tensor shard from its own local copy.
    TensorParallel,
}

/// How much of the available cluster the planner should use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployStrategy {
    /// Pick the smallest set of peers that can serve the selected model.
    #[default]
    Simple,
    /// Consider every eligible peer to maximize GPU use and spread load.
    Complete,
}

pub(super) const MAX_DEPLOYMENT_NAME_CHARS: usize = 128;

pub(crate) fn normalize_deployment_name(
    requested: Option<&str>,
    fallback: &str,
) -> Result<String, String> {
    let name = requested
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback.trim());
    if name.is_empty() {
        return Err("the deployment name cannot be empty".to_string());
    }
    if name.chars().count() > MAX_DEPLOYMENT_NAME_CHARS {
        return Err(format!(
            "the deployment name can have at most {MAX_DEPLOYMENT_NAME_CHARS} characters"
        ));
    }
    Ok(name.to_string())
}

impl DeployKind {
    pub(super) fn whole(self) -> bool {
        matches!(self, DeployKind::Whole)
    }

    /// How a refusal names the one thing being placed on a machine.
    ///
    /// A whole model has a layer range too — it is 0 to the last one — but
    /// saying "a fatia L0-31" to an operator who picked a whole model names
    /// something that is not on their screen.
    pub(super) fn unit(self, group: &str, start: u32, end: u32) -> String {
        match self {
            DeployKind::Shards => format!("slice L{start}-{end}"),
            DeployKind::Whole => format!("the model {group}"),
            DeployKind::TensorParallel => format!("the tensor-parallel group {group}"),
        }
    }

    /// The same thing, indefinite, for a sentence about its weight.
    pub(super) fn indefinite(self) -> &'static str {
        match self {
            DeployKind::Shards => "a slice",
            DeployKind::Whole => "a model",
            DeployKind::TensorParallel => "a tensor-parallel group",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DeployRequest {
    /// The shard set, named the way the catalog groups it — or, with
    /// `kind: "whole"`, the model's own name in the catalog.
    pub group: String,
    /// Operational name of the deployment. `group` remains the model's
    /// technical identity used by the catalog and by routing.
    #[serde(default)]
    pub deployment_name: Option<String>,
    #[serde(default)]
    pub strategy: DeployStrategy,
    #[serde(default)]
    pub preferred_node_ids: Vec<String>,
    /// Slices or a whole model. Absent means slices, which is what every
    /// caller written before whole-model deploys existed means.
    #[serde(default)]
    pub kind: DeployKind,
    #[serde(default)]
    pub params: RuntimeParams,
    /// The operator's explicit choice. `None` is the default path: the node
    /// decides, and the screen shows the decision.
    #[serde(default)]
    pub selection: Option<Vec<SelectedSlice>>,
    /// Prefix cut requested from a whole model proposer. The remainder stays
    /// on that proposer as the next pipeline stage.
    #[serde(default)]
    pub whole_slice: Option<WholeSliceSelection>,
    /// Join adjacent slices assigned to the same machine into a single file,
    /// so that run becomes one pipeline step. `false` keeps each selected
    /// range as its own pipeline step.
    #[serde(default)]
    pub merge: bool,
    /// Tensor parallelism group (path C). Present only with
    /// `kind: "tensor_parallel"`: the primary serves the whole model and the
    /// workers extract their own tensor shard.
    #[serde(default)]
    pub tp: Option<TpLaunch>,
    /// Start the selected model with its catalogued mmproj projector.
    #[serde(default)]
    pub vision: bool,
    /// Optional projector selected by the operator, used only with `vision`.
    /// It is resolved against the selected pipeline head by path or, when the
    /// screen sent another machine's copy, by file name
    /// (`planning::resolve_head_projector`), and validated there before launch.
    #[serde(default)]
    pub mmproj_path: Option<String>,
    /// Vision projector chosen on the screen, by file name + size. Overrides
    /// the model's saved link; `None` leaves the link (or the catalog's
    /// sibling projector) in charge.
    #[serde(default)]
    pub vision_choice: Option<SidecarChoice>,
    /// MTP draft chosen on the screen (file, embedded or none). Same rules.
    #[serde(default)]
    pub mtp_choice: Option<SidecarChoice>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WholeSliceSelection {
    pub source_node_id: String,
    pub target_node_id: String,
    pub end_layer: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WholeLayerOptions {
    pub source_node_id: String,
    pub total_layers: u32,
    pub cuts: Vec<LayerCut>,
    pub granularity: Option<u32>,
    pub rule: String,
    pub reason: Option<String>,
}

/// Specification of a tensor parallelism group for the deploy.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TpLaunch {
    /// How many ranks in the group (1 primary + size-1 workers).
    pub size: u32,
    /// node_id of the primary (rank 0). The URL of the collective (POST
    /// /v1/tp/reduce) is derived from here - from the inference port announced
    /// by the node itself -, not typed by the operator.
    pub primary_node_id: String,
    /// node_ids of the workers (rank 1..size-1), in rank order.
    pub worker_node_ids: Vec<String>,
}

pub(super) fn effective_params_for_kind(kind: DeployKind, params: &RuntimeParams) -> RuntimeParams {
    let mut effective = params.clone();
    if kind == DeployKind::TensorParallel {
        // llama.cpp's common_fit_params explicitly refuses split-mode tensor.
        // Keep the plan honest and make the runtime safety belt independent of
        // whether the caller is a current UI or an older API client.
        effective.fit_memory = false;
    }
    effective
}
