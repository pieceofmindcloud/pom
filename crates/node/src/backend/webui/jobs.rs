use super::*;
// ── Applying a configuration ─────────────────────────────────────────────────

/// Which inference engine the operator asks the runtime to use for a model.
/// `Auto` keeps the runtime's model-metadata decision; the explicit variants
/// are persisted with the deployment so every peer receives the same choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceMode {
    #[default]
    Auto,
    Dense,
    Moe,
}

/// Sampling and context knobs. Defaults match `run-p1.sh`, which is the
/// configuration the two-peer cluster was validated with.
///
/// `PartialEq` porque o registro de implantação guarda uma cópia por nó
/// (`deployment::DeploymentNode::config`) e comparar dois registros é como se
/// verifica que uma reimplantação mudou alguma coisa.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RuntimeParams {
    /// Engine preference. `Auto` is backwards-compatible and lets the runtime
    /// choose from the actual GGUF metadata.
    #[serde(default)]
    pub inference_mode: InferenceMode,
    #[serde(default = "d_gpu_layers")]
    pub n_gpu_layers: u32,
    #[serde(default = "d_n_ctx")]
    pub n_ctx: u32,
    #[serde(default = "d_true")]
    pub offload_kqv: bool,
    /// Precisão do estado oculto nas fronteiras entre peers. f16 mantém o
    /// comportamento original; f8 é uma escolha explícita da implantação.
    #[serde(default)]
    pub hidden_state_dtype: crate::wire::WireDtype,
    /// Deixa o `common_fit_params` do llama.cpp ajustar a alocação à memória
    /// livre. No TP o runtime desliga esta opção, porque o fit nativo não
    /// suporta `split-mode tensor`.
    #[serde(default = "d_true")]
    pub fit_memory: bool,
    /// VRAM que o `fit` deixa livre em cada dispositivo para absorver variação
    /// de alocador, workspaces tardios e processos que apareçam depois.
    #[serde(default = "d_fit_memory_margin_mb")]
    pub fit_memory_margin_mb: u32,
    #[serde(default = "d_n_batch")]
    pub n_batch: u32,
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
    /// Máximo de pedaços de prefill em voo durante a sobreposição.
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
    /// Detector conservador de ciclos; fica desligado por padrão e pode ser
    /// ligado sem alterar os parâmetros do sampler.
    #[serde(default = "d_cycle_detection_enabled")]
    pub cycle_detection_enabled: bool,
    #[serde(default = "d_cycle_detection_min_window_tokens")]
    pub cycle_detection_min_window_tokens: u32,
    #[serde(default = "d_cycle_detection_max_window_tokens")]
    pub cycle_detection_max_window_tokens: u32,
    #[serde(default = "d_cycle_detection_repetitions")]
    pub cycle_detection_repetitions: u32,
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
    /// Predição multi-token pelo bloco `nextn` do modelo. Interruptor
    /// separado do de cima de propósito: são mecanismos diferentes, e só
    /// este usa peso treinado do modelo. Ver `crate::mtp`.
    #[serde(default)]
    pub mtp_enabled: bool,
    #[serde(default = "d_mtp_draft_tokens")]
    pub mtp_draft_tokens: u32,
    #[serde(default = "d_mtp_min_probability")]
    pub mtp_min_probability: f32,
    /// Permite recuar o MTP para a RAM quando a VRAM não comportar o contexto.
    #[serde(default = "d_mtp_cpu_fallback_enabled")]
    pub mtp_cpu_fallback_enabled: bool,
    /// Descarrega os pesos depois de um período sem requisições neste peer.
    #[serde(default)]
    pub unload_if_idle: bool,
    /// Minutos de inatividade antes de liberar RAM/VRAM.
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
pub(super) fn d_cycle_detection_enabled() -> bool {
    false
}
pub(super) fn d_cycle_detection_min_window_tokens() -> u32 {
    crate::cycle::DEFAULT_CYCLE_MIN_WINDOW_TOKENS
}
pub(super) fn d_cycle_detection_max_window_tokens() -> u32 {
    crate::cycle::DEFAULT_CYCLE_MAX_WINDOW_TOKENS
}
pub(super) fn d_cycle_detection_repetitions() -> u32 {
    crate::cycle::DEFAULT_CYCLE_REPETITIONS
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
pub(super) fn d_mtp_min_probability() -> f32 {
    0.0
}
pub(super) fn d_mtp_cpu_fallback_enabled() -> bool {
    true
}
pub(super) fn d_idle_minutes() -> u32 {
    60
}

impl Default for RuntimeParams {
    fn default() -> Self {
        Self {
            inference_mode: InferenceMode::Auto,
            n_gpu_layers: d_gpu_layers(),
            n_ctx: d_n_ctx(),
            offload_kqv: d_true(),
            hidden_state_dtype: crate::wire::WireDtype::F16,
            fit_memory: d_true(),
            fit_memory_margin_mb: d_fit_memory_margin_mb(),
            n_batch: d_n_batch(),
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
            cycle_detection_enabled: d_cycle_detection_enabled(),
            cycle_detection_min_window_tokens: d_cycle_detection_min_window_tokens(),
            cycle_detection_max_window_tokens: d_cycle_detection_max_window_tokens(),
            cycle_detection_repetitions: d_cycle_detection_repetitions(),
            no_thinking: d_true(),
            no_embeddings: false,
            speculative_decoding_enabled: false,
            speculative_k: d_speculative_k(),
            speculative_lookup_window: d_speculative_lookup_window(),
            mtp_enabled: false,
            mtp_draft_tokens: d_mtp_draft_tokens(),
            mtp_min_probability: d_mtp_min_probability(),
            mtp_cpu_fallback_enabled: d_mtp_cpu_fallback_enabled(),
            unload_if_idle: false,
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
            n_gpu_layers: c.n_gpu_layers,
            n_ctx: c.n_ctx,
            offload_kqv: c.offload_kqv,
            hidden_state_dtype: c.hidden_state_dtype,
            fit_memory: c.fit_memory,
            fit_memory_margin_mb: c.fit_memory_margin_mb,
            n_batch: c.n_batch,
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
            cycle_detection_enabled: c.cycle_detection_enabled,
            cycle_detection_min_window_tokens: c.cycle_detection_min_window_tokens,
            cycle_detection_max_window_tokens: c.cycle_detection_max_window_tokens,
            cycle_detection_repetitions: c.cycle_detection_repetitions,
            no_thinking: c.no_thinking,
            no_embeddings: c.no_embeddings,
            speculative_decoding_enabled: c.speculative_decoding_enabled,
            speculative_k: c.speculative_k,
            speculative_lookup_window: c.speculative_lookup_window,
            mtp_enabled: c.mtp_enabled,
            mtp_draft_tokens: c.mtp_draft_tokens,
            mtp_min_probability: c.mtp_min_probability,
            mtp_cpu_fallback_enabled: c.mtp_cpu_fallback_enabled,
            unload_if_idle: c.unload_if_idle,
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
    /// Nome operacional da implantação, separado da identidade técnica do
    /// modelo. Vazio mantém compatibilidade com chamadas antigas.
    #[serde(default)]
    pub deployment_name: String,
    pub start_layer: u32,
    pub end_layer: u32,
    pub total_layers: u32,
    pub peer_count: u32,
    pub peer_index: u32,
    #[serde(default)]
    pub primary: bool,
    /// Identidade de tensor parallelism deste nó (caminho C). `None` = nó sem
    /// TP. Presente, **todo** rank extrai o próprio shard de tensor - o rank 0
    /// inclusive. O `role` só diz quem hospeda a coletiva (`rank0_url`); ver
    /// `tp_shard_to_extract`.
    #[serde(default)]
    pub tp: Option<crate::network::TpIdentity>,
    /// Set when the source is already a shard (or covers the whole model) and
    /// no slicing is needed.
    #[serde(default)]
    pub skip_split: bool,
    /// A implantação de que este nó vai fazer parte. Ver `deployment.rs`.
    #[serde(default)]
    pub deploy_id: String,
    /// Hash da parte que este nó tem que carregar, quando quem manda já sabe
    /// qual é - o caso normal numa implantação, em que o `.pom` já existe.
    ///
    /// É o que faz o reaproveitamento ser uma pergunta sobre **conteúdo** em
    /// vez de sobre nome de arquivo: ou a parte com este hash está no disco, ou
    /// ela não está, e adivinhar não é uma das opções.
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
