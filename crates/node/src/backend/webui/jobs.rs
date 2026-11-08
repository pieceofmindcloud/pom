use super::*;
// ── Applying a configuration ─────────────────────────────────────────────────

/// Which inference engine the operator asks the runtime to use for a model.
/// `Dense` is the default and `Auto` remains an explicit choice guided by the
/// metadata; every variant is persisted with the deployment so that all peers
/// receive the same choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceMode {
    /// llama.cpp is the product's safe default. Auto remains available when
    /// the metadata-guided selection is requested explicitly.
    #[default]
    Dense,
    Auto,
    Moe,
}

/// What a whole-model machine of a plugin inference service does besides answering its own requests
/// (`RuntimeParams::inference_role`). Every role answers requests and reads its own prompts. `Prefill` ("prefill
/// first") also reads, while it is free, the long prompts of the other machines and hands them over as a session
/// (`plugin_engine`, prefill/decode split). `Shared` is not chosen by the operator: the deploy gives it to a `Both`
/// machine that runs the whole model beside other machines, so its plugin keeps the sessions on and it can take a
/// prompt a prefill machine read. `Both` alone keeps the sessions off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceRole {
    #[default]
    Both,
    Prefill,
    Shared,
}

impl InferenceRole {
    pub fn is_both(&self) -> bool {
        *self == Self::Both
    }

    /// The role a machine runs with when the deployment puts the whole model on several machines: `Both` becomes
    /// `Shared`, which keeps the sessions on so the machines can hand conversations to each other.
    pub fn shared(self) -> Self {
        if self == Self::Both {
            Self::Shared
        } else {
            self
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Prefill => "prefill",
            Self::Shared => "shared",
        }
    }
}

/// Sampling and context knobs. Defaults match `run-p1.sh`, which is the
/// configuration the two-peer cluster was validated with.
///
/// `PartialEq` because the deployment record keeps one copy per node
/// (`deployment::DeploymentNode::config`) and comparing two records is how one
/// checks that a redeploy changed something.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RuntimeParams {
    /// Engine preference. `Dense` selects llama.cpp by default; `Auto`
    /// lets an explicit request choose by the real GGUF metadata.
    #[serde(default)]
    pub inference_mode: InferenceMode,
    /// Who runs the model. `None` is the POM's own engine (llama.cpp, the
    /// distributed pipeline). A plugin code (`"strata"`) is a whole-model
    /// inference service that plugin offers: the target machine asks the
    /// plugin to load the model instead of starting llama.cpp, and the
    /// gateway sends it the rendered prompt. See `crate::plugin_engine`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_service: Option<String>,
    /// The settings of that service's engine, by the keys it publishes
    /// (`PluginService::settings`). Passed to `engine.load` as they are; the
    /// plugin validates them. Empty for llama.cpp.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub engine_settings: std::collections::BTreeMap<String, serde_json::Value>,
    /// What this machine does when a plugin inference service runs the whole
    /// model here: read prompts only, answer only, or both (the default). See
    /// `InferenceRole`.
    #[serde(default, skip_serializing_if = "InferenceRole::is_both")]
    pub inference_role: InferenceRole,
    /// The prefill/decode split's threshold: a turn goes through the prefill machine only when the part of its prompt
    /// the decode machine does not already hold has at least this many tokens. `None` takes the engine's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefill_min_tokens: Option<u32>,
    #[serde(default = "d_gpu_layers")]
    pub n_gpu_layers: u32,
    #[serde(default = "d_n_ctx")]
    pub n_ctx: u32,
    #[serde(default = "d_true")]
    pub offload_kqv: bool,
    /// Hidden-state precision at the boundaries between peers. f16 remains the
    /// default; f32 preserves the activations at twice the payload. f8 and i4
    /// are explicit lossy choices.
    #[serde(default)]
    pub hidden_state_dtype: crate::wire::WireDtype,
    /// Lets llama.cpp's `common_fit_params` fit the allocation to free
    /// memory. In TP the runtime turns this option off, because the native fit
    /// does not support `split-mode tensor`.
    #[serde(default = "d_true")]
    pub fit_memory: bool,
    /// VRAM that `fit` leaves free on each device to absorb allocator
    /// variation, late workspaces and processes that show up afterwards.
    #[serde(default = "d_fit_memory_margin_mb")]
    pub fit_memory_margin_mb: u32,
    #[serde(default = "d_n_batch")]
    pub n_batch: u32,
    /// Physical micro-batch (llama.cpp's `n_ubatch`) while the memory fit is
    /// on; never larger than `n_batch`.
    #[serde(default = "d_n_ubatch")]
    pub n_ubatch: u32,
    #[serde(default = "d_max_new")]
    pub max_new_tokens: u32,
    /// Generations allowed in flight at once. Each generation gets the full
    /// context window; raising this value multiplies the KV-cache reservation.
    #[serde(default = "d_max_sessions")]
    pub max_sessions: u32,
    #[serde(default = "d_batching_enabled")]
    pub batching_enabled: bool,
    #[serde(default = "d_max_queue_requests")]
    pub max_queue_requests: u32,
    #[serde(default = "d_batch_window_ms")]
    pub batch_window_ms: u32,
    #[serde(default = "d_max_batch_sequences")]
    pub max_batch_sequences: u32,
    #[serde(default = "d_max_batch_tokens")]
    pub max_batch_tokens: u32,
    /// Prompt tokens per round trip through the pipeline.
    #[serde(default = "d_prefill_chunk")]
    pub prefill_chunk_tokens: u32,
    /// Maximum number of prefill chunks in flight during the overlap.
    #[serde(default = "d_prefill_max_inflight_chunks")]
    pub prefill_max_inflight_chunks: u32,
    #[serde(default = "d_temp")]
    pub temperature: f32,
    #[serde(default = "d_top_p")]
    pub top_p: f32,
    #[serde(default = "d_top_k")]
    pub top_k: u32,
    #[serde(default = "d_repeat_penalty")]
    pub repeat_penalty: f32,
    #[serde(default = "d_repeat_last_n")]
    pub repeat_last_n: u32,
    /// Subtracted once from the logit of each token seen in the window. 0 turns it off.
    #[serde(default)]
    pub presence_penalty: f32,
    /// Subtracted from the logit per occurrence of the token in the window. 0 turns it off.
    #[serde(default)]
    pub frequency_penalty: f32,
    /// Cuts candidates below this fraction of the most likely one. 0 turns it off.
    #[serde(default)]
    pub min_p: f32,
    /// Seed of the sampler's generator, applied when the deployment loads.
    /// The default is the fixed value POM has always used.
    #[serde(
        with = "llama_engine::llm::seed_text",
        default = "llama_engine::llm::seed_text::default_seed"
    )]
    pub seed: u64,
    /// llama.cpp's DRY sampler: penalizes the token that would extend a
    /// sequence already seen. `dry_multiplier` 0 turns it off.
    #[serde(default)]
    pub dry_multiplier: f32,
    #[serde(default = "d_dry_base")]
    pub dry_base: f32,
    #[serde(default = "d_dry_allowed_length")]
    pub dry_allowed_length: u32,
    #[serde(default = "d_dry_penalty_last_n")]
    pub dry_penalty_last_n: u32,
    /// Token limit of a reasoning block (llama.cpp's `--reasoning-budget`):
    /// on reaching it, the end tag is forced. -1 means no limit.
    #[serde(default = "d_reasoning_budget")]
    pub reasoning_budget: i32,
    #[serde(default = "d_true")]
    pub no_thinking: bool,
    /// Disable embedding extraction in llama.cpp context on this peer.
    /// Auto-set for the last peer of a multi-peer pipeline when true.
    #[serde(default)]
    pub no_embeddings: bool,
    /// Operator kill-switch for speculative decoding — off by default even
    /// on a cluster where every peer supports it (`speculate/v1-lookup`).
    #[serde(default)]
    pub speculative_decoding_enabled: bool,
    #[serde(default = "d_speculative_k")]
    pub speculative_k: u32,
    #[serde(default = "d_speculative_lookup_window")]
    pub speculative_lookup_window: u32,
    /// Multi-token prediction through the model's `nextn` block. A switch
    /// separate from the one above on purpose: they are different mechanisms,
    /// and only this one uses trained model weights. See `crate::mtp`. On by
    /// default: the gateway only uses it when the plan supports it
    /// (`mtp_enabled_for_plan`).
    #[serde(default = "d_true")]
    pub mtp_enabled: bool,
    #[serde(default = "d_mtp_draft_tokens")]
    pub mtp_draft_tokens: u32,
    #[serde(default = "d_mtp_min_probability")]
    pub mtp_min_probability: f32,
    /// Allows MTP to fall back to RAM when the VRAM cannot hold the context.
    #[serde(default = "d_mtp_cpu_fallback_enabled")]
    pub mtp_cpu_fallback_enabled: bool,
    /// Where the MTP block lives: `auto`, `vram` or `ram`.
    #[serde(default)]
    pub mtp_placement: llama_engine::MtpPlacement,
    /// All routed experts of a MoE stay in RAM and the CPU computes them
    /// (llama.cpp's `--cpu-moe`); the VRAM keeps the rest of the model. Skips
    /// the memory fit, which refuses overrides defined before it.
    #[serde(default)]
    pub moe_experts_in_ram: bool,
    /// GPU cache, in MiB, of the experts that stayed in RAM: the recently used
    /// ones are served from VRAM, only the misses go up over PCIe (batches of up
    /// to 32 tokens). `0` turns it off. With the memory fit on, the fit counts
    /// the cache and leaves fewer whole expert layers on the GPU.
    #[serde(default)]
    pub moe_cache_mib: u32,
    /// The gateway picks each request's prefill chunk (between 32 tokens and
    /// `prefill_chunk_tokens`) from a model of the route it learns. Off, every
    /// prompt goes in chunks of exactly `prefill_chunk_tokens`.
    #[serde(default = "d_adaptive_prefill_chunks")]
    pub adaptive_prefill_chunks: bool,
    /// Unloads the weights after a period with no requests on this peer.
    #[serde(default)]
    pub unload_if_idle: bool,
    /// Lets the GPU arbiter unload this deployment while it has no request in
    /// flight, so an image component on the same physical GPU can run; the
    /// next request reloads it (POM settings, off by default for every engine).
    #[serde(default)]
    pub yield_gpu_to_images: bool,
    /// Minutes of inactivity before releasing RAM/VRAM.
    #[serde(default = "d_idle_minutes")]
    pub idle_minutes: u32,
}

pub(super) fn d_gpu_layers() -> u32 {
    99
}
pub(super) fn d_n_ctx() -> u32 {
    8192
}
pub(super) fn d_n_batch() -> u32 {
    512
}
pub(super) fn d_max_new() -> u32 {
    256
}
pub(super) fn d_max_sessions() -> u32 {
    1
}
pub(super) fn d_batching_enabled() -> bool {
    true
}
pub(super) fn d_max_queue_requests() -> u32 {
    128
}
pub(super) fn d_batch_window_ms() -> u32 {
    0
}
pub(super) fn d_max_batch_sequences() -> u32 {
    u32::MAX
}
pub(super) fn d_max_batch_tokens() -> u32 {
    u32::MAX
}
pub(super) fn d_prefill_chunk() -> u32 {
    crate::prefill::DEFAULT_CHUNK_TOKENS
}
pub(super) fn d_prefill_max_inflight_chunks() -> u32 {
    crate::prefill::PrefillOverlapWindow::DEFAULT_MAX_INFLIGHT_CHUNKS as u32
}
pub(super) fn d_temp() -> f32 {
    0.7
}
pub(super) fn d_top_p() -> f32 {
    0.9
}
pub(super) fn d_top_k() -> u32 {
    40
}
pub(super) fn d_repeat_penalty() -> f32 {
    1.15
}
pub(super) fn d_repeat_last_n() -> u32 {
    64
}
pub(super) fn d_true() -> bool {
    true
}
pub(super) fn d_fit_memory_margin_mb() -> u32 {
    crate::runtime::DEFAULT_FIT_MEMORY_MARGIN_MB
}
pub(super) fn d_speculative_k() -> u32 {
    4
}
pub(super) fn d_speculative_lookup_window() -> u32 {
    3
}
pub(super) fn d_mtp_draft_tokens() -> u32 {
    3
}
/// The same floor the screen writes when turning MTP on (`MTP_MIN_PROBABILITY`).
pub(super) fn d_mtp_min_probability() -> f32 {
    0.75
}
pub(super) fn d_dry_base() -> f32 {
    1.75
}
pub(super) fn d_dry_allowed_length() -> u32 {
    2
}
pub(super) fn d_dry_penalty_last_n() -> u32 {
    1024
}
pub(super) fn d_n_ubatch() -> u32 {
    llama_engine::DEFAULT_N_UBATCH
}
pub(super) fn d_reasoning_budget() -> i32 {
    -1
}
pub(super) fn d_mtp_cpu_fallback_enabled() -> bool {
    true
}
pub(super) fn d_adaptive_prefill_chunks() -> bool {
    true
}
pub(super) fn d_idle_minutes() -> u32 {
    60
}

impl Default for RuntimeParams {
    fn default() -> Self {
        Self {
            inference_mode: InferenceMode::Dense,
            inference_service: None,
            engine_settings: Default::default(),
            inference_role: Default::default(),
            prefill_min_tokens: None,
            n_gpu_layers: d_gpu_layers(),
            n_ctx: d_n_ctx(),
            offload_kqv: d_true(),
            hidden_state_dtype: crate::wire::WireDtype::F16,
            fit_memory: d_true(),
            fit_memory_margin_mb: d_fit_memory_margin_mb(),
            n_batch: d_n_batch(),
            n_ubatch: d_n_ubatch(),
            max_new_tokens: d_max_new(),
            max_sessions: d_max_sessions(),
            batching_enabled: d_batching_enabled(),
            max_queue_requests: d_max_queue_requests(),
            batch_window_ms: d_batch_window_ms(),
            max_batch_sequences: d_max_batch_sequences(),
            max_batch_tokens: d_max_batch_tokens(),
            prefill_chunk_tokens: d_prefill_chunk(),
            prefill_max_inflight_chunks: d_prefill_max_inflight_chunks(),
            temperature: d_temp(),
            top_p: d_top_p(),
            top_k: d_top_k(),
            repeat_penalty: d_repeat_penalty(),
            repeat_last_n: d_repeat_last_n(),
            presence_penalty: 0.0,
            frequency_penalty: 0.0,
            min_p: 0.0,
            seed: llama_engine::llm::DEFAULT_SAMPLER_SEED,
            dry_multiplier: 0.0,
            dry_base: 1.75,
            dry_allowed_length: 2,
            dry_penalty_last_n: 1024,
            reasoning_budget: -1,
            no_thinking: d_true(),
            no_embeddings: false,
            speculative_decoding_enabled: false,
            speculative_k: d_speculative_k(),
            speculative_lookup_window: d_speculative_lookup_window(),
            mtp_enabled: d_true(),
            mtp_draft_tokens: d_mtp_draft_tokens(),
            mtp_min_probability: d_mtp_min_probability(),
            mtp_cpu_fallback_enabled: d_mtp_cpu_fallback_enabled(),
            mtp_placement: Default::default(),
            moe_experts_in_ram: false,
            moe_cache_mib: 0,
            adaptive_prefill_chunks: true,
            unload_if_idle: false,
            yield_gpu_to_images: false,
            idle_minutes: d_idle_minutes(),
        }
    }
}

/// Carry a running node's knobs over to whoever takes its slice.
///
/// A handover that quietly reset the context window or the sampling would change
/// the model's behaviour halfway through a session, which is worse than the
/// handover failing outright.
impl From<&NodeConfig> for RuntimeParams {
    fn from(c: &NodeConfig) -> Self {
        Self {
            inference_mode: c.inference_mode,
            inference_service: c.inference_service.clone(),
            engine_settings: c.engine_settings.clone(),
            inference_role: c.inference_role,
            prefill_min_tokens: c.prefill_min_tokens,
            n_gpu_layers: c.n_gpu_layers,
            n_ctx: c.n_ctx,
            offload_kqv: c.offload_kqv,
            hidden_state_dtype: c.hidden_state_dtype,
            fit_memory: c.fit_memory,
            fit_memory_margin_mb: c.fit_memory_margin_mb,
            n_batch: c.n_batch,
            n_ubatch: c.n_ubatch,
            max_new_tokens: c.max_new_tokens,
            max_sessions: c.max_sessions,
            batching_enabled: c.batching_enabled,
            max_queue_requests: c.max_queue_requests,
            batch_window_ms: c.batch_window_ms,
            max_batch_sequences: c.max_batch_sequences,
            max_batch_tokens: c.max_batch_tokens,
            prefill_chunk_tokens: c.prefill_chunk_tokens,
            prefill_max_inflight_chunks: c.prefill_max_inflight_chunks,
            temperature: c.temperature,
            top_p: c.top_p,
            top_k: c.top_k,
            repeat_penalty: c.repeat_penalty,
            repeat_last_n: c.repeat_last_n,
            presence_penalty: c.presence_penalty,
            frequency_penalty: c.frequency_penalty,
            min_p: c.min_p,
            seed: c.seed,
            dry_multiplier: c.dry_multiplier,
            dry_base: c.dry_base,
            dry_allowed_length: c.dry_allowed_length,
            dry_penalty_last_n: c.dry_penalty_last_n,
            reasoning_budget: c.reasoning_budget,
            no_thinking: c.no_thinking,
            no_embeddings: c.no_embeddings,
            speculative_decoding_enabled: c.speculative_decoding_enabled,
            speculative_k: c.speculative_k,
            speculative_lookup_window: c.speculative_lookup_window,
            mtp_enabled: c.mtp_enabled,
            mtp_draft_tokens: c.mtp_draft_tokens,
            mtp_min_probability: c.mtp_min_probability,
            mtp_cpu_fallback_enabled: c.mtp_cpu_fallback_enabled,
            mtp_placement: c.mtp_placement,
            moe_experts_in_ram: c.moe_experts_in_ram,
            moe_cache_mib: c.moe_cache_mib,
            adaptive_prefill_chunks: c.adaptive_prefill_chunks,
            unload_if_idle: c.unload_if_idle,
            yield_gpu_to_images: c.yield_gpu_to_images,
            idle_minutes: c.idle_minutes,
        }
    }
}

/// Told to a single peer: slice this range out of your local copy and serve it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApplyRequest {
    /// Full model on the receiving peer's filesystem.
    pub source_model_path: String,
    /// Shared across peers so every shard resolves to the same model label —
    /// the pipeline planner only chains peers whose label matches exactly.
    pub base_name: String,
    /// Operator-visible model identity. When supplied by deployment it must be
    /// preserved exactly instead of collapsing model families into profiles.
    #[serde(default)]
    pub model_label: Option<String>,
    /// Operational name of the deployment, separate from the model's technical
    /// identity. Empty keeps compatibility with old calls.
    #[serde(default)]
    pub deployment_name: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    pub peer_count: u32,
    pub peer_index: u32,
    #[serde(default)]
    pub primary: bool,
    /// Tensor parallelism identity of this node (path C). `None` = node without
    /// TP. When present, **every** rank extracts its own tensor shard - rank 0
    /// included. The `role` only says who hosts the collective (`rank0_url`);
    /// see `tp_shard_to_extract`.
    #[serde(default)]
    pub tp: Option<crate::network::TpIdentity>,
    /// Set when the source is already a shard (or covers the whole model) and
    /// no slicing is needed.
    #[serde(default)]
    pub skip_split: bool,
    /// The deployment this node is going to be part of. See `deployment.rs`.
    #[serde(default)]
    pub deploy_id: String,
    /// Hash of the part this node has to load, when the sender already knows
    /// which one it is - the normal case in a deployment, where the `.pom`
    /// already exists.
    ///
    /// It is what makes reuse a question about **content** instead of about a
    /// file name: either the part with this hash is on disk, or it is not, and
    /// guessing is not one of the options.
    #[serde(default)]
    pub part_sha256: Option<String>,
    /// Local slices to join into one file before serving. When present, the
    /// peer merges them and serves the union instead of slicing
    /// `source_model_path`.
    #[serde(default)]
    pub merge: Option<Vec<MergeSource>>,
    /// Load the adjacent mmproj projector for this target's model.
    #[serde(default)]
    pub use_mmproj: bool,
    /// Explicit projector path from the local catalog. Slices may be stored
    /// away from the original whole model, so adjacency is not sufficient.
    #[serde(default)]
    pub mmproj_path: Option<String>,
    /// External MTP draft to load with the model, chosen by the deployment.
    #[serde(default)]
    pub mtp_draft_path: Option<String>,
    /// The draft by name + size, resolved on this machine's disk when no path
    /// came with the request.
    #[serde(default)]
    pub mtp_draft_file: Option<crate::catalog::SidecarFile>,
    /// The projector by name + size, under the same rule.
    #[serde(default)]
    pub mmproj_file: Option<crate::catalog::SidecarFile>,
    #[serde(default)]
    pub params: RuntimeParams,
}

impl ApplyRequest {
    pub(super) fn is_first(&self) -> bool {
        self.start_layer == 0
    }
    pub(super) fn is_last(&self) -> bool {
        self.end_layer + 1 >= self.total_layers
    }
    pub(super) fn is_last_peer(&self) -> bool {
        self.peer_index + 1 >= self.peer_count
    }
    /// A single peer covering the whole model runs without hidden-state
    /// extraction, which is what `--single-peer` controls.
    pub(super) fn single_peer(&self) -> bool {
        self.peer_count <= 1 && self.is_first() && self.is_last()
    }
    /// When checked in the UI, force no_embeddings on every peer (dangerous —
    /// the first peer won't be able to extract hidden states). When unchecked,
    /// auto-detect: only the last peer in a multi-peer pipeline skips embeddings.
    pub(super) fn no_embeddings(&self) -> bool {
        if self.params.no_embeddings {
            true
        } else {
            !self.single_peer() && self.is_last_peer()
        }
    }

    pub(super) fn no_embeddings_evaluation(&self) -> crate::capabilities::NoEmbeddingsEvaluation {
        crate::capabilities::evaluate_no_embeddings(&[crate::capabilities::PeerTopology {
            node_id: self.base_name.clone(),
            start_layer: Some(self.start_layer),
            end_layer: Some(self.end_layer),
            total_layers: Some(self.total_layers),
            peer_index: Some(self.peer_index),
            peer_count: Some(self.peer_count),
        }])
    }
}

pub(super) fn model_label(req: &ApplyRequest) -> String {
    req.model_label
        .as_deref()
        .filter(|label| !label.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| crate::model::ModelProfile::guess_from_name(&req.base_name).name)
}

pub(super) fn deployment_name(req: &ApplyRequest) -> String {
    normalize_deployment_name(Some(&req.deployment_name), &req.base_name)
        .unwrap_or_else(|_| req.base_name.clone())
}
